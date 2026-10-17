#!/usr/bin/env python3
"""Drive Claude Code or wizard-ui-claude through the comparison scenes in a private tmux server.

Every tmux call goes to the socket `lookclaude` (`tmux -L lookclaude`), never the default
server. Each scene ends in one or more captures: `<out>/<scene>-<WxH>.ansi` is
`capture-pane -e -p` (text with SGR), `<out>/<scene>-<WxH>.meta` the cursor cell and whether
the cursor and the alternate screen are on.
"""
import argparse, os, re, subprocess, sys, time

SOCKET = 'lookclaude'
SIZES = ['80x24', '120x40', '200x50']
# NAME=value pairs passed through to the UI under test (LOOK_ENV, space separated)
EXTRA_ENV = os.environ.get('LOOK_ENV', '').split()

P_BASH = 'Run the bash command `echo hi` and then reply with exactly: done'
P_EDIT = 'Read main.rs, then change hello to hi in it, then reply with exactly: done'
LONG = ('this is a long line of input that keeps going so that it has to wrap at the right edge of the '
        'prompt box more than once at eighty columns and at least once at one hundred and twenty')

# (scene name, steps). A step is ('type', text), ('keys', k1, k2, ...), ('wait', seconds),
# ('idle',), ('until', regex, timeout), ('raw_snap', suffix) (no settling), ('fast_keys', ...)
# (no waiting after), ('quit',) (ctrl+c until the alternate screen closes)
# or ('snap', suffix). ('ref_keys', ...) is sent to the
# reference only.
STATIC = [
    ('home', [('snap', '')]),
    ('shortcuts', [('type', '?'), ('snap', ''), ('keys', 'BSpace')]),
    ('multiline', [('type', 'line one\\'), ('keys', 'Enter'), ('type', 'line two'), ('snap', '')]),
    ('ctrlc', [('fast_keys', 'C-c'), ('raw_snap', '-hint'), ('wait', 2.5), ('snap', '-after')]),
    ('bash-mode', [('type', '!'), ('snap', ''), ('type', 'ls'), ('snap', '-typed'),
                   ('keys', 'BSpace', 'BSpace', 'BSpace')]),
    # the reference has a fourth mode (auto) the backend does not; one more press brings it round
    ('modes', [('wait', 1.0), ('keys', 'BTab'), ('snap', '-1'), ('keys', 'BTab'), ('snap', '-2'),
               ('keys', 'BTab'), ('snap', '-3'), ('ref_keys', 'BTab'), ('wait', 1.0),
               ('snap', '-4')]),
    ('mention', [('type', '@'), ('wait', 0.8), ('snap', ''), ('type', 's'), ('snap', '-query'),
                 ('keys', 'Down'), ('snap', '-selected'), ('keys', 'C-c')]),
    ('slash', [('type', '/'), ('snap', ''), ('keys', 'Down', 'Down'), ('snap', '-down'),
               ('type', 'mo'), ('snap', '-filter'), ('type', 'zzz'), ('snap', '-none'),
               ('keys', 'C-c')]),
    # the panel takes a few seconds to take keys after it is drawn; Tab sent sooner is lost
    ('help', [('type', '/help'), ('snap', '-typed'), ('keys', 'Enter'),
              ('until', r'For more help', 15), ('wait', 3.0), ('snap', ''),
              ('keys', 'Tab'), ('wait', 3.0), ('snap', '-commands'), ('keys', 'Down', 'Down'),
              ('snap', '-commands-down'), ('keys', 'Escape'), ('wait', 0.5)]),
    ('model', [('type', '/model'), ('keys', 'Enter'), ('until', r'Select model', 15),
               ('wait', 0.5), ('snap', ''), ('keys', 'Down'), ('snap', '-down'),
               ('keys', 'Escape'), ('wait', 1.5), ('snap', '-kept')]),
    ('status', [('type', '/status'), ('keys', 'Enter'), ('until', r'Esc to cancel', 15),
                ('wait', 0.8), ('snap', ''), ('keys', 'Escape'), ('wait', 0.5)]),
    ('resume', [('type', '/resume'), ('keys', 'Enter'), ('until', r'Resume session', 15),
                ('wait', 1.0), ('snap', ''), ('keys', 'Down'), ('snap', '-down'),
                ('keys', 'Escape'), ('wait', 0.5)]),
    ('tab', [('type', '/mo'), ('keys', 'Tab'), ('snap', ''), ('keys', 'C-c'), ('wait', 1.0)]),
    ('wrap', [('type', LONG), ('snap', ''), ('keys', 'C-c')]),
    # a double press is two within 800 ms; the hint snap takes longer than that
    ('escape', [('type', 'abc'), ('fast_keys', 'Escape'), ('raw_snap', '-hint'), ('wait', 1.5),
                ('keys', 'Escape', 'Escape'), ('snap', '-cleared')]),
    ('resize', [('resize', 100, 30), ('wait', 1.0), ('snap', '')]),
    ('exit', [('quit',), ('wait', 0.5), ('snap', '')]),
]

