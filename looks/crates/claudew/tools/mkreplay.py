#!/usr/bin/env python3
"""Write the replay scripts the look is driven with, from the reference captures.

    mkreplay.py REF_DIR CWD OUT_DIR

CLAUDE DATA. The static, turn, turn2 and turn3 scripts hand the look what the reference
showed: Claude Code's slash commands, model names, saved sessions, modes and permission
prompts, read off the captures. That is only so the pixel layout can be compared cell by
cell. It says nothing about how the look handles Wizard's own data; the wizard-* scripts
(from a recorded `wizard acp` session) and live.py (real wizard) cover that.

The version the replay reports is the one a real wizard reported in that recorded session;
OUT_DIR/version.txt holds it for compare.py.
"""
import json, os, re, sys, time, zlib

ANSI = re.compile(r'\x1b\[[0-9;]*m|\x1b\][^\x07\x1b]*(\x07|\x1b\\)')
HERE = os.path.dirname(os.path.abspath(__file__))
RECORDED = os.path.join(HERE, '..', '..', 'backend-wizard', 'tests', 'fixtures', 'new_session.jsonl')


def recorded_version():
    """agentInfo.version from the recorded wizard's initialize answer."""
    for line in open(RECORDED, encoding='utf-8'):
        v = json.loads(line)['msg'].get('result', {})
        if isinstance(v, dict) and 'agentInfo' in v:
            return v['agentInfo']['version']
    raise SystemExit('no agentInfo.version in ' + RECORDED)


WIZARD_VERSION = recorded_version()


def rows(ref, name, size='200x50'):
    path = os.path.join(ref, '%s-%s.ansi' % (name, size))
    if not os.path.exists(path):
        return []
    return [ANSI.sub('', l).rstrip() for l in open(path, encoding='utf-8').read().split('\n')]


SIZES = ('200x50', '120x40', '80x24')


# argument hints the reference draws after a completed command
HINTS = {'model': '[model]'}


def fill(desc):
    """A description cut with an ellipsis gets more words so it is cut again."""
    return desc[:-1] + 'x and the rest of the description goes on' if desc.endswith('…') else desc


def menu_commands(ref, name):
    """Commands a menu capture shows, from every size; the widest capture's text wins."""
    seen = {}
    order = []
    for size in SIZES:
        for n, d in menu_commands_at(ref, name, size):
            if n not in seen:
                order.append(n)
                seen[n] = d
    return [(n, seen[n]) for n in order]


def menu_commands_at(ref, name, size):
    out = []
    for line in rows(ref, name, size):
        m = re.match(r'^  [❯ ] /(\S+)\s+(.*)$', line)
        if m:
            out.append([m.group(1), m.group(2)])
        elif out and re.match(r'^ {30,}\S', line):
            out[-1][1] += ' ' + line.strip()
        elif line.startswith('─'):
            break
    return [(n, fill(d)) for n, d in out]


def help_commands(ref):
    seen, out = set(), []
    for size in SIZES:
        lines = rows(ref, 'help-commands', size)
        for i, line in enumerate(lines):
            m = re.match(r'^   [❯ ] /(\S+)$', line)
            if m and i + 1 < len(lines) and m.group(1) not in seen:
                seen.add(m.group(1))
                out.append((m.group(1), fill(lines[i + 1].strip())))
    return out


def models(ref):
    out = []
    for line in rows(ref, 'model'):
        m = re.match(r'^\s+[❯↓ ]\s*\d+\.\s+(.+?)\s{2,}(.*)$', line)
        if m:
            out.append((m.group(1).replace(' ✔', ''), m.group(2), '✔' in m.group(1)))
    return out


