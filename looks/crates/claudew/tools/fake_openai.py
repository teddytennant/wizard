#!/usr/bin/env python3
"""A fake OpenAI-compatible server, so live.py can drive real `wizard acp` sessions with no model.

    fake_openai.py PORT PROJECT_DIR LOG

The last word of the user's message picks the answer: md, long, slow, bash, git, read, edit,
badedit, write, refuse, plan, waitN (answers after N seconds); anything else gets "ok". A tool
result always gets "done". Each request is logged with the tools offered, the model and whether
wizard was in plan mode.
"""
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18999
CWD = sys.argv[2] if len(sys.argv) > 2 else '.'
LOG = open(sys.argv[3] if len(sys.argv) > 3 else '/dev/null', 'a')
# answers taken from the reference captures, read when asked: `<log dir>/shared-<name>.txt`
import os
SHARED_DIR = os.path.dirname(sys.argv[3]) if len(sys.argv) > 3 else None


def cwd():
    """The project the paths point into: the shared scenes run in a project of their own."""
    return shared('cwd', CWD).strip()


def shared(name, default):
    try:
        return open(os.path.join(SHARED_DIR, 'shared-%s.txt' % name), encoding='utf-8').read()
    except (OSError, TypeError):
        return default

MD = ('Here is the summary.\n\n| file | lines |\n|------|-------|\n| a.rs | 3 |\n| b.rs | 120 |\n\n'
      'Use `cargo build` to build **`src/`**, see [Docs](https://example.com/docs).\n\n'
      '```rust\nfn main() { println!("hi"); }\n```\n')
LONG = '\n\n'.join('Paragraph %d of a long answer that goes on for a while so the transcript scrolls.' % i
                   for i in range(1, 61))


def pick(tools, *names):
    for n in names:
        for t in tools:
            if t.lower() == n.lower():
                return t
    return names[0]


