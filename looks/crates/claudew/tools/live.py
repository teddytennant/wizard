#!/usr/bin/env python3
"""Drive real `wizard` (skin claude, so it starts wizard-ui-claude) against tools/fake_openai.py
and check what only shows with Wizard's own data.

    live.py --wizard BIN --look BIN --cwd DIR --work DIR [--sizes 80x24,120x40]

Two throwaway homes under WORK: one whose provider is the fake server, one with no working
provider at all. Nothing under the real ~/.wizard is read or written. Captures go to
WORK/live/<scene>-<WxH>.ansi; every failed check is printed, and the exit status is 1 if any.
tmux runs on the socket `lookclaude` only.
"""
import argparse, json, os, re, shutil, socket, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
SGR = re.compile(r'\x1b\[[0-9;:]*m')
# OSC 8 hyperlinks around file names and URLs
OSC = re.compile(r'\x1b\]8;[^\x07\x1b]*(?:\x07|\x1b\\)')


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def home(path, provider):
    os.makedirs(os.path.join(path, '.wizard'), exist_ok=True)
    with open(os.path.join(path, '.wizard', 'config.toml'), 'w') as f:
        f.write(provider + '\n[ui]\nskin = "claude"\n\n[update]\nnotify = false\nauto = false\n')


def text(path):
    with open(path, encoding='utf-8') as f:
        return OSC.sub('', SGR.sub('', f.read()))


