#!/usr/bin/env python3
"""Wizard bridge: a long-lived NDJSON stdio adapter around a NexAU code agent.

Protocol (one JSON object per line, both directions):

  stdin  (TUI -> bridge):
    {"type": "prompt", "text": "<user message>"}
    {"type": "interrupt"}          # cancel the in-flight turn
    {"type": "shutdown"}           # exit cleanly

  stdout (bridge -> TUI):
    {"type": "BRIDGE_READY", "workdir": "..."}      # emitted once on boot
    {"type": "BRIDGE_ERROR", "message": "..."}      # fatal/turn error
    {"type": "TURN_COMPLETE", "result": "..."}      # after each prompt finishes
    {"type": "<NexAU EventType>", ...fields}        # streamed agent events

The agent runs with NexAU's LocalSandbox (no E2B): file/shell ops happen
directly in `workdir`. Library logging and stray prints are forced to stderr so
they can never corrupt the NDJSON stream on stdout.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import queue
import sys
from datetime import datetime
from pathlib import Path

# --- stdout hardening -------------------------------------------------------
# Dup the real stdout, then point fd 1 at stderr. Anything NexAU (or a C
# extension) writes to stdout now lands on stderr; only `emit()` reaches the
# real stdout, keeping the NDJSON stream clean.
_real_stdout = os.fdopen(os.dup(1), "w", buffering=1, encoding="utf-8")
os.dup2(2, 1)
sys.stdout = sys.stderr  # belt-and-suspenders for Python-level prints

logging.basicConfig(level=os.environ.get("WIZARD_LOG", "WARNING"), stream=sys.stderr)
log = logging.getLogger("wizard.bridge")


def emit(obj: dict) -> None:
    """Write one NDJSON record to the real stdout."""
    _real_stdout.write(json.dumps(obj, ensure_ascii=False, default=str) + "\n")
    _real_stdout.flush()


def now() -> str:
    return datetime.now().strftime("%Y-%m-%d %H:%M:%S")


# --- agent setup ------------------------------------------------------------

AGENT_DIR = (Path(__file__).resolve().parent.parent / "agent").resolve()
# The agent's tool bindings (e.g. `tools.shell_tools:run_shell_command`) are
# imported relative to the agent dir, so it must be on sys.path.
sys.path.insert(0, str(AGENT_DIR))


def workdir() -> Path:
    wd = Path(os.environ.get("SANDBOX_WORK_DIR") or os.getcwd()).resolve()
    wd.mkdir(parents=True, exist_ok=True)
    return wd


def event_to_dict(event) -> dict:
    """Normalise a NexAU event into a plain JSON-able dict with a stable
    string `type`, dropping the heavyweight provider `raw_event` blob.

    `type` is forced to the EventType *name* (e.g. "TOOL_CALL_START") so the
    Rust matcher does not depend on pydantic's enum-value serialisation."""
    try:
        data = event.model_dump(mode="json", exclude={"raw_event"})
    except Exception:
        data = {}
    t = getattr(event, "type", None)
    name = getattr(t, "name", None)  # EventType.TOOL_CALL_START -> "TOOL_CALL_START"
    data["type"] = name or (t if isinstance(t, str) else str(t))
    return data


def build_agent():
    from nexau import Agent, AgentConfig
    from nexau.archs.main_sub.execution.middleware.agent_events_middleware import (
        AgentEventsMiddleware,
    )

    cfg = AgentConfig.from_yaml(config_path=AGENT_DIR / "code_agent.yaml")
    # Thread-safe queue: the middleware callback may fire from a worker thread.
    events: "queue.Queue" = queue.Queue()
    mw = AgentEventsMiddleware(session_id="wizard", on_event=events.put)
    cfg.middlewares = list(cfg.middlewares or []) + [mw]
    agent = Agent(config=cfg)
    return agent, events


async def run_turn(agent, events: "queue.Queue", history: list, text: str) -> None:
    """Run a single agent turn, streaming events to stdout as they arrive."""
    wd = workdir()
    ctx = {
        "date": now(),
        "username": os.environ.get("USER", "user"),
        "working_directory": str(wd),
        "env_content": {
            "date": now(),
            "username": os.environ.get("USER", "user"),
            "working_directory": str(wd),
        },
    }

    # Drain any stale events from a previous (cancelled) turn.
    while not events.empty():
        try:
            events.get_nowait()
        except queue.Empty:
            break

    task = asyncio.ensure_future(
        agent.run_async(message=text, history=list(history), context=ctx)
    )

    def drain() -> None:
        while True:
            try:
                emit(event_to_dict(events.get_nowait()))
            except queue.Empty:
                return

    try:
        while not task.done():
            drain()
            await asyncio.sleep(0.01)
        drain()  # final flush
    except asyncio.CancelledError:
        task.cancel()
        raise

    result = task.result()
    text_out = result[0] if isinstance(result, tuple) else result
    text_out = "" if text_out is None else str(text_out)
    # Message-level history keeps multi-turn context across runs.
    history.append({"role": "user", "content": text})
    history.append({"role": "assistant", "content": text_out})
    emit({"type": "TURN_COMPLETE", "result": text_out})


async def stdin_lines():
    """Async generator of stripped stdin lines (blocking read off-loop)."""
    loop = asyncio.get_event_loop()
    reader = asyncio.StreamReader()
    protocol = asyncio.StreamReaderProtocol(reader)
    await loop.connect_read_pipe(lambda: protocol, sys.stdin)
    while True:
        raw = await reader.readline()
        if not raw:  # EOF
            return
        yield raw.decode("utf-8", "replace").strip()


async def main() -> None:
    try:
        agent, events = build_agent()
    except Exception as exc:  # construction failure is fatal
        log.exception("agent construction failed")
        emit({"type": "BRIDGE_ERROR", "message": f"agent construction failed: {exc}"})
        return

    history: list = []
    emit({"type": "BRIDGE_READY", "workdir": str(workdir())})

    current: asyncio.Task | None = None

    async for line in stdin_lines():
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            emit({"type": "BRIDGE_ERROR", "message": f"bad json: {line[:200]}"})
            continue

        kind = msg.get("type")
        if kind == "shutdown":
            if current and not current.done():
                current.cancel()
            break
        if kind == "interrupt":
            if current and not current.done():
                current.cancel()
            continue
        if kind == "set_api_key":
            # OAuth token refresh: swap the key and rebuild the LLM client.
            # NexAU bakes the key into the client at construction, so the
            # client must be re-initialised; agent history is untouched.
            try:
                if agent.config.llm_config is not None:
                    agent.config.llm_config.api_key = msg.get("key", "")
                agent.openai_client = agent._initialize_openai_client()
            except Exception as exc:
                emit({"type": "BRIDGE_ERROR", "message": f"set_api_key failed: {exc}"})
            continue
        if kind == "prompt":
            text = msg.get("text", "")
            if current and not current.done():
                emit({"type": "BRIDGE_ERROR", "message": "turn already in progress"})
                continue

            async def go(t=text):
                try:
                    await run_turn(agent, events, history, t)
                except asyncio.CancelledError:
                    emit({"type": "TURN_COMPLETE", "result": "", "interrupted": True})
                except Exception as exc:  # one bad turn shouldn't kill the bridge
                    log.exception("turn failed")
                    emit({"type": "RUN_ERROR", "message": str(exc)})
                    emit({"type": "TURN_COMPLETE", "result": "", "error": True})

            current = asyncio.ensure_future(go())
            continue

        emit({"type": "BRIDGE_ERROR", "message": f"unknown message type: {kind!r}"})

    if current and not current.done():
        current.cancel()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