def sessions(ref, cwd, now, size):
    out, lines = [], rows(ref, 'resume', size)
    unit = {'second': 1, 'minute': 60, 'hour': 3600, 'day': 86400}
    total = 0
    for line in lines:
        m = re.search(r'Resume session \((\d+) of (\d+)\)', line)
        if m:
            total = int(m.group(2))
    for i, line in enumerate(lines[:-1]):
        m = re.match(r'^     (\d+) (second|minute|hour|day)s? ago', lines[i + 1])
        if m and re.match(r'^   [❯↓↑ ] \S', line):
            title = line[5:].strip()
            is_id = re.fullmatch(r'[0-9a-f]{8}', title) is not None
            # the same session gets the same id at every size: from its title and age
            key = '%s|%s' % (title, lines[i + 1].strip())
            out.append({
                'id': (title if is_id else '%08x' % (zlib.crc32(key.encode()) & 0xffffffff)) + '-0000-4000-8000-000000000000',
                'title': '' if is_id else title,
                'cwd': cwd,
                # seconds before the replay lists it, so the age is the same when the panel opens
                'updated': -int(m.group(1)) * unit[m.group(2)] - 2,
                'bytes': size_of(lines[i + 1]),
            })
    # the rest of the reference's list, which its window never reaches, so the count shows
    for n in range(len(out), total):
        out.append({'id': '%08x-0000-4000-8000-000000000000' % (0x5e55 + n), 'title': 'older session %d' % n,
                    'cwd': cwd, 'updated': -40 * 86400 - n, 'bytes': 0})
    return out


def size_of(meta):
    """`1 minute ago · HEAD · 3.2KB` -> a byte count that prints as 3.2KB."""
    m = re.search(r' · ([0-9.]+)([KMG]?)B\s*$', meta)
    if not m:
        return 0
    return int(float(m.group(1)) * {'': 1, 'K': 1024, 'M': 1024 ** 2, 'G': 1024 ** 3}[m.group(2)])


def ev(e):
    return json.dumps({'event': e}, ensure_ascii=False)


def head(ref, cwd, size):
    seen, cmds = set(), []
    # the two entries after /dataviz are never on screen, but their two-row height decides
    # whether anything is drawn under it
    after = [('workflow-authoring', fill('Reference for writing a Workflow tool script (script API and gotchas, resume, quality patterns, worked examples). Load before authoring a script for a workflow…')),
             ('update-config', fill('Use this skill to configure the harness via settings.json. Automated behaviors ("from now on when X", "each time X", "whenever X", "before/after X") require hooks configured in settings…'))]
    lists = [menu_commands(ref, 'slash'), after] + [menu_commands(ref, n) for n in ('slash-filter', 'help-typed')] + [help_commands(ref)]
    tail = [('compact', 'Clear conversation history but keep a summary in context. Optional: /compact [instructions for summarization]'),
            ('fewer-permission-prompts', 'Scan your transcripts for common read-only tool calls')]
    for name, desc in [c for l in lists for c in l] + tail:
        if name not in seen:
            seen.add(name)
            desc = re.sub(r'\bClaude\b(?! Code| API|/| mobile|\.ai)', 'Wizard', desc)
            cmds.append({'name': name, 'description': desc, 'input_hint': HINTS.get(name, '')})
    ms = models(ref)
    current = next((n for n, _, cur in ms if cur), ms[0][0] if ms else 'model')
    slug = lambda n: 'claude-' + re.sub(r'[^a-z0-9]+', '-', n.lower()).strip('-')
    opts = [{'id': slug(n), 'name': n, 'provider': d} for n, d, _ in ms]
    opts += [{'id': 'extra-%d' % i, 'name': 'Extra %d' % i, 'provider': ''} for i in range(3)]
    config = {
        'model': slug(current), 'models': opts, 'effort': 'medium', 'efforts': ['low', 'medium', 'high'],
        'mode': 'manual', 'modes': ['manual', 'accept edits'], 'cwd': cwd, 'backend': 'wizard ' + WIZARD_VERSION,
    }
    return [
        ev({'Ready': {'session_id': '0191b6f0-3c1a-7a52-9a0e-6d3e9b2f1c44', 'config': config}}),
        ev({'Commands': cmds}),
        json.dumps({'sessions': [{k: v for k, v in s.items() if k != 'bytes'}
                                 for s in sessions(ref, cwd, int(time.time()), size)]}, ensure_ascii=False),
    ]


def tool(id_, kind, title, inp, status, output=None, diff=None, name=None):
    return ev({'Tool': {'id': id_, 'name': name or kind, 'kind': kind, 'title': title, 'input': inp,
                        'status': status, 'output': output, 'diff': diff, 'parent_id': None}})