TURN = [
    ('turn', [('type', P_BASH), ('snap', '-typed'), ('fast_keys', 'Enter'), ('wait', 0.5),
              ('raw_snap', '-spinner'), ('idle',), ('snap', '-done'), ('keys', 'C-o'), ('wait', 0.6),
              ('snap', '-detail'), ('keys', 'C-o'), ('wait', 0.4)]),
    ('edit', [('type', P_EDIT), ('keys', 'Enter'),
              ('until', r'Do you want to make this edit', 60), ('wait', 0.5), ('snap', '-permission'),
              ('keys', 'Down'), ('snap', '-permission-down'), ('keys', 'Up'),
              ('keys', 'Enter'), ('idle',), ('snap', '-done'), ('keys', 'C-o'), ('wait', 0.6),
              ('snap', '-detail'), ('keys', 'C-o'), ('wait', 0.4)]),
    ('history', [('keys', 'Up'), ('snap', '-1'), ('keys', 'Up'), ('snap', '-2'),
                 ('keys', 'Down', 'Down'), ('snap', '-back')]),
    ('compact', [('type', '/compact'), ('keys', 'Enter'), ('wait', 2), ('idle',), ('snap', '')]),
    ('clear', [('type', '/clear'), ('keys', 'Enter'), ('wait', 1.5), ('snap', '')]),
    ('exit-summary', [('quit',), ('wait', 0.5), ('snap', '')]),
]

P_COUNT = 'Count from 1 to 3000, one number per line. No tools.'
P_PLAN = ('Plan adding the comment "// entry point" as the first line of main.rs. Do not read or '
          'explore anything, I told you all you need. Write a two-line plan and present it for '
          'approval right away.')

# A turn stopped with Esc before any tool ran, and a prompt typed while a turn runs.
P_BADEDIT = ('Use the Edit tool on main.rs to replace the exact text `not in the file` with `x`. Do not '
             'read any file first, and if the edit fails do not retry. Then reply with exactly: done')

# Compared cell by cell with live wizard, which is driven through the same keys and prompts
# against the fake server (live.py feeds it the reference's own answers and plan): a failed
# edit, /btw, /plan, a rejected plan, the rewind list and its confirm step, and ctrl+r.
SHARED = [
    ('x-badedit', [('type', P_BADEDIT), ('keys', 'Enter'), ('wait', 2.0), ('idle',), ('wait', 1.0), ('snap', '')]),
    ('x-btw', [('type', '/btw what is 2+2'), ('keys', 'Enter'), ('until', r'to close', 30),
               ('wait', 3.0), ('snap', ''), ('keys', 'Escape'), ('wait', 1.0)]),
    ('x-plancmd', [('type', '/plan'), ('keys', 'Enter'), ('wait', 2.0), ('snap', '')]),
    ('x-planesc', [('type', P_PLAN), ('keys', 'Enter'), ('until', r'Ready to code', 120), ('wait', 2.0),
                   ('fast_keys', 'Escape'), ('wait', 2.0), ('idle',), ('wait', 1.0), ('snap', '')]),
    ('x-rewind', [('fast_keys', 'Escape'), ('wait', 0.2), ('fast_keys', 'Escape'), ('wait', 2.0), ('snap', ''),
                  # the reference counts /plan as a rewind point, so it takes one more Up to the edit
                  ('keys', 'Up'), ('keys', 'Up'), ('ref_keys', 'Up'), ('keys', 'Enter'), ('wait', 1.5),
                  ('snap', '-confirm'),
                  ('keys', 'Escape'), ('wait', 1.0), ('keys', 'Escape'), ('wait', 1.0)]),
    ('x-ctrlr', [('keys', 'C-r'), ('wait', 1.5), ('snap', ''), ('keys', 'Escape'), ('wait', 1.0)]),
]

