#!/usr/bin/env python3
"""Stand-in for `claude -p` used by tests/live.rs. Answers the handshake, streams one text
delta for a prompt, ignores interrupts, and hangs the first launch so the backend has to
restart it. Launches are counted in the file named by `--fake-log`."""
import json, sys, time

argv = sys.argv[1:]
if "--version" in argv:
    print("9.9.9 (Fake)")
    sys.exit(0)
log = argv[argv.index("--fake-log") + 1]
sid = argv[argv.index("--session-id") + 1] if "--session-id" in argv else argv[argv.index("--resume") + 1]
with open(log, "a") as f:
    f.write(sid + "\n")
launch = len(open(log).read().split())


def out(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


def ctl_ok(rid, body):
    out({"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": body}})


for line in sys.stdin:
    m = json.loads(line)
    if m["type"] == "control_request":
        sub = m["request"]["subtype"]
        if sub == "initialize":
            ctl_ok(m["request_id"], {"commands": [], "models": [{"value": "haiku", "resolvedModel": "claude-haiku-4-5", "displayName": "Haiku"}], "current_permission_mode": "bypassPermissions"})
        elif sub == "get_settings":
            ctl_ok(m["request_id"], {"effective": {}})
        elif sub == "get_context_usage":
            ctl_ok(m["request_id"], {"model": "claude-haiku-4-5", "maxTokens": 200000, "totalTokens": 1})
        elif sub == "interrupt":
            pass  # a stuck CLI never answers
        continue
    out({"type": "system", "subtype": "init", "session_id": sid, "model": "claude-haiku-4-5", "permissionMode": "bypassPermissions"})
    out({"type": "stream_event", "parent_tool_use_id": None, "event": {"type": "message_start", "message": {"id": "m1", "usage": {}}}})
    out({"type": "stream_event", "parent_tool_use_id": None, "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}})
    if launch == 1:
        time.sleep(3600)
    out({"type": "result", "subtype": "success", "is_error": False, "num_turns": 1, "result": "partial", "session_id": sid})