def check(fails, scene, ok, what):
    if not ok:
        fails.append('%s: %s' % (scene, what))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--wizard', required=True)
    ap.add_argument('--look', required=True)
    ap.add_argument('--cwd', required=True)
    ap.add_argument('--work', required=True)
    ap.add_argument('--sizes', default='120x40')
    ap.add_argument('--ref', help='reference captures: the shared scenes (x-*) run against their data')
    ap.add_argument('--shared-only', action='store_true', help='run only the shared scenes')
    ap.add_argument('--xcwd', help='the project the shared scenes run in (default: --cwd)')
    a = ap.parse_args()
    out = os.path.join(a.work, 'live')
    os.makedirs(out, exist_ok=True)
    bindir = os.path.join(a.work, 'live-bin')
    os.makedirs(bindir, exist_ok=True)
    for name, target in (('wizard', a.wizard), ('wizard-ui-claude', a.look)):
        link = os.path.join(bindir, name)
        if os.path.lexists(link):
            os.remove(link)
        os.symlink(os.path.abspath(target), link)
    version = subprocess.run([a.wizard, '--version'], capture_output=True, text=True).stdout.split()
    version = version[-1] if version else ''
    port = free_port()
    log = os.path.join(a.work, 'fake.log')
    fake = subprocess.Popen([sys.executable, os.path.join(HERE, 'fake_openai.py'), str(port), a.cwd, log],
                            stderr=subprocess.DEVNULL)
    good = os.path.join(a.work, 'live-home')
    bad = os.path.join(a.work, 'live-home-broken')
    home(good, 'model = "fake-model"\nactive_provider = "fake"\n\n[[providers]]\nname = "fake"\n'
               'kind = "openai"\nbase_url = "http://127.0.0.1:%d/v1"\nmodel = "fake-model"\n' % port)
    # a provider whose key is never set
    home(bad, 'model = "grok-4"\nactive_provider = "xai"\n\n[[providers]]\nname = "xai"\nkind = "xai"\n'
              'base_url = "http://127.0.0.1:9/v1"\nmodel = "grok-4"\napi_key_env = "CLAUDEW_LIVE_NO_KEY"\n')
    env = 'env -u XAI_API_KEY WIZARD_DEV_SKILLS=/nonexistent CLAUDEW_STATE_DIR=%s PATH=%s:$PATH' % (
        os.path.join(a.work, 'live-state'), bindir)
    fails = []
    try:
        for size in ([] if a.shared_only else a.sizes.split(',')):
            main_rs = 'fn main() {\n    println!("hello");\n}\n'
            with open(os.path.join(a.cwd, 'main.rs'), 'w') as f:
                f.write(main_rs)
            other = os.path.join(a.work, 'proj2')
            os.makedirs(other, exist_ok=True)
            for f in ('w.txt',):
                if os.path.exists(os.path.join(a.cwd, f)):
                    os.remove(os.path.join(a.cwd, f))
            early = os.path.join(a.work, 'live-home-early')
            early2 = os.path.join(a.work, 'live-home-early2')
            for h in (early, early2):
                shutil.rmtree(h, ignore_errors=True)
                home(h, 'model = "fake-model"\nactive_provider = "fake"\n\n[[providers]]\nname = "fake"\n'
                        'kind = "openai"\nbase_url = "http://127.0.0.1:%d/v1"\nmodel = "fake-model"\n' % port)
            skill = os.path.join(good, '.wizard', 'skills', 'frontend-design')
            os.makedirs(skill, exist_ok=True)
            with open(os.path.join(skill, 'SKILL.md'), 'w') as f:
                f.write('---\nname: frontend-design\ndescription: Build web pages\n---\nMake it look good.\n')
            # the user's own commands: one global, one the project's
            for d, name, body in ((os.path.join(good, '.wizard', 'commands'), 'greet.md', 'Greeting: $ARGUMENTS'),
                                  (os.path.join(a.cwd, '.wizard', 'commands'), 'deploy.md',
                                   'Deploy the service to $ARGUMENTS and say done')):
                os.makedirs(d, exist_ok=True)
                with open(os.path.join(d, name), 'w') as f:
                    f.write(body + '\n')
            for group, h, cwd in (('live', good, a.cwd), ('live-other', good, other),
                                  ('live-resumed', good, a.cwd), ('live-carry', good, a.cwd),
                                  ('live-carry-resume', good, a.cwd), ('live-btw', good, a.cwd),
                                  ('live-slash', good, a.cwd), ('live-early', early, a.cwd),
                                  ('live-early-cmd', early2, a.cwd),
                                  ('live-fail', bad, a.cwd)):
                # the slow start's model list takes 8 s; a fresh home has none cached
                slow = os.path.join(os.path.dirname(log), 'shared-slow-get.txt')
                if group in ('live-early', 'live-early-cmd'):
                    with open(slow, 'w') as f:
                        f.write('8')
                elif os.path.exists(slow):
                    os.remove(slow)
                subprocess.run([sys.executable, os.path.join(HERE, 'drive.py'), '--target', 'look',
                                '--cwd', cwd, '--out', out, '--group', group, '--sizes', size,
                                '--cmd', '%s HOME=%s wizard' % (env, h)], check=True,
                               stdout=subprocess.DEVNULL)
            slow = os.path.join(os.path.dirname(log), 'shared-slow-get.txt')
            if os.path.exists(slow):
                os.remove(slow)
            history = os.path.join(a.work, 'live-state', 'history.jsonl')
            fails += checks(out, size, version, log, a.cwd, history)
            # the next size starts from a fresh history and fresh sessions
            for d in ('live-state',):
                shutil.rmtree(os.path.join(a.work, d), ignore_errors=True)
            shutil.rmtree(os.path.join(good, '.wizard', 'sessions'), ignore_errors=True)
        if a.ref and os.path.exists(os.path.join(a.ref, 'x-badedit-120x40.ansi')):
            shared(a, env, out, port, log)
    finally:
        fake.kill()
        fake.wait()
    for f in fails:
        print('live check failed: ' + f)
    print('live checks: %d failed' % len(fails))
    sys.exit(1 if fails else 0)


def rows_of(path):
    sys.path.insert(0, HERE)
    from cells import parse
    return parse(open(path, encoding='utf-8').read())


def code_markdown(cells):
    """A row's text with runs in the code colour back in backticks."""
    out, code = '', False
    for c, k in cells:
        is_code = str(k[0]) == '153'
        if is_code != code and c.strip():
            out += '`'
            code = is_code
        out += c
    return out.rstrip() + ('`' if code else '')


