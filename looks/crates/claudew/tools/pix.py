#!/usr/bin/env python3
"""A real pixel screenshot of a tmux session, for comparing two UIs as a person sees them.

    pix.py SOCK SESSION OUT.png [COLSxROWS]

Hyprland here has no monitor, so this makes a headless output (`hyprctl output create
headless`) sized to the terminal, shows the session in a foot window on it, takes the picture
with grim and removes the output again. foot attaches read-only, so nothing in the session is
disturbed. It needs a running Hyprland session (XDG_RUNTIME_DIR=/run/user/1000), foot, tmux and
grim; without grim on PATH it builds one with `nix build nixpkgs#grim`.

PIX_FONT (default `monospace:size=11`), PIX_CW and PIX_CH (the cell size in pixels that font
gives, 9 and 18) decide the picture's size: columns * PIX_CW + 12 by rows * PIX_CH + 12.
"""
import os, shutil, subprocess, sys, time

OUT = 'WZSHOT'


def grim():
    found = shutil.which('grim')
    if found:
        return found
    path = subprocess.run(['nix', 'build', 'nixpkgs#grim', '--no-link', '--print-out-paths'],
                          capture_output=True, text=True).stdout.strip().split('\n')[-1]
    return path + '/bin/grim'


def shoot(sock, session, png, size='120x40', tmpdir=None):
    font = os.environ.get('PIX_FONT', 'monospace:size=11')
    cw, ch = int(os.environ.get('PIX_CW', 9)), int(os.environ.get('PIX_CH', 18))
    cols, rows = (int(x) for x in size.split('x'))
    run = os.environ.get('XDG_RUNTIME_DIR', '/run/user/1000')
    hy = os.path.join(run, 'hypr')
    sig = sorted(os.listdir(hy), key=lambda n: os.path.getmtime(os.path.join(hy, n)))[-1]
    env = dict(os.environ, XDG_RUNTIME_DIR=run, HYPRLAND_INSTANCE_SIGNATURE=sig, WAYLAND_DISPLAY='wayland-1')
    env.pop('TMUX', None)
    if tmpdir:
        env['TMUX_TMPDIR'] = tmpdir

    def hypr(*a):
        return subprocess.run(['hyprctl', *a], env=env, capture_output=True, text=True).stdout

    tool = grim()
    hypr('output', 'create', 'headless', OUT)
    time.sleep(0.6)
    hypr('keyword', 'monitor', '%s,%dx%d@60,0x0,1' % (OUT, cols * cw + 12, rows * ch + 12))
    time.sleep(0.6)
    foot = subprocess.Popen(['foot', '-W', size, '-f', font, '-T', 'wzshot', '-o', 'pad=6x6',
                             'tmux', '-L', sock, 'attach', '-r', '-t', session], env=env,
                            stderr=subprocess.DEVNULL)
    try:
        time.sleep(2.5)
        r = subprocess.run([tool, '-o', OUT, png], env=env, capture_output=True, text=True)
        if r.returncode:
            print('grim:', r.stderr, file=sys.stderr)
    finally:
        foot.terminate()
        time.sleep(0.4)
        hypr('output', 'remove', OUT)
    return png


if __name__ == '__main__':
    print(shoot(sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else '120x40'))