# A new session that resumes the one above.
SHARED_RESUME = [
    ('x-resume', [('wait', 1.0), ('type', '/resume'), ('keys', 'Enter'), ('wait', 3.0), ('snap', ''),
                  ('keys', 'Enter'), ('wait', 4.0), ('snap', '-opened')]),
]

TURN3 = [
    ('interrupt', [('type', P_COUNT), ('keys', 'Enter'), ('wait', 2.0), ('fast_keys', 'Escape'),
                   ('wait', 2.0), ('idle',), ('snap', '')]),
    ('queue', [('type', P_COUNT), ('keys', 'Enter'), ('wait', 1.5), ('type', 'queued one'),
               ('fast_keys', 'Enter'), ('wait', 0.8), ('raw_snap', ''), ('fast_keys', 'Escape'),
               ('wait', 1.5), ('raw_snap', '-esc'), ('idle',)]),
    # manual, accept edits, plan
    ('plan', [('keys', 'BTab', 'BTab'), ('wait', 1.0), ('type', P_PLAN), ('keys', 'Enter'),
              ('until', r'Ready to code', 120), ('wait', 2.0), ('snap', ''),
              ('keys', 'Down'), ('snap', '-down'), ('keys', 'Escape'), ('idle',)]),
]

# Real wizard behind the look, answering from tools/fake_openai.py: the last word of each
# prompt picks the fake model's answer. live.py checks these captures.
def plan_round(scene, answer, snaps=True):
    """Ask for a plan, wait for the approval pane, answer it and let the turn end."""
    return (scene, [('type', 'please plan'), ('keys', 'Enter'), ('until', r'Ready to code', 30),
                    ('wait', 0.5), ('snap', '-ask')] + answer + [('idle',), ('wait', 0.5), ('snap', '')])


LIVE = [
    ('live-home', [('wait', 1.0), ('snap', '')]),
    ('live-edit', [('type', 'please edit'), ('keys', 'Enter'), ('idle',), ('wait', 1.0), ('snap', '')]),
    ('live-badedit', [('type', 'please badedit'), ('keys', 'Enter'), ('idle',), ('wait', 1.0), ('snap', '')]),
    ('live-interrupt', [('type', 'please slow'), ('keys', 'Enter'), ('wait', 2.0),
                        ('fast_keys', 'Escape'), ('wait', 1.0), ('idle',), ('snap', '')]),
    ('live-queue', [('type', 'please slow'), ('keys', 'Enter'), ('wait', 1.5), ('type', 'queued one'),
                    ('fast_keys', 'Enter'), ('wait', 1.0), ('raw_snap', ''), ('fast_keys', 'Escape'),
                    ('wait', 1.0), ('idle',), ('snap', '-esc')]),
    # the three answers to a plan, in plan mode reached with /plan: keep planning and Esc stay
    # in plan mode, approving leaves it
    ('live-plan-on', [('type', '/plan'), ('keys', 'Enter'), ('wait', 1.5)]),
    plan_round('live-plan-keep', [('keys', 'Down'), ('snap', '-down'), ('keys', 'Enter')]),
    plan_round('live-plan-esc', [('fast_keys', 'Escape')]),
    plan_round('live-plan-approve', [('keys', 'Enter')]),
    # plan reached by shift+tab: genie, sovereign, chat, then plan on genie
    ('live-plan-tab', [('keys', 'BTab'), ('wait', 1.5), ('keys', 'BTab'), ('wait', 1.5), ('keys', 'BTab'),
                       ('wait', 2.0), ('snap', '-footer'), ('type', 'please plan'), ('keys', 'Enter'),
                       ('until', r'Ready to code', 30), ('wait', 0.5), ('snap', ''), ('fast_keys', 'Escape'),
                       ('idle',), ('keys', 'BTab'), ('wait', 2.0)]),
    # idle: two shift+tabs (to chat) and a prompt sent in the same burst
    ('live-burst', [('fast_keys', 'BTab', 'BTab'), ('send', 'now write'), ('fast_keys', 'Enter'),
                    ('wait', 2.0), ('idle',), ('wait', 1.0), ('snap', '')]),
    # still in chat, which has no write_file: wizard refuses the call
    ('live-refuse', [('type', 'please refuse'), ('keys', 'Enter'), ('idle',), ('wait', 1.0), ('snap', '')]),
    # during a turn: one shift+tab (chat to plan) and a prompt queued behind it
    ('live-queue-mode', [('type', 'please slow'), ('keys', 'Enter'), ('wait', 1.5), ('fast_keys', 'BTab'),
                         ('wait', 0.3), ('type', 'please write'),
                         ('fast_keys', 'Enter'), ('wait', 1.0), ('raw_snap', ''), ('fast_keys', 'Escape'),
                         ('wait', 2.0), ('idle',), ('wait', 1.0), ('snap', '-done')]),
    # two presses well inside a second: the second lands while wizard is still on the first
    ('live-modes', [('fast_keys', 'BTab'), ('wait', 0.3), ('fast_keys', 'BTab'), ('wait', 3.0),
                    ('snap', '-1'), ('type', 'say ok'), ('keys', 'Enter'), ('idle',), ('snap', '-turn'),
                    ('type', '/status'), ('keys', 'Enter'), ('wait', 1.5), ('snap', '-status'),
                    ('keys', 'Escape'), ('wait', 0.5), ('snap', '-dismissed')]),
    ('live-rewind', [('fast_keys', 'Escape'), ('wait', 0.2), ('fast_keys', 'Escape'), ('wait', 1.5),
                     ('snap', '-list'), ('keys', 'Up'), ('keys', 'Enter'), ('wait', 1.0), ('snap', ''),
                     ('keys', 'Escape'), ('wait', 0.8), ('snap', '-back'), ('keys', 'Escape'), ('wait', 0.5)]),
    ('live-resume', [('type', '/resume'), ('keys', 'Enter'), ('wait', 2.0), ('snap', ''),
                     ('keys', 'Escape'), ('wait', 0.5)]),
    ('live-search', [('keys', 'C-r'), ('wait', 1.0), ('snap', ''), ('keys', 'Escape'), ('wait', 0.5)]),
    ('live-exit', [('quit',), ('wait', 0.5), ('snap', '')]),
]