def find_command(ref, cwd, size):
    """The search command the reference ran at this size, and what it printed, read off its
    detailed transcript."""
    path = os.path.join(ref, 'edit-detail-%s.ansi' % size)
    default = "find %s -name main.rs -not -path '*/target/*' 2>/dev/null" % cwd
    if not os.path.exists(path):
        return default, cwd + '/main.rs\n'
    lines = [ANSI.sub('', l).rstrip() for l in open(path, encoding='utf-8').read().split('\n')]
    for i, line in enumerate(lines):
        if line.startswith('● Bash(find '):
            text = line[len('● Bash('):]
            j = i + 1
            while j < len(lines) and lines[j].startswith('      '):
                text += ' ' + lines[j].strip()
                j += 1
            output = []
            while j < len(lines) and (lines[j].startswith('  ⎿') or (output and lines[j].startswith('     '))):
                output.append(lines[j][5:].strip())
                j += 1
            cmd = text[:-1] if text.endswith(')') else text
            return cmd, '\n'.join(output) + '\n' if output else cwd + '/main.rs\n'
    return default, cwd + '/main.rs\n'


def turn(cwd, found):
    find, find_out = found
    path = cwd + '/main.rs'
    old = 'fn main() {\n    println!("hello");\n}\n'
    new = 'fn main() {\n    println!("hi");\n}\n'
    diff = {'path': path, 'old': old, 'new': new}
    wait = lambda what: json.dumps({'await': what})
    sleep = lambda ms: json.dumps({'sleep': ms})
    end = ev({'TurnEnd': 'end_turn'})
    return [
        wait('prompt'), ev('TurnStart'), sleep(5000),
        tool('t1', 'execute', 'Printing hi', {'command': 'echo hi'}, 'running'), sleep(300),
        tool('t1', 'execute', 'Printing hi', {'command': 'echo hi'}, 'completed', 'hi\n'),
        ev({'TextDelta': 'done'}), end,
        wait('prompt'), ev('TurnStart'), sleep(300),
        tool('t2', 'execute', 'Finding main.rs', {'command': find}, 'completed', find_out),
        tool('t3', 'read', 'main.rs', {'path': path}, 'completed', old),
        tool('t4', 'edit', 'main.rs', {'path': path}, 'pending', None, diff),
        ev({'Permission': {'id': 'p1', 'tool': 'Edit', 'kind': 'edit', 'title': path, 'input': {},
                           'diff': diff, 'rule': 'edits'}}),
        wait('decide'), sleep(200),
        tool('t4', 'edit', 'main.rs', {'path': path}, 'completed', None, diff),
        ev({'TextDelta': 'done'}), end,
        wait('prompt'), ev('TurnStart'), sleep(600),
        ev({'Notice': {'level': 'info', 'text': 'Compacted (ctrl+o to see full summary)'}}), end,
    ]


MD = ('| file | lines |\n|------|-------|\n| a.rs | 3 |\n| b.rs | 120 |\n\n**`src/`**\n\n'
      '[Docs](https://example.com/docs)\n\n```rust\nfn main() { println!("hi"); }\n```')


def md_reply(ref, size):
    """The markdown reply as the model gave it at this size: at 80x24 it put it in a fenced
    markdown block, which the reference shows as written."""
    if any(l.startswith('  ```rust') for l in rows(ref, 'md', size)):
        return '```markdown\n' + MD + '\n```'
    return MD


def bash_call(ref, size):
    """The description and command the reference asked about, read off its prompt."""
    lines = rows(ref, 'bash-permission', size)
    try:
        at = next(i for i, l in enumerate(lines) if l.strip() == 'Bash command')
    except StopIteration:
        return 'Initialize a git repository', 'git init -q && echo ok'
    desc, command, inside = [], [], False
    for l in lines[at + 1:]:
        if l.startswith('╌'):
            if inside:
                break
            inside = True
            continue
        (command if inside else desc).append(l.strip())
    # the tip above the description can take two rows; the description is the row before the box
    return ([d for d in desc if d][-1:] or [''])[0], ' '.join(command)


