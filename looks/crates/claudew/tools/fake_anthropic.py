#!/usr/bin/env python3
"""A fake Anthropic Messages server, so a real `claude` can be driven with no account.

    fake_anthropic.py PORT LOG

Point claude at it with ANTHROPIC_BASE_URL and a made-up ANTHROPIC_API_KEY, in a HOME and
CLAUDE_CONFIG_DIR of their own (see claude-isolated.sh). The scenes in drive.py send fixed
prompts; the prompt picks a script of tool calls, and once the script runs out the answer is
"done". Anything else gets "ok". Every request is logged as one JSON line.
"""
import json, os, re, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18998
LOG = open(sys.argv[2] if len(sys.argv) > 2 else '/dev/null', 'a')

MD = ('| file | lines |\n|------|-------|\n| a.rs | 3 |\n| b.rs | 120 |\n\n**`src/`**\n\n'
      '[Docs](https://example.com/docs)\n\n```rust\nfn main() { println!("hi"); }\n```')
PLAN = '1. Add the comment `// entry point` as the first line of main.rs.\n2. Check the file still compiles.'
TASKS = [
    {'subject': 'Read main.rs', 'description': 'Read the file', 'activeForm': 'Reading main.rs'},
    {'subject': 'Change hello to hi', 'description': 'Edit the string', 'activeForm': 'Changing hello to hi'},
    {'subject': 'Run the tests', 'description': 'Run cargo test', 'activeForm': 'Running the tests'},
]
SUMMARY = ('<analysis>The user asked for small edits.</analysis>\n<summary>\n1. Primary Request: small '
           'edits to main.rs.\n2. Files: main.rs.\n</summary>')


