"""Scripted OpenAI-compatible endpoint for driving the reference jcode binary.

The last user message picks a scenario with `fake:<name>`; the look's own mock backend
(`src/fake.rs`) answers the same names, so both programs can be sent the same prompt and their
screens compared. Each scenario is a list of steps; a step is one model reply (text, tool calls,
or both) and the next step is served when the client comes back with the tool results.

    python3 fake_llm.py <port> <project-dir> [request-log.jsonl]
"""

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = "grok-4.7"

MARKDOWN = """# Heading one

Some **bold** text, some *italic* text and `inline code`.

## Heading two

- first bullet
- second bullet with a longer line that keeps going so that it has to wrap at the edge of a narrow terminal window
  - nested bullet

1. numbered one
2. numbered two

> a block quote

```rust
fn main() {
    println!("hello");
}
```

| name | value |
| ---- | ----- |
| one  | 1     |
| two  | 2     |

A [link](https://example.com) and a rule:

---

Done."""

LONG = "\n\n".join(
    "Paragraph %d. The quick brown fox jumps over the lazy dog and keeps running through the field until the sun goes down." % i
    for i in range(1, 31)
)


def tool(name, args, call_id):
    return {"name": name, "args": args, "id": call_id}


def scenarios(cwd):
    return {
        "text": [{"text": "Hello from the scripted model. This is a plain reply."}],
        "markdown": [{"text": MARKDOWN}],
        "long": [{"text": LONG}],
        "slow": [{"text": "Slow reply finished.", "delay": 30.0}],
        "bash": [
            {
                "text": "Listing the directory.",
                "tools": [tool("bash", {"command": "printf 'alpha\\nbeta\\ngamma\\n'", "description": "Print three lines"}, "call_bash_1")],
            },
            {"text": "Three lines were printed."},
        ],
        "bashfail": [
            {"tools": [tool("bash", {"command": "sh -c 'echo boom >&2; exit 3'", "description": "Run a failing command"}, "call_fail_1")]},
            {"text": "The command failed."},
        ],
        "edit": [
            {
                "text": "Creating the file.",
                "tools": [tool("write", {"file_path": cwd + "/notes.txt", "content": "one\ntwo\nthree\n", "description": "Create notes.txt"}, "call_write_1")],
            },
            {
                "text": "Now changing the second line.",
                "tools": [
                    tool(
                        "edit",
                        {"file_path": cwd + "/notes.txt", "old_string": "two", "new_string": "TWO\ntwo and a half", "description": "Rewrite line two"},
                        "call_edit_1",
                    )
                ],
            },
            {"text": "Edited `notes.txt`."},
        ],
        "read": [
            {"tools": [tool("read", {"file_path": cwd + "/notes.txt", "description": "Read notes.txt"}, "call_read_1")]},
            {"text": "Read the file."},
        ],
        "todo": [
            {
                "tools": [
                    tool(
                        "todo",
                        {
                            "todos": [
                                {"content": "Read the code", "status": "completed", "priority": "high", "id": "1"},
                                {"content": "Write the patch", "status": "in_progress", "priority": "high", "id": "2"},
                                {"content": "Run the tests", "status": "pending", "priority": "medium", "id": "3"},
                            ],
                            "description": "Plan the work",
                        },
                        "call_todo_1",
                    )
                ]
            },
            {"text": "Plan recorded."},
        ],
    }


def pick(messages):
    last = ""
    for m in messages:
        if m.get("role") == "user":
            c = m.get("content")
            if isinstance(c, list):
                c = " ".join(p.get("text", "") for p in c if isinstance(p, dict))
            last = c or ""
    name = "text"
    if "fake:" in last:
        name = last.split("fake:", 1)[1].split()[0]
    step = 0
    seen_user = False
    for m in reversed(messages):
        if m.get("role") == "user":
            seen_user = True
            break
        if m.get("role") == "assistant":
            step += 1
    return name, step if seen_user else 0


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    log_path = None
    cwd = "."

    def log_message(self, *args):
        pass

    def _json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self._json({"object": "list", "data": [{"id": MODEL, "object": "model", "owned_by": "fake"}]})
        else:
            self._json({})

    def _event(self, obj):
        data = ("data: %s\n\n" % (obj if isinstance(obj, str) else json.dumps(obj))).encode()
        self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
        self.wfile.flush()

    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0"))
        req = json.loads(self.rfile.read(n) or b"{}")
        if self.log_path:
            with open(self.log_path, "a") as f:
                f.write(json.dumps({"path": self.path, "body": req}) + "\n")
        messages = req.get("messages", [])
        name, step = pick(messages)
        steps = scenarios(self.cwd).get(name) or [{"text": "unknown scenario " + name}]
        reply = steps[min(step, len(steps) - 1)]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        base = {"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 0, "model": MODEL}
        try:
            time.sleep(reply.get("delay", 0.2))
            self._event({**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": None}]})
            text = reply.get("text", "")
            for i in range(0, len(text), 24):
                self._event({**base, "choices": [{"index": 0, "delta": {"content": text[i : i + 24]}, "finish_reason": None}]})
                time.sleep(0.01)
            for i, t in enumerate(reply.get("tools", [])):
                self._event(
                    {
                        **base,
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"tool_calls": [{"index": i, "id": t["id"], "type": "function", "function": {"name": t["name"], "arguments": json.dumps(t["args"])}}]},
                                "finish_reason": None,
                            }
                        ],
                    }
                )
            finish = "tool_calls" if reply.get("tools") else "stop"
            self._event({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]})
            self._event({**base, "choices": [], "usage": {"prompt_tokens": 1200, "completion_tokens": 34, "total_tokens": 1234}})
            self._event("[DONE]")
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    port = int(sys.argv[1])
    Handler.cwd = sys.argv[2]
    Handler.log_path = sys.argv[3] if len(sys.argv) > 3 else None
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