def shared_data(ref, size):
    """The reference's /btw answer and rejected plan, for the fake server to give back."""
    data = {}
    rows = rows_of(os.path.join(ref, 'x-btw-%s.ansi' % size))
    texts = [''.join(c for c, _ in r).rstrip() for r in rows]
    at = next((i for i, t in enumerate(texts) if t.lstrip().startswith('/btw ')), None)
    end = next((i for i, t in enumerate(texts) if 'to close' in t), None)
    if at is not None and end is not None:
        data['btw'] = '\n'.join(t.strip() for t in texts[at + 1:end] if t.strip())
    # what the model said after the failed edit: the last reply before the done line
    rows = rows_of(os.path.join(ref, 'x-badedit-%s.ansi' % size))
    texts = [''.join(c for c, _ in r).rstrip() for r in rows]
    done = next((i for i in range(len(texts) - 1, -1, -1) if texts[i].startswith('✻ ')), None)
    start = next((i for i in range(done or 0, -1, -1) if texts[i].startswith('● ')), None) if done else None
    if start is not None:
        reply = [code_markdown([x for x in rows[i] if x[0]][2:]) for i in range(start, done) if texts[i].strip()]
        data['badedit-reply'] = ' '.join(x.strip() for x in reply)
    rows = rows_of(os.path.join(ref, 'x-planesc-%s.ansi' % size))
    plan, inside = [], False
    for r in rows:
        t = ''.join(c for c, _ in r).rstrip()
        if t.strip().startswith('╭'):
            inside = True
            continue
        if t.strip().startswith('╰'):
            break
        if inside:
            cells = [x for x in r if x[0]][7:]
            line = code_markdown(cells).rstrip().rstrip('│').rstrip()
            plan.append(line)
    if plan:
        data['plan'] = '\n'.join(plan).strip()
    return data


def seed_history(ref, size, path, cwd):
    texts = [''.join(c for c, _ in r).rstrip() for r in rows_of(os.path.join(ref, 'x-ctrlr-%s.ansi' % size))]
    unit = {'s': 1, 'm': 60, 'h': 3600, 'd': 86400}
    mine = ('Use the Edit tool', '/btw ', '/plan', 'Plan adding the comment')
    now = int(time.time())
    seeded = []
    for t in texts:
        m = re.match(r'^  [❯↑↓ ] (\d+)([smhd]) ago\s+(.*?)\s{2,}[│╭╰]', t + '  │')
        if not m or m.group(3).startswith(mine):
            continue
        text = m.group(3)
        # a cut entry is cut again the same way: make it long enough
        if text.endswith('…'):
            text = text[:-1] + ' and the rest of the prompt carries on past the list'
        seeded.append({'text': text, 'at': now - int(m.group(1)) * unit[m.group(2)] - 5, 'cwd': cwd})
    # the list showed ↑: there were older ones above it
    if seeded:
        oldest = seeded[0]['at']
        seeded = [{'text': 'an older prompt %d' % n, 'at': oldest - 60 * (n + 1), 'cwd': cwd}
                  for n in range(3, 0, -1)] + seeded
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, 'w') as f:
        for e in seeded:
            f.write(json.dumps(e) + '\n')