def turn2(ref, cwd, size):
    wait = lambda what: json.dumps({'await': what})
    sleep = lambda ms: json.dumps({'sleep': ms})
    end = ev({'TurnEnd': 'end_turn'})
    desc, command = bash_call(ref, size)
    call = {'command': command, 'description': desc}
    path = cwd + '/w.txt'
    diff = {'path': path, 'old': None, 'new': ''.join('%d\n' % n for n in range(1, 31))}
    return [
        wait('prompt'), ev('TurnStart'), sleep(300), ev({'TextDelta': md_reply(ref, size)}), end,
        wait('prompt'), ev('TurnStart'), sleep(300),
        tool('b1', 'execute', desc, call, 'pending'),
        ev({'Permission': {'id': 'p2', 'tool': 'Bash', 'kind': 'execute', 'title': desc, 'input': call,
                           'diff': None, 'rule': 'git init *'}}),
        wait('decide'), ev({'TextDelta': 'done'}), end,
        wait('prompt'), ev('TurnStart'), sleep(300),
        tool('w1', 'edit', 'w.txt', {'path': path}, 'pending', None, diff),
        ev({'Permission': {'id': 'p3', 'tool': 'Write', 'kind': 'edit', 'title': path, 'input': {},
                           'diff': diff, 'rule': 'edits'}}),
        wait('decide'), sleep(200),
        tool('w1', 'edit', 'w.txt', {'path': path}, 'completed', None, diff),
        ev({'TextDelta': 'done'}), end,
    ]


def wizard_head(cwd):
    """Wizard's own models, efforts, modes and commands, from a recorded `wizard acp` session."""
    options, commands = [], []
    for line in open(RECORDED, encoding='utf-8'):
        msg = json.loads(line)['msg']
        res = msg.get('result')
        if isinstance(res, dict) and 'configOptions' in res:
            options = res['configOptions']
        upd = msg.get('params', {}).get('update', {})
        if 'availableCommands' in upd:
            commands = upd['availableCommands']
    def opt(i):
        return next((o for o in options if o['id'] == i), {})
    def values(o):
        return [v['value'] for v in o.get('options', [])]
    models = [{'id': m, 'name': m.split('/', 1)[-1], 'provider': m.split('/', 1)[0] if '/' in m else ''}
              for m in values(opt('model'))]
    config = {
        'model': opt('model').get('currentValue', ''), 'models': models,
        'effort': opt('thought_level').get('currentValue', ''), 'efforts': values(opt('thought_level')),
        'mode': opt('wizard_mode').get('currentValue', ''), 'modes': values(opt('wizard_mode')),
        'cwd': cwd, 'backend': 'wizard ' + WIZARD_VERSION,
    }
    cmds = [{'name': c['name'].lstrip('/'), 'description': c.get('description', ''),
             'input_hint': (c.get('input') or {}).get('hint', '')} for c in commands]
    return [
        ev({'Ready': {'session_id': '2026-10-14T03-00-00', 'config': config}}),
        ev({'Commands': cmds}),
        json.dumps({'sessions': []}),
    ]


def thinking(ref, size):
    """The queued prompt's turn shows `thinking` in the reference's spinner."""
    return any('· thinking' in l for l in rows(ref, 'queue-esc', size))


def counted(ref, scene, size):
    """The count as far as the reference got before Esc, as the model streamed it."""
    lines = rows(ref, scene, size)
    at = [i for i, l in enumerate(lines) if re.fullmatch(r'[● ] *\d+', l)]
    top = max(int(lines[i][1:]) for i in at) if at else 200
    # the model put a blank line between the numbers when the reference shows one
    gap = '\n\n' if len(at) > 1 and at[1] - at[0] == 2 else '\n'
    return gap.join(str(n) for n in range(1, top + 1))


def turn3(ref, cwd, size):
    """A count stopped with Esc, a prompt queued behind a count, then a plan put up for approval."""
    wait = lambda what: json.dumps({'await': what})
    sleep = lambda ms: json.dumps({'sleep': ms})
    end = ev({'TurnEnd': 'end_turn'})
    plan = plan_text(ref, size)
    return [
        wait('prompt'), ev('TurnStart'), sleep(1000), ev({'TextDelta': counted(ref, 'interrupt', size)}),
        wait('cancel'),
        wait('prompt'), ev('TurnStart'), sleep(800), ev({'TextDelta': counted(ref, 'queue', size)}),
        wait('cancel'),
        # the queued prompt's own turn has not answered yet when the reference is captured;
        # at some sizes it was already thinking
        wait('prompt'), ev('TurnStart')] + ([ev({'ThoughtDelta': 'Counting.'})] if thinking(ref, size) else []) + [
        sleep(4000), ev({'TextDelta': 'ok'}), end,
        wait('prompt'), ev('TurnStart'), sleep(1500),
        tool('x1', 'other', 'exit_plan', {'plan': plan}, 'running', name='exit_plan'),
        ev({'Permission': {'id': 'plan1', 'tool': 'exit_plan', 'kind': 'other', 'title': 'Ready to code?',
                           'input': {'plan': plan}, 'diff': None, 'rule': ''}}),
        wait('decide'),
        tool('x1', 'other', 'exit_plan', {'plan': plan}, 'failed', 'plan rejected', name='exit_plan'), end,
    ]


