# jcodew

Wizard with the terminal UI of [jcode](https://github.com/1jehuang/jcode) (MIT). Ships as
`wizard-ui-jcode`; `/ui jcode` in Wizard starts it.

## Run

    cargo run -p jcodew                      # needs `wizard` on PATH, or JCODEW_WIZARD_BIN
    JCODEW_BACKEND=mock cargo run -p jcodew  # scripted backend, no model calls

With the scripted backend, a prompt containing `fake:<name>` plays a scenario: `text`,
`markdown`, `long`, `slow`, `bash`, `bashfail`, `edit`, `read`, `todo`.

## Compare with jcode

    python3 crates/jcodew/tools/compare.py --jcode /path/to/jcode

This drives jcode and the look through the same keys in a private tmux server
(`tmux -L jcodew-cmp`), at 80x24, 120x40 and 200x50, and prints mismatched cells per scene.
jcode runs with a throwaway `HOME` under `/tmp/jcodew-cmp` against `tools/fake_llm.py`, so it
needs no login. That directory must not be inside a git repository, and the captures record
its path, so keep the default. A full run takes about 40 minutes. The
captures land in `tests/reference/` and are what `cargo test -p jcodew` compares against.
Without `--jcode` the look is compared with the captures already there. `--show 20` prints
the differing cells. The script's header lists what is normalised before the diff and why.

## What is not here

jcode features Wizard has no backend for are left out rather than imitated:

- named servers and sessions, the `server:` and `client:` header rows, `/login` and the
  provider login list
- the info widgets that report them (Overview, Runtime, KV cache, usage limits, memory,
  swarm), and the git Changes and Commits widgets
- tokens per second and live token counts on the status line; the turn footer has the
  duration and the turn's token totals
- a one-line intent on tool rows (wizard's tools take no such argument)
- sending into a running turn without stopping it; a prompt typed during a turn is queued,
  and Ctrl+Enter stops the turn and sends
- the session picker's crash banner, groups, message counts and transcript preview; wizard
  lists a session's title, directory and age
- Ctrl+R history search, the entry pulse on a prompt scrolling into view, and the copy badges
- the side panel, diagrams, swarm, memory, voice, favourites and per-route model details,
  centred layout, copy mode and the idle animation