# What the user chose has to survive wizard opening the session again: plan mode and a picked
# model, through a model switch, a rewind and a /resume. Each prompt is sent as fast as it can
# be typed, so it races the reload.
LIVE_CARRY = [
    ('live-carry-model', [('type', '/plan'), ('keys', 'Enter'), ('wait', 1.5),
                          ('type', '/model fake-other'), ('keys', 'Enter'), ('wait', 2.0),
                          ('type', 'first write'), ('keys', 'Enter'), ('idle',), ('wait', 0.5), ('snap', '')]),
    ('live-carry-rewind', [('fast_keys', 'Escape'), ('wait', 0.2), ('fast_keys', 'Escape'),
                           ('until', r'Restore the code and/or conversation', 15),
                           ('keys', 'Up'), ('keys', 'Enter'), ('until', r'Confirm you want to restore', 15),
                           ('fast_keys', 'Enter'), ('until', r'^❯\s+first write\s*$', 15),
                           ('fast_keys', 'C-u'), ('send', 'rewound write'), ('fast_keys', 'Enter'),
                           ('wait', 3.0), ('idle',), ('wait', 1.0), ('snap', '')]),
    ('live-carry-quit', [('quit',), ('wait', 0.5)]),
]
LIVE_CARRY_RESUME = [
    ('live-carry-resume', [('wait', 1.0), ('type', '/plan'), ('keys', 'Enter'), ('wait', 1.5),
                           ('type', '/model fake-other'), ('keys', 'Enter'), ('wait', 2.0),
                           ('type', '/resume'), ('keys', 'Enter'), ('wait', 2.0), ('fast_keys', 'Enter'),
                           ('send', 'resumed write'), ('fast_keys', 'Enter'),
                           ('wait', 3.0), ('idle',), ('wait', 1.0), ('snap', '')]),
    # /plan with text, from chat: plan mode goes on and the text runs in it; again in plan mode,
    # it stays on
    ('live-plan-text', [('fast_keys', 'BTab'), ('wait', 1.5), ('type', '/mode chat'), ('keys', 'Enter'),
                        ('wait', 1.5), ('type', '/plan text write'), ('keys', 'Enter'), ('wait', 2.0), ('idle',),
                        ('type', '/plan refine wait1'), ('keys', 'Enter'), ('wait', 2.0), ('idle',),
                        ('wait', 1.0), ('snap', '')]),
    ('live-config', [('type', '/config'), ('keys', 'Enter'), ('wait', 1.5), ('snap', '')]),
    ('live-mode', [('type', '/mode'), ('keys', 'Enter'), ('wait', 1.0), ('snap', ''),
                   ('keys', 'Escape'), ('wait', 0.5)]),
    # typed forms of what the pickers do, and a command nobody runs
    ('live-unknown', [('type', '/zzzfoo'), ('keys', 'Enter'), ('wait', 1.0), ('snap', '')]),
    ('live-rewind-typed', [('type', '/rewind'), ('keys', 'Enter'), ('wait', 1.0), ('snap', ''),
                           ('keys', 'Escape'), ('wait', 0.5)]),
    ('live-model-typed', [('type', '/model fake-model'), ('keys', 'Enter'), ('wait', 2.0), ('snap', '')]),
    # /omakase says what it did; a new session keeps it, so its plan is not put to a review
    ('live-omakase', [('type', '/omakase'), ('keys', 'Enter'), ('wait', 2.0), ('snap', ''),
                      ('type', '/clear'), ('keys', 'Enter'), ('wait', 2.5),
                      ('type', 'please plan'), ('keys', 'Enter'), ('wait', 4.0), ('snap', '-planned'),
                      ('fast_keys', 'Escape'), ('wait', 1.0), ('idle',)]),
]

