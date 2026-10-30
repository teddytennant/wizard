#!/usr/bin/env python3
"""A fake Anthropic Messages server, so a real `claude` can be driven with no account.

    fake_anthropic.py PORT LOG

Point claude at it with ANTHROPIC_BASE_URL and a made-up ANTHROPIC_API_KEY, in a HOME and
CLAUDE_CONFIG_DIR of their own (see claude-isolated.sh). The scenes in drive.py send fixed
prompts; the prompt picks a script of tool calls, and once the script runs out the answer is
"done". Anything else gets "ok". Every request is logged as one JSON line.
"""
import json, re, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18998
LOG = open(sys.argv[2] if len(sys.argv) > 2 else '/dev/null', 'a')

MD = ('Here is the summary.\n\n| file | lines |\n|------|-------|\n| a.rs | 3 |\n| b.rs | 120 |\n\n'
      'Use `cargo build` to build **`src/`**, see [Docs](https://example.com/docs).\n\n'
      '```rust\nfn main() { println!("hi"); }\n```\n')
PLAN = '1. Add the comment `// entry point` as the first line of main.rs.\n2. Check the file still compiles.'
SUMMARY = ('<analysis>The user asked for small edits.</analysis>\n<summary>\n1. Primary Request: small '
           'edits to main.rs.\n2. Files: main.rs.\n</summary>')


def scripts(cwd):
    f = cwd + '/main.rs'
    return [
        ('echo hi', [('Bash', {'command': 'echo hi', 'description': 'Print hi'})], 'done'),
        ('Read main.rs, then change hello', [
            ('Read', {'file_path': f}),
            ('Edit', {'file_path': f, 'old_string': 'hello', 'new_string': 'hi'})], 'done'),
        ('not in the file', [('Edit', {'file_path': f, 'old_string': 'not in the file', 'new_string': 'x'})], 'done'),
        ('git init -q', [('Bash', {'command': 'git init -q && echo ok', 'description': 'Init a repo'})], 'done'),
        ('Use the Write tool to create w.txt', [
            ('Write', {'file_path': cwd + '/w.txt', 'content': ''.join('%d\n' % i for i in range(1, 31))})], 'done'),
        ('Plan adding the comment', [('ExitPlanMode', {'plan': PLAN})], 'The plan was not approved.'),
        ('No tools. Output exactly this markdown', [], MD),
        ('Count from 1 to 3000', [], '\n'.join(str(i) for i in range(1, 3001))),
        ('what is 2+2', [], '4'),
    ]


def text_of(content):
    """The text blocks of a message, without the <system-reminder> ones claude adds."""
    if isinstance(content, str):
        return content
    return ''.join(b.get('text', '') for b in content
                   if b.get('type') == 'text' and not b.get('text', '').lstrip().startswith('<system-reminder>'))


def plan_for(body):
    system = body.get('system') or ''
    system = system if isinstance(system, str) else text_of(system)
    m = re.search(r'working directory: (\S+)', system)
    cwd = m.group(1) if m else '.'
    msgs = body.get('messages', [])
    prompt, tool_uses = '', 0
    for msg in msgs:
        if msg['role'] == 'user':
            t = text_of(msg['content'])
            if t.strip():
                prompt, tool_uses = t, 0
        elif isinstance(msg['content'], list):
            tool_uses += sum(1 for b in msg['content'] if b.get('type') == 'tool_use')
    last = text_of(msgs[-1]['content']) if msgs else ''
    if 'CRITICAL: Respond with TEXT ONLY' in last or 'create a detailed summary' in last.lower():
        return None, SUMMARY
    for key, calls, final in scripts(cwd):
        if key in prompt:
            offered = {t.get('name') for t in body.get('tools', [])}
            if tool_uses < len(calls) and calls[tool_uses][0] in offered:
                return calls[tool_uses], None
            return None, final
    return None, 'ok'


class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *a):
        pass

    def reply_json(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        self.reply_json({'data': [], 'has_more': False})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get('Content-Length', 0))) or b'{}')
        if self.path.endswith('/count_tokens'):
            return self.reply_json({'input_tokens': 100})
        if not self.path.split('?')[0].endswith('/messages'):
            return self.reply_json({})
        call, text = plan_for(body)
        LOG.write(json.dumps({'model': body.get('model'), 'tools': len(body.get('tools', [])),
                              'call': call and call[0], 'text': text and text[:40]}) + '\n')
        LOG.flush()
        usage = {'input_tokens': 120, 'output_tokens': 20, 'cache_creation_input_tokens': 0,
                 'cache_read_input_tokens': 0}
        if call:
            blocks = [{'type': 'tool_use', 'id': 'toolu_%d' % time.time_ns(), 'name': call[0], 'input': call[1]}]
        else:
            blocks = [{'type': 'text', 'text': text}]
        stop = 'tool_use' if call else 'end_turn'
        message = {'id': 'msg_%d' % time.time_ns(), 'type': 'message', 'role': 'assistant',
                   'model': body.get('model', 'claude-haiku-5-5'), 'content': blocks,
                   'stop_reason': stop, 'stop_sequence': None, 'usage': usage}
        if not body.get('stream'):
            return self.reply_json(message)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Cache-Control', 'no-cache')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()

        def send(event, data):
            chunk = ('event: %s\ndata: %s\n\n' % (event, json.dumps(data))).encode()
            self.wfile.write(b'%x\r\n' % len(chunk) + chunk + b'\r\n')
            self.wfile.flush()

        try:
            send('message_start', {'type': 'message_start', 'message': dict(message, content=[], stop_reason=None)})
            block = blocks[0]
            if block['type'] == 'tool_use':
                send('content_block_start', {'type': 'content_block_start', 'index': 0,
                                             'content_block': dict(block, input={})})
                send('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': {
                    'type': 'input_json_delta', 'partial_json': json.dumps(block['input'])}})
            else:
                send('content_block_start', {'type': 'content_block_start', 'index': 0,
                                             'content_block': {'type': 'text', 'text': ''}})
                lines = block['text'].split('\n')
                slow = len(lines) > 1000
                for i in range(0, len(lines), 20 if slow else 1000):
                    piece = '\n'.join(lines[i:i + (20 if slow else 1000)])
                    send('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': {
                        'type': 'text_delta', 'text': piece + ('\n' if slow else '')}})
                    if slow:
                        time.sleep(0.05)
            send('content_block_stop', {'type': 'content_block_stop', 'index': 0})
            send('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': stop, 'stop_sequence': None},
                                   'usage': usage})
            send('message_stop', {'type': 'message_stop'})
            self.wfile.write(b'0\r\n\r\n')
        except (BrokenPipeError, ConnectionResetError):
            pass


ThreadingHTTPServer(('127.0.0.1', PORT), H).serve_forever()