def scripts(cwd, plan_file):
    f = cwd + '/main.rs'
    return [
        ('echo hi', [('Bash', {'command': 'echo hi', 'description': 'Printing hi'})], 'done'),
        ('Read main.rs, then change hello', [
            ('Read', {'file_path': f}),
            ('Edit', {'file_path': f, 'old_string': 'hello', 'new_string': 'hi'})], 'done'),
        ('not in the file', [('Edit', {'file_path': f, 'old_string': 'not in the file', 'new_string': 'x'})], 'done'),
        ('git init -q', [('Bash', {'command': 'git init -q && echo ok', 'description': 'Init a repo'})], 'done'),
        ('Use the Write tool to create w.txt', [
            ('Write', {'file_path': cwd + '/w.txt', 'content': ''.join('%d\n' % i for i in range(1, 31))})], 'done'),
        ('Plan adding the comment', [('Write', {'file_path': plan_file, 'content': PLAN}), ('ExitPlanMode', {})], 'The plan was not approved.'),
        ('No tools. Output exactly this markdown', [], MD),
        ('Count from 1 to 3000', [], '\n'.join(str(i) for i in range(1, 3001))),
        ('what is 2+2', [], '4'),
        ('Think it over', [], 'The answer is 4.', 'Two plus two is four, so the answer is 4.'),
        ('Probe task tools', [('ToolSearch', {'query': 'e', 'max_results': 30})], 'done'),
        ('Probe paths', [
            ('Read', {'file_path': cwd + '/src/lib.rs'}),
            ('Read', {'file_path': cwd + '/../outside.txt'}),
            ('Read', {'file_path': '/etc/hostname'}),
            ('Write', {'file_path': cwd + '/src/new.txt', 'content': 'x\n'})], 'done'),
        ('Make many tasks', [('ToolSearch', {'query': 'select:TaskCreate,TaskGet,TaskUpdate', 'max_results': 3})] +
            [('TaskCreate', {'subject': 'Step %d of the long job, with a name long enough to need cutting at narrow widths' % n,
                             'description': 'step', 'activeForm': 'Doing step %d of the long job, with a name long enough to need cutting at narrow widths' % n}) for n in range(1, 15)] +
            [('TaskUpdate', {'taskId': str(n), 'status': 'completed'}) for n in range(1, 4)] +
            [('TaskUpdate', {'taskId': '4', 'status': 'in_progress'})], 'done'),
        ('Make long progress', [('ToolSearch', {'query': 'select:TaskCreate,TaskGet,TaskUpdate', 'max_results': 3})] +
            [('TaskCreate', {'subject': 'Step %d' % n, 'description': 'step', 'activeForm': 'Step %d' % n})
             for n in range(1, 13)] +
            [('TaskUpdate', {'taskId': str(n), 'status': 'completed'}) for n in range(1, 9)] +
            [('TaskUpdate', {'taskId': '9', 'status': 'in_progress'})], 'done'),
        ('Finish the tasks', [('TaskUpdate', {'taskId': str(n), 'status': 'completed'}) for n in range(1, 4)], 'done'),
        ('Make a task list', [
            ('ToolSearch', {'query': 'select:TaskCreate,TaskGet,TaskUpdate', 'max_results': 3}),
            ('TaskCreate', TASKS[0]), ('TaskCreate', TASKS[1]), ('TaskCreate', TASKS[2]),
            ('TaskUpdate', {'taskId': '1', 'status': 'completed'}),
            ('TaskUpdate', {'taskId': '2', 'status': 'in_progress'})], 'done'),
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
    plan_file = (re.search(r'(/[^"\\\s]*/plans/[^"\\\s]*\.md)', json.dumps(body)) or [None, cwd + '/plan.md'])[1]
    prompt, tool_uses = '', 0
    for msg in msgs:
        if msg['role'] == 'user':
            t = text_of(msg['content'])
            # text that rides along with a tool result (a ToolSearch answer) is not a new prompt
            carries_result = isinstance(msg['content'], list) and any(
                b.get('type') == 'tool_result' for b in msg['content'])
            if t.strip() and not carries_result:
                prompt, tool_uses = t, 0
            elif carries_result:
                # a prompt typed after a refused call arrives in the refusal's own message
                refused = any(b.get('type') == 'tool_result' and "doesn't want to proceed" in json.dumps(b.get('content'))
                              for b in msg['content'])
                typed = [b['text'] for b in msg['content'] if b.get('type') == 'text'
                         and not b['text'].lstrip().startswith(('<system-reminder>', '[Request interrupted'))]
                if refused and typed:
                    prompt, tool_uses = typed[-1], 0
        elif isinstance(msg['content'], list):
            tool_uses += sum(1 for b in msg['content'] if b.get('type') == 'tool_use')
    last = text_of(msgs[-1]['content']) if msgs else ''
    if 'CRITICAL: Respond with TEXT ONLY' in last or 'create a detailed summary' in last.lower():
        return None, SUMMARY, None
    for key, calls, final, *think in scripts(cwd, plan_file):
        if key in prompt:
            offered = {t.get('name') for t in body.get('tools', [])}
            if tool_uses < len(calls) and (calls[tool_uses][0] in offered or calls[tool_uses][0] == 'ExitPlanMode'):
                return calls[tool_uses], None, None
            return None, final, (think[0] if think and not tool_uses else None)
    return None, 'ok', None


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
        call, text, thought = plan_for(body)
        if os.environ.get('FAKE_DUMP'):
            open(os.environ['FAKE_DUMP'], 'a').write(json.dumps(body.get('messages', [])) + '\n')
        if call is None and text == 'done' and '"TaskUpdate"' in json.dumps(body.get('messages', [])):
            # hold the last answer so the list can be captured while the turn is still running
            time.sleep(float(os.environ.get('FAKE_TASK_PAUSE', '8')))
        if call is None and text == 'done' and '"echo hi"' in json.dumps(body.get('messages', [])):
            # hold the answer after the shell command so the turn-spinner scene finds the turn
            # running when it is captured, half a second after Enter
            time.sleep(float(os.environ.get('FAKE_BASH_PAUSE', '3')))
        if os.environ.get('FAKE_SCHEMA'):
            open(os.environ['FAKE_SCHEMA'], 'a').write(json.dumps(
                [t for t in body.get('tools', []) if t.get('name', '').startswith('Task')]) + '\n')
        LOG.write(json.dumps({'model': body.get('model'), 'tools': [t.get('name') for t in body.get('tools', [])],
                              'call': call and call[0], 'text': text and text[:40]}) + '\n')
        LOG.flush()
        usage = {'input_tokens': 120, 'output_tokens': 20, 'cache_creation_input_tokens': 0,
                 'cache_read_input_tokens': 0}
        if call:
            blocks = [{'type': 'tool_use', 'id': 'toolu_%d' % time.time_ns(), 'name': call[0], 'input': call[1]}]
        else:
            blocks = [{'type': 'text', 'text': text}]
        if thought:
            blocks.insert(0, {'type': 'thinking', 'thinking': thought, 'signature': 'fake-signature'})
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
            for index, block in enumerate(blocks):
                if block['type'] == 'tool_use':
                    send('content_block_start', {'type': 'content_block_start', 'index': index,
                                                 'content_block': dict(block, input={})})
                    send('content_block_delta', {'type': 'content_block_delta', 'index': index, 'delta': {
                        'type': 'input_json_delta', 'partial_json': json.dumps(block['input'])}})
                elif block['type'] == 'thinking':
                    send('content_block_start', {'type': 'content_block_start', 'index': index,
                                                 'content_block': {'type': 'thinking', 'thinking': '', 'signature': ''}})
                    send('content_block_delta', {'type': 'content_block_delta', 'index': index, 'delta': {
                        'type': 'thinking_delta', 'thinking': block['thinking']}})
                    send('content_block_delta', {'type': 'content_block_delta', 'index': index, 'delta': {
                        'type': 'signature_delta', 'signature': block['signature']}})
                else:
                    send('content_block_start', {'type': 'content_block_start', 'index': index,
                                                 'content_block': {'type': 'text', 'text': ''}})
                    lines = block['text'].split('\n')
                    slow = len(lines) > 1000
                    for i in range(0, len(lines), 20 if slow else 1000):
                        piece = '\n'.join(lines[i:i + (20 if slow else 1000)])
                        send('content_block_delta', {'type': 'content_block_delta', 'index': index, 'delta': {
                            'type': 'text_delta', 'text': piece + ('\n' if slow else '')}})
                        if slow:
                            time.sleep(0.05)
                send('content_block_stop', {'type': 'content_block_stop', 'index': index})
            send('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': stop, 'stop_sequence': None},
                                   'usage': usage})
            send('message_stop', {'type': 'message_stop'})
            self.wfile.write(b'0\r\n\r\n')
        except (BrokenPipeError, ConnectionResetError):
            pass


ThreadingHTTPServer(('127.0.0.1', PORT), H).serve_forever()