# A side question in flight in plan mode, closed, Esc once more, and a prompt sent while it is
# still running: the prompt waits for it, and runs in plan mode.
LIVE_BTW = [
    ('live-btw', [('type', '/plan'), ('keys', 'Enter'), ('wait', 1.5),
                  ('type', '/btw how long wait15'), ('keys', 'Enter'), ('wait', 0.5), ('fast_keys', 'Escape'),
                  ('wait', 0.3), ('fast_keys', 'Escape'), ('wait', 0.3), ('send', 'btw write'),
                  ('fast_keys', 'Enter'), ('wait', 1.0), ('snap', '-held'), ('wait', 22.0), ('idle',),
                  ('wait', 1.0), ('snap', '')]),
]

# A first session that opens slowly (the model list takes 8 s): two shift+tabs and a prompt are
# all in before wizard is ready.
LIVE_EARLY = [
    ('live-early', [('fast_keys', 'BTab', 'BTab'), ('send', 'early write'), ('fast_keys', 'Enter'),
                    ('wait', 9.0), ('idle',), ('wait', 1.0), ('snap', '')]),
]

# A command typed before wizard has listed its commands is held for wizard, not refused.
LIVE_EARLY_CMD = [
    ('live-early-cmd', [('send', '/sovereign'), ('fast_keys', 'Enter'), ('wait', 9.0), ('idle',),
                        ('wait', 1.0), ('snap', '')]),
]

# Commands wizard runs but does not list, the user's own commands, file references and paths,
# a queue that has a local command in it, and the plan-off line after a custom command.
LIVE_SLASH = [
    ('live-login', [('type', '/login xai'), ('keys', 'Enter'), ('wait', 2.0), ('idle',), ('snap', '')]),
    ('live-custom', [('type', '/deploy prod'), ('keys', 'Enter'), ('idle',),
                     ('type', '/greet hi there'), ('keys', 'Enter'), ('idle',),
                     ('type', 'what is in @notes.md'), ('keys', 'Enter'), ('idle',),
                     ('type', '/tmp what is here'), ('keys', 'Enter'), ('idle',), ('wait', 0.5), ('snap', '')]),
    ('live-queue-local', [('type', 'please wait3'), ('keys', 'Enter'), ('wait', 0.5),
                          ('type', '/config'), ('fast_keys', 'Enter'), ('wait', 0.3),
                          ('type', '/effort low'), ('fast_keys', 'Enter'), ('wait', 0.3),
                          ('type', 'fourth prompt'), ('fast_keys', 'Enter'),
                          ('wait', 6.0), ('idle',), ('wait', 1.0), ('snap', '')]),
    # a skill typed as a command goes to the model
    ('live-skill', [('type', '/frontend-design make a landing page'), ('keys', 'Enter'), ('idle',),
                    ('wait', 0.5), ('snap', '')]),
    # /clear queued behind a turn: the session switch may meet wizard's own usage probe
    ('live-queue-clear', [('type', 'please wait3'), ('keys', 'Enter'), ('wait', 0.5),
                          ('send', '/clear'), ('fast_keys', 'Enter'), ('wait', 0.3),
                          ('send', 'hello again'), ('fast_keys', 'Enter'),
                          ('wait', 6.0), ('idle',), ('wait', 1.0), ('snap', '')]),
    # /model queued behind a turn opens the picker; the prompt after it waits for the pick
    ('live-queue-picker', [('type', 'please wait5'), ('keys', 'Enter'), ('wait', 0.5),
                           ('send', '/model'), ('fast_keys', 'Enter'), ('wait', 0.3),
                           ('send', 'say hi after model'), ('fast_keys', 'Enter'),
                           ('wait', 7.0), ('snap', '-open'), ('keys', 'Down'), ('keys', 'Enter'),
                           ('wait', 3.0), ('idle',), ('wait', 1.0), ('snap', '')]),
    ('live-omakase-custom', [('type', '/omakase'), ('keys', 'Enter'), ('wait', 2.0),
                             ('type', '/greet please plan'), ('keys', 'Enter'), ('wait', 4.0), ('idle',),
                             ('wait', 0.5), ('snap', '')]),
]