def plan_text(ref, size):
    """The plan the reference showed between its dashed rules, back as markdown: a bold row
    was a heading, a run in the code colour was `code`."""
    sys.path.insert(0, HERE)
    from cells import parse
    path = os.path.join(ref, 'plan-%s.ansi' % size)
    if not os.path.exists(path):
        return '1. Add `// entry point` as the first line of main.rs.\n2. Save the file.'
    inside, out = False, []
    for row in parse(open(path, encoding='utf-8').read()):
        text = ''.join(c for c, _ in row).rstrip()
        if text.strip().startswith('╌'):
            if inside:
                break
            inside = True
            continue
        if not inside:
            continue
        # the plan ran past the pane: the reference shows ↓ and no closing rule. The rest is
        # off its screen, so a line stands in for it and the look has to cut it the same way.
        if text.strip().startswith('─'):
            out.append('3. The rest of the plan, below the reference\'s screen.')
            break
        cut = text.endswith('↓')
        if cut:
            row = [(c, k) for c, k in row][:len(text) - 1]
        cells = [(c, k) for c, k in row if c][3:]
        body = ''.join(c for c, _ in cells).rstrip()
        if not body:
            out.append('')
            continue
        if all(k[2] for c, k in cells[:len(body)] if c != ' '):
            # bold, italic and underlined is a top heading, bold alone the next level
            top = all(k[4] and k[5] for c, k in cells[:len(body)] if c != ' ')
            out.append(('# ' if top else '## ') + body)
            continue
        line, code = '', False
        for c, k in cells[:len(body)]:
            is_code = str(k[0]) == '153'
            if is_code != code:
                line += '`'
                code = is_code
            line += c
        out.append(line + ('`' if code else ''))
    # a wrapped row carries on the line before it
    md = []
    for l in out:
        if md and l.startswith('   ') and md[-1]:
            md[-1] += ' ' + l.strip()
        else:
            md.append(l)
    return '\n'.join(md).strip()


def main():
    ref, cwd, out = sys.argv[1:4]
    os.makedirs(out, exist_ok=True)
    open(os.path.join(out, 'version.txt'), 'w').write(WIZARD_VERSION + '\n')
    # wizard's session files, as big as the reference said, for the sizes the picker shows
    files = {}
    for size in SIZES:
        for s in sessions(ref, cwd, 0, size):
            if s['bytes']:
                files[s['id']] = s['bytes']
    with open(os.path.join(out, 'session-files.tsv'), 'w') as f:
        for id_, size in files.items():
            f.write('%s\t%d\n' % (id_, size))
    for size in ('80x24', '120x40', '200x50'):
        h = head(ref, cwd, size)
        open(os.path.join(out, 'static-%s.jsonl' % size), 'w', encoding='utf-8').write('\n'.join(h) + '\n')
        script = h + turn(cwd, find_command(ref, cwd, size))
        open(os.path.join(out, 'turn2-%s.jsonl' % size), 'w', encoding='utf-8').write('\n'.join(h + turn2(ref, cwd, size)) + '\n')
        open(os.path.join(out, 'turn3-%s.jsonl' % size), 'w', encoding='utf-8').write('\n'.join(h + turn3(ref, cwd, size)) + '\n')
        open(os.path.join(out, 'wizard-%s.jsonl' % size), 'w', encoding='utf-8').write('\n'.join(wizard_head(cwd)) + '\n')
        open(os.path.join(out, 'turn-%s.jsonl' % size), 'w', encoding='utf-8').write('\n'.join(script) + '\n')


if __name__ == '__main__':
    main()