class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *a):
        pass

    def send_json(self, obj):
        b = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        # a slow model list holds wizard's first session open: `slow-get` beside the log
        # holds the seconds
        try:
            time.sleep(float(shared('slow-get', '0') or 0))
        except ValueError:
            pass
        self.send_json({'object': 'list', 'data': [{'id': 'fake-model', 'object': 'model', 'created': 1},
                                                   {'id': 'fake-other', 'object': 'model', 'created': 0}]})

    def chunk(self, delta, finish=None):
        obj = {'id': 'x', 'object': 'chat.completion.chunk', 'created': 1, 'model': 'fake-model',
               'choices': [{'index': 0, 'delta': delta, 'finish_reason': finish}]}
        data = ('data: %s\n\n' % json.dumps(obj)).encode()
        self.wfile.write(b'%x\r\n%s\r\n' % (len(data), data))
        self.wfile.flush()

    def text(self, s, delay=0.0):
        for i in range(0, len(s), 40):
            self.chunk({'content': s[i:i + 40]})
            if delay:
                time.sleep(delay)

    def tool(self, name, args):
        self.chunk({'tool_calls': [{'index': 0, 'id': 'call_%d' % int(time.time() * 1000), 'type': 'function',
                                    'function': {'name': name, 'arguments': json.dumps(args)}}]})

    def do_POST(self):
        try:
            self.answer()
        except (BrokenPipeError, ConnectionResetError):
            pass  # wizard hung up: an interrupted turn

    def answer(self):
        n = int(self.headers.get('Content-Length', 0))
        body = json.loads(self.rfile.read(n) or b'{}')
        msgs = body.get('messages', [])
        tools = [t.get('function', {}).get('name', '') for t in body.get('tools', [])]
        last = msgs[-1] if msgs else {}
        users = [m for m in msgs if m.get('role') == 'user']
        u = users[-1].get('content', '') if users else ''
        if isinstance(u, list):
            u = ' '.join(p.get('text', '') for p in u if isinstance(p, dict))
        plan = any('## Plan mode (active)' in str(m.get('content')) for m in msgs if m.get('role') == 'system')
        LOG.write(json.dumps({'tools': tools, 'last_role': last.get('role'), 'user': u[-200:],
                              'plan': plan, 'model': body.get('model'),
                              'last': str(last.get('content'))[:300]}) + '\n')
        LOG.flush()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()
        self.chunk({'role': 'assistant'})
        key = u.strip().split()[-1] if u.strip() else ''
        # the prompts shared with the reference are told apart by their text; what to answer
        # comes from the reference itself (live.py writes it to files beside the log)
        if 'not in the file' in u:
            key = 'badedit'
        elif 'what is 2+2' in u:
            key = 'btw'
        elif 'Plan adding the comment' in u:
            key = 'plan'
        if last.get('role') == 'tool':
            self.text(shared('badedit-reply', 'done') if key == 'badedit' else 'done')
            fin = 'stop'
        elif key.startswith('wait') and key[4:].isdigit():
            time.sleep(int(key[4:]))
            self.text('waited')
            fin = 'stop'
        elif key == 'refuse':
            # in chat mode wizard does not offer write_file, and refuses the call
            self.tool('write_file', {'path': cwd() + '/refused.txt', 'content': 'x'})
            fin = 'tool_calls'
        elif key == 'md':
            self.text(MD, 0.05)
            fin = 'stop'
        elif key == 'long':
            self.text(LONG, 0.01)
            fin = 'stop'
        elif key == 'slow':
            self.text('Thinking about it', 0)
            time.sleep(40)
            self.text(' and done.')
            fin = 'stop'
        elif key == 'bash':
            self.tool(pick(tools, 'execute', 'bash'), {'command': 'echo hi', 'description': 'Print hi'})
            fin = 'tool_calls'
        elif key == 'git':
            self.tool(pick(tools, 'execute', 'bash'), {'command': 'git init -q && echo ok', 'description': 'Initialize a git repository'})
            fin = 'tool_calls'
        elif key == 'read':
            self.tool(pick(tools, 'read_file', 'read', 'view'), {'path': cwd() + '/main.rs'})
            fin = 'tool_calls'
        elif key == 'edit':
            self.tool(pick(tools, 'edit_file', 'edit', 'str_replace'), {'path': cwd() + '/main.rs', 'old_string': 'hello', 'new_string': 'hi', 'old': 'hello', 'new': 'hi'})
            fin = 'tool_calls'
        elif key == 'btw':
            self.text(shared('btw', '2 + 2 = 4.'))
            fin = 'stop'
        elif key == 'badedit':
            # an edit that cannot apply: its text is not in the file
            self.tool(pick(tools, 'edit_file', 'edit', 'str_replace'), {'path': cwd() + '/main.rs', 'old_string': 'not in the file', 'new_string': 'x', 'old': 'not in the file', 'new': 'x'})
            fin = 'tool_calls'
        elif key == 'plan' and 'exit_plan' in tools:
            self.tool('exit_plan', {'plan': shared('plan', '1. Add `// entry point` as the first line of main.rs.\n2. Save the file.')})
            fin = 'tool_calls'
        elif key == 'write':
            self.tool(pick(tools, 'write_file', 'write', 'create_file'), {'path': cwd() + '/w.txt', 'content': ''.join('%d\n' % i for i in range(1, 31))})
            fin = 'tool_calls'
        else:
            self.text('ok')
            fin = 'stop'
        self.chunk({}, fin)
        usage = {'id': 'x', 'object': 'chat.completion.chunk', 'created': 1, 'model': 'fake-model', 'choices': [],
                 'usage': {'prompt_tokens': 100, 'completion_tokens': 10, 'total_tokens': 110}}
        data = ('data: %s\n\ndata: [DONE]\n\n' % json.dumps(usage)).encode()
        self.wfile.write(b'%x\r\n%s\r\n0\r\n\r\n' % (len(data), data))
        self.wfile.flush()


ThreadingHTTPServer(('127.0.0.1', PORT), H).serve_forever()