# Another project sharing the same history file.
LIVE_OTHER = [
    ('live-other', [('type', 'say other'), ('keys', 'Enter'), ('idle',), ('quit',), ('wait', 0.5)]),
]

# A new session that opens the first one with /resume.
LIVE_RESUMED = [
    ('live-resumed', [('wait', 1.0), ('type', '/resume'), ('keys', 'Enter'), ('wait', 2.0), ('snap', '-list'),
                      ('keys', 'Enter'), ('wait', 3.0), ('snap', '')]
                     + [x for n in range(1, 13) for x in (('keys', 'PPage'), ('wait', 0.6), ('snap', '-up%d' % n))]
                     + [('quit',), ('wait', 0.5)]),
]

# Wizard with no working provider: the session never starts. Up walks this project's history.
LIVE_FAIL = [
    ('live-fail', [('wait', 2.0), ('snap', ''), ('keys', 'Up'), ('wait', 0.5), ('snap', '-up'),
                   ('keys', 'Up'), ('wait', 0.5), ('snap', '-up2'),
                   ('keys', 'Escape', 'Escape'), ('wait', 0.5), ('type', 'hello'), ('keys', 'Enter'), ('wait', 1.5),
                   ('snap', '-typed'), ('quit',), ('wait', 0.5), ('snap', '-exit')]),
]

# The light theme, for the scenes that carry the most colour without a model call.
LIGHT = [
    ('light-home', [('snap', '')]),
    ('light-shortcuts', [('type', '?'), ('snap', ''), ('keys', 'BSpace')]),
    ('light-modes', [('wait', 1.0), ('keys', 'BTab'), ('snap', '-1'), ('keys', 'BTab'), ('snap', '-2'),
                     ('keys', 'BTab'), ('ref_keys', 'BTab'), ('wait', 1.0)]),
    ('light-slash', [('type', '/mo'), ('snap', ''), ('keys', 'C-c'), ('wait', 1.0)]),
    ('light-bash', [('type', '!'), ('snap', ''), ('keys', 'BSpace'), ('wait', 0.5)]),
    ('light-model', [('type', '/model'), ('keys', 'Enter'), ('until', r'Select model', 15),
                     ('wait', 0.5), ('snap', ''), ('keys', 'Escape'), ('wait', 1.5), ('snap', '-kept')]),
]

P_MD = ('No tools. Output exactly this markdown and nothing else: a table |file|lines| with rows '
        '|a.rs|3| and |b.rs|120|, then **`src/`** in bold, then [Docs](https://example.com/docs), '
        'then a fenced rust block containing: fn main() { println!("hi"); }')
P_GIT = 'Run this exact bash command with the Bash tool: git init -q && echo ok. Then reply with exactly: done'
P_WRITE = ('Use the Write tool to create w.txt containing the numbers 1 to 30, one per line. '
           'Then reply with exactly: done')

# A markdown reply and scrolling back over it, a shell command and a new file behind
# permission prompts, one turned down with Esc and one allowed.
TURN2 = [
    ('md', [('type', P_MD), ('keys', 'Enter'), ('idle',), ('snap', ''), ('keys', 'PPage'),
            ('wait', 1.0), ('snap', '-pgup'), ('keys', 'C-End'), ('wait', 1.0)]),
    ('bash', [('type', P_GIT), ('keys', 'Enter'), ('until', r'Do you want to proceed', 90),
              ('wait', 1.0), ('snap', '-permission'), ('keys', 'Escape'), ('idle',), ('snap', '-esc')]),
    ('write', [('type', P_WRITE), ('keys', 'Enter'), ('until', r'Do you want to create', 90),
               ('wait', 1.0), ('snap', '-permission'), ('keys', 'Enter'), ('idle',), ('snap', '-done')]),
]