def shared(a, env, out, port, log):
    """The shared scenes through real wizard, answered with the reference's own data, then
    compared with the reference cell by cell."""
    size = '120x40'
    for name, text in shared_data(a.ref, size).items():
        with open(os.path.join(os.path.dirname(log), 'shared-%s.txt' % name), 'w') as f:
            f.write(text)
    h = os.path.join(a.work, 'live-home-shared')
    # the reference runs at medium effort; so does this wizard
    home(h, 'model = "fake-model"\nactive_provider = "fake"\nreasoning_effort = "medium"\n\n[[providers]]\n'
            'name = "fake"\nkind = "openai"\nbase_url = "http://127.0.0.1:%d/v1"\nmodel = "fake-model"\n' % port)
    xcwd = a.xcwd or a.cwd
    os.makedirs(os.path.join(xcwd, '.wizard'), exist_ok=True)
    with open(os.path.join(xcwd, 'main.rs'), 'w') as f:
        f.write('fn main() {\n    println!("hello");\n}\n')
    # ctrl+r lists every prompt the reference ever sent from here; the earlier ones go into this
    # wizard's history file, at the ages the reference showed, so both lists hold the same
    seed_history(a.ref, size, os.path.join(a.work, 'live-state', 'history.jsonl'), xcwd)
    with open(os.path.join(os.path.dirname(log), 'shared-cwd.txt'), 'w') as f:
        f.write(xcwd)
    for group in ('shared', 'shared-resume'):
        subprocess.run([sys.executable, os.path.join(HERE, 'drive.py'), '--target', 'look',
                        '--cwd', xcwd, '--out', out, '--group', group, '--sizes', size,
                        '--cmd', '%s HOME=%s wizard' % (env, h)], check=True, stdout=subprocess.DEVNULL)


def footer_mode(t):
    rows = [r for r in t.split('\n') if r.strip()]
    m = re.search(r'([a-z][\w ]*?) (?:mode )?on\b', rows[-1]) if rows else None
    return m.group(1) if m else ''


def requests(log, word):
    """What the fake model was offered for each prompt ending in `word`."""
    return [json.loads(l)['tools'] for l in open(log)
            if json.loads(l).get('last_role') == 'user' and json.loads(l).get('user', '').endswith(word)]


def request(log, text):
    """The last request the fake model got for the prompt `text`, or None."""
    hits = [e for e in map(json.loads, open(log))
            if e.get('last_role') == 'user' and e.get('user', '').rstrip().endswith(text)]
    return hits[-1] if hits else None


def ran_in_plan(fails, scene, log, text, model=None):
    """`text` reached the model in plan mode: read tools and exit_plan, and the plan prompt."""
    r = request(log, text)
    check(fails, scene, r is not None, '%r reached the model' % text)
    if r is None:
        return
    check(fails, scene, r['plan'] and 'exit_plan' in r['tools'] and 'read_file' in r['tools'],
          '%r ran in plan mode (plan prompt %s, offered %s)' % (text, r['plan'], r['tools']))
    if model:
        check(fails, scene, r['model'] == model, '%r ran on %s, not %s' % (text, model, r['model']))


