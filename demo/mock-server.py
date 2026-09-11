#!/usr/bin/env python3
"""An OpenAI-compatible server that streams one canned answer, for demo/first-run.tape.

    python3 demo/mock-server.py            # 127.0.0.1:8089
    python3 demo/mock-server.py 9000

Answers GET /v1/models (the first-run key check) and POST /v1/chat/completions,
streamed as SSE when the request asks for it. Nothing it says comes from a model.
"""

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

MODEL = "demo-model"
ANSWER = (
    "This is a single-file project. `main.py` defines `add(a, b)` and a `main()` that "
    "prints `2 + 3`, guarded by the usual `__name__` check.\n\n"
    "One thing stands out: `add` returns `a - b`, and the comment above it says so. "
    "Want me to fix it and add a test?"
)


def chunk(delta, finish=None):
    body = {
        "id": "chatcmpl-demo",
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": MODEL,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }
    return f"data: {json.dumps(body)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send_json(self, body, status=200):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self.send_json({"object": "list", "data": [{"id": MODEL, "object": "model"}]})
        else:
            self.send_json({"error": "not found"}, 404)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        request = json.loads(self.rfile.read(length) or b"{}")
        if not self.path.rstrip("/").endswith("/chat/completions"):
            return self.send_json({"error": "not found"}, 404)
        if not request.get("stream"):
            return self.send_json({
                "id": "chatcmpl-demo", "object": "chat.completion", "model": MODEL,
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": ANSWER}}],
            })
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self.wfile.write(chunk({"role": "assistant", "content": ""}))
        for word in ANSWER.split(" "):
            self.wfile.write(chunk({"content": word + " "}))
            self.wfile.flush()
            time.sleep(0.04)
        self.wfile.write(chunk({}, "stop"))
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8089
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