# The look with Wizard's own models, modes and commands, compared by shape with the
# reference scenes of the same name.
WIZARD = [
    ('wz-home', [('snap', '')]),
    ('wz-slash', [('type', '/'), ('snap', ''), ('keys', 'C-c'), ('wait', 1.0)]),
    ('wz-slash-filter', [('type', '/mo'), ('snap', ''), ('keys', 'C-c'), ('wait', 1.0)]),
    ('wz-modes', [('keys', 'BTab'), ('snap', '-1'), ('keys', 'BTab'), ('snap', '-2'),
                  ('keys', 'BTab'), ('snap', '-3'), ('keys', 'BTab'), ('snap', '-4')]),
    # the reference opened help before the model picker, so its transcript had no /model yet
    ('wz-help', [('type', '/help'), ('keys', 'Enter'), ('wait', 0.5), ('keys', 'Tab'),
                 ('snap', '-commands'), ('keys', 'Escape'), ('wait', 0.5)]),
    ('wz-model', [('type', '/model'), ('keys', 'Enter'), ('wait', 0.5), ('snap', ''),
                  ('keys', 'Escape'), ('wait', 0.5)]),
    ('wz-status', [('type', '/status'), ('keys', 'Enter'), ('wait', 0.5), ('snap', ''),
                   ('keys', 'Escape'), ('wait', 0.5)]),
]

GROUPS = {'mention': [s for s in STATIC if s[0] == 'mention'], 'static': STATIC, 'turn': TURN, 'light': LIGHT, 'turn2': TURN2, 'turn3': TURN3,
          'wizard': WIZARD, 'live': LIVE, 'live-other': LIVE_OTHER,
          'live-resumed': LIVE_RESUMED, 'live-fail': LIVE_FAIL, 'shared': SHARED,
          'live-carry': LIVE_CARRY, 'live-carry-resume': LIVE_CARRY_RESUME, 'live-early': LIVE_EARLY,
          'live-btw': LIVE_BTW, 'live-early-cmd': LIVE_EARLY_CMD, 'live-slash': LIVE_SLASH,
          'shared-resume': SHARED_RESUME}


def tmux(*args, check=True):
    env = dict(os.environ)
    env.pop('TMUX', None)
    env['TMUX_TMPDIR'] = os.environ.get('LOOK_TMUX_TMPDIR', '/tmp')
    r = subprocess.run(['tmux', '-L', SOCKET, *args], capture_output=True, text=True, env=env)
    if check and r.returncode != 0:
        raise SystemExit('tmux %s: %s' % (' '.join(args), r.stderr.strip()))
    return r.stdout


class Pane:
    def __init__(self, name, w, h, cwd, cmd):
        self.name = name
        tmux('kill-session', '-t', name, check=False)
        tmux('-f', '/dev/null', 'new-session', '-d', '-s', name, '-x', str(w), '-y', str(h), '-c', cwd,
             'env', '-i', 'HOME=' + os.environ['HOME'], 'PATH=' + os.environ['PATH'],
             'USER=' + os.environ.get('USER', ''), 'LANG=en_US.UTF-8', 'TERM=tmux-256color',
             # what tmux itself sets in a pane; both programs decide on hyperlinks from it
             'TERM_PROGRAM=tmux', 'TERM_PROGRAM_VERSION=3.6a', 'EDITOR=nvim',
             'SHELL=/bin/sh', *EXTRA_ENV, 'sh')
        for opt, val in (('status', 'off'), ('escape-time', '0'), ('focus-events', 'on'),
                         ('mouse', 'on'), ('history-limit', '2000')):
            tmux('set', '-g', opt, val)
        tmux('resize-window', '-t', name, '-x', str(w), '-y', str(h))
        self.send_literal("PS1='$ '; clear; " + cmd)
        self.keys('Enter')

    def changed(self, before, timeout=1.5):
        """Wait until the screen is no longer `before`: a slow UI must take a key before the next."""
        end = time.time() + timeout
        while time.time() < end and self.text() == before:
            time.sleep(0.05)

    def keys(self, *ks):
        for k in ks:
            before = self.text()
            tmux('send-keys', '-t', self.name, k)
            self.changed(before)
            time.sleep(0.12)

    def send_literal(self, text):
        before = self.text()
        tmux('send-keys', '-t', self.name, '-l', text)
        self.changed(before)

    def text(self):
        return tmux('capture-pane', '-t', self.name, '-p')

    def wait_for(self, pattern, timeout):
        end = time.time() + timeout
        while time.time() < end:
            t = self.text()
            if re.search(pattern, t, re.M):
                return t
            time.sleep(0.25)
        raise SystemExit('%s: timed out waiting for %r\n%s' % (self.name, pattern, self.text()))

    def idle(self, timeout=120):
        end = time.time() + timeout
        calm = 0
        while time.time() < end:
            if 'esc to interrupt' in self.text():
                calm = 0
            else:
                calm += 1
                if calm >= 3:
                    return
            time.sleep(0.4)
        raise SystemExit('%s: still busy after %ss' % (self.name, timeout))

    def settle(self, timeout=5.0):
        """Wait until two captures 600 ms apart are the same; a UI may lag behind its input."""
        end = time.time() + timeout
        last = self.text()
        while time.time() < end:
            time.sleep(0.6)
            now = self.text()
            if now == last:
                return
            last = now

    def snap(self, path, settle=True):
        if settle:
            self.settle()
        else:
            time.sleep(0.15)
        with open(path + '.ansi', 'w', encoding='utf-8') as f:
            f.write(tmux('capture-pane', '-t', self.name, '-e', '-p'))
        with open(path + '.meta', 'w') as f:
            f.write(tmux('display', '-p', '-t', self.name,
                         'cursor=#{cursor_x},#{cursor_y} visible=#{cursor_flag} alt=#{alternate_on}'))

    def kill(self):
        tmux('kill-session', '-t', self.name, check=False)