def checks(out, size, version, log, cwd, history):
    fails = []
    p = lambda scene: os.path.join(out, '%s-%s.ansi' % (scene, size))
    tag = lambda scene: '%s %s' % (scene, size)

    t = text(p('live-home'))
    check(fails, tag('live-home'), 'Wizard v%s' % version in t,
          'the header names wizard %s, the real binary' % version)

    t = text(p('live-modes-1'))
    check(fails, tag('live-modes'), 'Options can only be changed' not in t,
          'cycling modes prints nothing')
    mode = footer_mode(t)
    s = text(p('live-modes-status'))
    m = re.search(r'Mode:\s+(\S+)', s)
    check(fails, tag('live-modes'), m is not None and m.group(1) == mode,
          'the footer (%r) and /status (%r) agree after a fast cycle' % (mode, m and m.group(1)))
    check(fails, tag('live-modes'), footer_mode(text(p('live-modes-turn'))) == mode,
          'a turn does not move the mode')
    # what the fake model was offered on that turn says which mode wizard was really in
    offered = [json.loads(l) for l in open(log) if '"say ok"' in l][-1]['tools']
    check(fails, tag('live-modes'), (sorted(offered) == ['web_fetch', 'web_search']) == (mode == 'chat'),
          'wizard ran the turn in the mode the footer shows (%r, offered %s)' % (mode, offered))

    t = text(p('live-edit'))
    check(fails, tag('live-edit'), re.search(r'^\s+1\s+fn main\(\) \{', t, re.M),
          'the edit diff shows the line before the change, numbered 1')
    check(fails, tag('live-edit'), re.search(r'^\s+2 -\s+println!\("hello"\);', t, re.M),
          'the removed line is numbered 2')
    check(fails, tag('live-edit'), re.search(r'^\s+2 \+\s+println!\("hi"\);', t, re.M),
          'the added line is numbered 2')
    check(fails, tag('live-edit'), re.search(r'^\s+3\s+\}', t, re.M),
          'the line after the change is shown, numbered 3')

    t = text(p('live-interrupt')).split('❯ please slow')[-1]
    check(fails, tag('live-interrupt'), 'Interrupted' in t, 'the interrupt is shown')
    check(fails, tag('live-interrupt'), not re.search(r'for \d+s · done', t),
          'no done line after an interrupt with no tool call')

    t = text(p('live-queue'))
    check(fails, tag('live-queue'), re.search(r'^❯ queued one', t, re.M), 'the queued prompt is shown')
    check(fails, tag('live-queue'), 'ctrl+enter to send now' in t, 'the send-now hint is shown')
    check(fails, tag('live-queue'), 'Press up to edit queued messages' in t, 'the queue placeholder')
    t = text(p('live-queue-esc'))
    check(fails, tag('live-queue'), re.search(r'^❯ queued one', t, re.M) and 'ok' in t.split('queued one')[-1],
          'after Esc the queued prompt is sent and answered')

    t = text(p('live-badedit')).split('please badedit')[-1]
    check(fails, tag('live-badedit'), re.search(r'Error|not found|not in the file', t),
          'a failed edit shows as failed')
    check(fails, tag('live-badedit'), not re.search(r'^\s+\d+ \+\s*x\s*$', t, re.M),
          'a failed edit draws no diff of a change that never happened')

    t = text(p('live-plan-keep-ask'))
    check(fails, tag('live-plan-keep'), 'Ready to code?' in t and 'Would you like to' in t,
          'a finished plan asks for approval')
    check(fails, tag('live-plan-keep'), '// entry point' in t, 'the plan text is shown')
    check(fails, tag('live-plan-keep'), '❯ 2. No, keep planning' in text(p('live-plan-keep-down')),
          'Down moves to keep planning')
    for scene in ('live-plan-keep', 'live-plan-esc'):
        t = text(p(scene)).split('please plan')[-1]
        check(fails, tag(scene), 'Ready to code?' not in t and 'plan mode on' in t,
              'the answer closes the pane and plan mode stays on')
        check(fails, tag(scene), "User rejected Wizard's plan:" in t and '╭' in t and '// entry point' in t,
              'the rejected plan stays in the transcript, boxed')
        check(fails, tag(scene), 'Used 1 tool' not in t and 'Interrupted' not in t,
              'no tool fold and no interruption for an answered plan')
        check(fails, tag(scene), re.search(r'for \d+s · done', t), 'the turn closes with its done line')
    t = text(p('live-plan-approve')).split('please plan')[-1]
    check(fails, tag('live-plan-approve'), "User approved Wizard's plan" in t and '// entry point' in t,
          'the approved plan stays in the transcript')
    check(fails, tag('live-plan-approve'), 'plan mode on' not in t, 'approving leaves plan mode')

    check(fails, tag('live-plan-tab'), 'plan mode on' in text(p('live-plan-tab-footer')),
          'shift+tab reaches plan mode')
    check(fails, tag('live-plan-tab'), 'Ready to code?' in text(p('live-plan-tab')),
          'plan reached by shift+tab can finish a plan')
    offered = requests(log, 'please plan')[-1]
    check(fails, tag('live-plan-tab'), 'read_file' in offered and 'exit_plan' in offered,
          'plan mode reached by shift+tab offers read tools and exit_plan (%s)' % offered)

    check(fails, tag('live-burst'), footer_mode(text(p('live-burst'))) == 'chat',
          'two shift+tabs from genie end in chat')
    offered = requests(log, 'now write')[-1]
    check(fails, tag('live-burst'), sorted(offered) == ['web_fetch', 'web_search'],
          'a prompt sent in the same burst as the presses ran in chat (offered %s)' % offered)
    check(fails, tag('live-burst'), not os.path.exists(os.path.join(cwd, 'w.txt')),
          'nothing was written by a prompt sent mid-switch')

    during = footer_mode(text(p('live-queue-mode')))
    after = footer_mode(text(p('live-queue-mode-done')))
    check(fails, tag('live-queue-mode'), during == 'plan',
          'the footer shows the pressed mode at once, as the reference does (%r)' % during)
    check(fails, tag('live-queue-mode'), after == 'plan', 'and wizard is in it afterwards (%r)' % after)
    offered = requests(log, 'please write')[-1]
    check(fails, tag('live-queue-mode'), 'exit_plan' in offered and 'read_file' in offered,
          'the queued prompt ran in plan mode, changed before it (offered %s)' % offered)
    check(fails, tag('live-queue-mode'), not os.path.exists(os.path.join(cwd, 'w.txt')),
          'nothing was written from plan mode')

    t = text(p('live-refuse')).split('please refuse')[-1]
    check(fails, tag('live-refuse'), re.search(r'● Write\(refused\.txt\)', t),
          'a refused write_file is drawn as Write, not Update')
    check(fails, tag('live-refuse'), 'not available in this turn' in t and 'Error editing file' not in t,
          'the refusal shows why, not an edit failure')
    check(fails, tag('live-refuse'), not os.path.exists(os.path.join(cwd, 'refused.txt')),
          'nothing was written')

    # plan mode and the model picked survive a model switch, a rewind and a /resume
    t = text(p('live-carry-model')).split('first write')[-1]
    check(fails, tag('live-carry-model'), footer_mode(text(p('live-carry-model'))) == 'plan',
          'the footer still says plan')
    ran_in_plan(fails, tag('live-carry-model'), log, 'first write', 'fake-other')
    check(fails, tag('live-carry-model'), re.search(r'● Write\(w\.txt\)', t) and 'blocked by plan mode' in t,
          'a write plan mode blocked is drawn with its name and the reason')
    for scene, prompt in (('live-carry-rewind', 'rewound write'), ('live-carry-resume', 'resumed write')):
        check(fails, tag(scene), footer_mode(text(p(scene))) == 'plan', 'the footer still says plan')
        ran_in_plan(fails, tag(scene), log, prompt, 'fake-other')
    check(fails, tag('live-btw'), request(log, 'how long wait15') is not None,
          'the side question reached the model')
    check(fails, tag('live-btw'), 'esc to interrupt' not in text(p('live-btw-held')),
          'the side question is not the user\'s turn: nothing to interrupt, so Esc cancels nothing')
    check(fails, tag('live-btw'), footer_mode(text(p('live-btw'))) == 'plan', 'the footer still says plan')
    ran_in_plan(fails, tag('live-btw'), log, 'btw write')
    check(fails, tag('live-plan-text'), request(log, '/plan text write') is None,
          '/plan with text is not handed to wizard\'s /plan, which drops the text')
    ran_in_plan(fails, tag('live-plan-text'), log, 'text write')
    ran_in_plan(fails, tag('live-plan-text'), log, 'refine wait1')
    check(fails, tag('live-plan-text'), footer_mode(text(p('live-plan-text'))) == 'plan',
          '/plan with text in plan mode leaves plan mode on')
    check(fails, tag('live-config'), request(log, '/config') is None, '/config is not sent to the model')
    check(fails, tag('live-config'), 'no settings panel' in text(p('live-config')),
          '/config says where the settings are')
    t = text(p('live-mode'))
    check(fails, tag('live-mode'), 'Select mode' in t and re.search(r'1\.\s+genie', t),
          '/mode opens a picker of wizard\'s modes')

    t = text(p('live-unknown'))
    check(fails, tag('live-unknown'), 'Unknown slash command: zzzfoo' in t and request(log, '/zzzfoo') is None,
          'an unknown command is answered here and spends no turn')
    check(fails, tag('live-rewind-typed'), 'Restore the code and/or conversation' in text(p('live-rewind-typed')),
          'a typed /rewind opens the picker')
    t = text(p('live-model-typed'))
    check(fails, tag('live-model-typed'), 'Set model to fake-model for this session only' in t
          and 'switched to model' not in t, 'a typed /model answers as the picker does')
    t = text(p('live-omakase')).split('❯ /omakase')[-1]
    check(fails, tag('live-omakase'), re.search(r'⎿\s+omakase on', t), '/omakase prints its answer')
    t = text(p('live-omakase-planned'))
    check(fails, tag('live-omakase'), 'Ready to code?' not in t,
          'after /clear omakase is still on: the plan is not put to a review')
    check(fails, tag('live-btw'), 'ctrl+enter to send now' not in text(p('live-btw-held')),
          'a prompt held behind a side question offers no send-now, which only cuts a turn short')

    t = text(p('live-early-cmd'))
    check(fails, tag('live-early-cmd'), 'Unknown slash command' not in t and footer_mode(t) == 'sovereign',
          'a command typed before wizard listed its commands reached wizard and ran (%r)' % footer_mode(t))
    t = re.sub(r'\s+', ' ', text(p('live-login')))
    check(fails, tag('live-login'), 'Unknown slash command' not in t and 'run it in the terminal' in t
          and request(log, '/login xai') is None, 'a command wizard runs but does not list reaches wizard')
    for prompt, want in (('Deploy the service to prod and say done', 'a project command'),
                         ('Greeting: hi there', 'a global command')):
        check(fails, tag('live-custom'), request(log, prompt) is not None,
              '%s is expanded before it reaches the model' % want)
    r = [e for e in map(json.loads, open(log)) if e.get('last_role') == 'user'
         and e.get('user', '').startswith('what is in ')]
    check(fails, tag('live-custom'), r and 'alpha' in r[-1]['user'],
          'an @file reference is read into the prompt')
    check(fails, tag('live-custom'), request(log, '/tmp what is here') is not None,
          'a path is a prompt, not an unknown command')
    check(fails, tag('live-queue-local'), request(log, 'fourth prompt') is not None,
          'a prompt queued behind a local command is sent')
    check(fails, tag('live-queue-local'), 'Options can only be changed' not in text(p('live-queue-local')),
          'a queued /effort waits for the turn to end instead of being refused')
    t = text(p('live-skill'))
    check(fails, tag('live-skill'), 'Unknown slash command' not in t
          and request(log, 'make a landing page') is not None, 'a skill typed as a command reaches the model')
    check(fails, tag('live-queue-clear'), request(log, 'hello again') is not None,
          'a prompt queued behind /clear is sent in the new session')
    t = text(p('live-queue-picker-open'))
    check(fails, tag('live-queue-picker'), 'Select model' in t, 'the queued /model opens its picker')
    r = request(log, 'say hi after model')
    check(fails, tag('live-queue-picker'), r is not None and r['model'] == 'fake-other',
          'the prompt queued after /model waits for the pick and runs on it (%s)' % (r and r['model']))
    t = text(p('live-omakase-custom')).split('❯ /greet please plan')[-1]
    check(fails, tag('live-omakase-custom'), 'plan mode off' not in t,
          'the plan-off line after an approved plan is left out after a custom command too')

    check(fails, tag('live-early'), footer_mode(text(p('live-early'))) == 'chat',
          'two shift+tabs before the session opened end in chat')
    r = request(log, 'early write')
    check(fails, tag('live-early'), r is not None and sorted(r['tools']) == ['web_fetch', 'web_search'],
          'a prompt sent before the session opened ran in the mode pressed before it (offered %s)'
          % (r and r['tools']))

    t = text(p('live-modes-dismissed'))
    rows = re.findall(r'^❯(.*)$', t, re.M)
    check(fails, tag('live-modes'), rows and rows[-1].strip() == '',
          'after a local command the prompt has no placeholder (%r)' % rows[-1:])

    t = text(p('live-rewind'))
    check(fails, tag('live-rewind'),
          'Confirm you want to restore to the point before you sent this message:' in t
          and '1. Restore conversation' in t and '2. Never mind' in t,
          'Enter on a prompt asks to confirm the rewind')
    check(fails, tag('live-rewind'), 'Restore the code and/or conversation' in text(p('live-rewind-back')),
          'Esc goes back to the list')

    t = ''.join(text(p('live-resumed-up%d' % n)) for n in range(1, 13)) + text(p('live-resumed'))
    check(fails, tag('live-resumed'), re.search(r'^\s+2 -\s+println!\("hello"\);', t, re.M)
          and re.search(r'^\s+2 \+\s+println!\("hi"\);', t, re.M),
          'a resumed edit shows real line numbers')
    check(fails, tag('live-resumed'), re.search(r'Error|not found|not in the file', t),
          'a resumed failed edit shows as failed')

    t = text(p('live-resume'))
    check(fails, tag('live-resume'), 'No conversations found in this project.' in t,
          'the picker leaves out the session it is open in')
    t = text(p('live-resumed-list'))
    ages = re.findall(r'(\d+) (second|minute|hour)s? ago · \S+ · [0-9.]+[KMG]?B', t)
    check(fails, tag('live-resumed'), ages, 'sessions show their age, branch and size')
    check(fails, tag('live-resumed'), any(u != 'second' or int(n) >= 3 for n, u in ages),
          'the clock moves: the session is older than a second or two (%s)' % ages)

    t = text(p('live-search'))
    check(fails, tag('live-search'), 'Search prompts · everywhere' in t and 'ctrl+s to scope' in t,
          'history search covers every session and offers ctrl+s')
    check(fails, tag('live-search'), re.search(r'\b[1-9]\d*[smhd] ago', t),
          'earlier prompts are not all 0s ago')

    here = [e['text'] for e in map(json.loads, open(history)) if e.get('cwd') == cwd]
    for n, scene in ((1, 'live-fail-up'), (2, 'live-fail-up2')):
        t = text(p(scene))
        rows = re.findall(r'^❯\s+(.*?)\s*$', t, re.M)
        want = here[-n] if len(here) >= n else None
        check(fails, tag('live-fail'), rows and rows[-1] == want,
              'Up %d recalls this project\'s prompt %r, not another project\'s (%r)' % (n, want, rows[-1:]))
        check(fails, tag('live-fail'), 'History %d/%d' % (len(here) + 1 - n, len(here)) in t,
              'the counter counts this project\'s prompts')
        check(fails, tag('live-fail'), 'Resume a previous' not in t and 'say other' not in t,
              'a recalled slash entry opens no menu')

    t = text(p('live-exit'))
    check(fails, tag('live-exit'), 'wizard --resume' in t, 'the exit names what the user runs')

    t = text(p('live-fail'))
    check(fails, tag('live-fail'), 'wizard setup' in t, 'the setup hint shows')
    check(fails, tag('live-fail'), 'v0.1.0' not in t and 'Wizard v%s' % version in t,
          'the header names the real wizard, not the look crate')
    t = text(p('live-fail-typed'))
    check(fails, tag('live-fail'), not re.search(r'esc to interrupt|…\s*$', t, re.M),
          'nothing looks busy after a failed start')
    meta = open(os.path.join(out, 'live-fail-exit-%s.meta' % size)).read()
    check(fails, tag('live-fail'), 'alt=0' in meta, 'ctrl+c twice exits')
    return fails


if __name__ == '__main__':
    main()