def run(target, cmd, cwd, size, group, out, prefix=''):
    w, h = (int(x) for x in size.split('x'))
    pane = Pane('%s-%s' % (target, size), w, h, cwd, cmd)
    t = pane.wait_for(r'^❯|Yes, I trust this folder|could not start', 40)
    if 'Yes, I trust this folder' in t:
        pane.keys('Down', 'Enter')
        pane.wait_for(r'^❯', 40)
    time.sleep(4.0 if target == 'ref' else 0.5)
    for scene, steps in GROUPS[group]:
        for step in steps:
            op = step[0]
            if op == 'type':
                pane.send_literal(step[1])
                time.sleep(0.35)
            elif op == 'send':
                # literal text without waiting for the screen to take it
                tmux('send-keys', '-t', pane.name, '-l', step[1])
            elif op == 'fast_keys':
                for k in step[1:]:
                    tmux('send-keys', '-t', pane.name, k)
            elif op == 'keys' or (op == 'ref_keys' and target == 'ref'):
                pane.keys(*step[1:])
                time.sleep(0.3)
            elif op == 'quit':
                # ctrl+c twice, again if the UI is still up (a first press may only clear)
                for _ in range(3):
                    pane.keys('C-c')
                    time.sleep(0.2)
                    pane.keys('C-c')
                    end = time.time() + 5
                    while time.time() < end and tmux('display', '-p', '-t', pane.name,
                                                     '#{alternate_on}').strip() == '1':
                        time.sleep(0.25)
                    if tmux('display', '-p', '-t', pane.name, '#{alternate_on}').strip() != '1':
                        break
            elif op == 'resize':
                tmux('resize-window', '-t', pane.name, '-x', str(step[1]), '-y', str(step[2]))
            elif op == 'wait':
                time.sleep(step[1])
            elif op == 'idle':
                pane.idle()
            elif op == 'until':
                pane.wait_for(step[1], step[2])
            elif op == 'raw_snap':
                pane.snap(os.path.join(out, '%s%s%s-%s' % (prefix, scene, step[1], size)), settle=False)
            elif op == 'snap':
                pane.snap(os.path.join(out, '%s%s%s-%s' % (prefix, scene, step[1], size)))
        time.sleep(0.8)
    pane.kill()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--target', choices=['ref', 'look'], required=True)
    ap.add_argument('--cmd', required=True, help='shell command that starts the UI')
    ap.add_argument('--cwd', required=True)
    ap.add_argument('--out', required=True)
    ap.add_argument('--group', choices=sorted(GROUPS), default='static')
    ap.add_argument('--sizes', default=','.join(SIZES))
    ap.add_argument('--before', default='', help='shell command run before each size (reset files)')
    ap.add_argument('--prefix', default='', help='put this in front of every capture name')
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    for size in a.sizes.split(','):
        if a.before:
            subprocess.run(a.before, shell=True, check=True, cwd=a.cwd)
        run(a.target, a.cmd, a.cwd, size, a.group, a.out, a.prefix)
        print('%s %s %s: captured' % (a.target, a.group, size), flush=True)


if __name__ == '__main__':
    main()
