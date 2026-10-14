# UI skins

Wizard's TUI can wear another coding agent's terminal chrome. Seven ship:
`wizard` (the house look), `codex`, `grok`, `opencode`, `pi`, `jcode` and
`claude`. `wizard`, `codex` and `grok` draw inside this process; `codex` and
`grok` also have a full look, and the other four exist only as one. A full look
is its own binary (`wizard-ui-<name>`, built from the openw repo pinned in
`looks.rev`) that `/ui` restarts into, talking to `wizard acp`. Pick one at
onboarding, change it live with `/ui`, or cycle the **Interface** row in
`/settings`. See [usage.md](usage.md#the-interface) for the user-facing half.

This document is the implementer's half: how the layer is built, and where the
borrowed parts came from.

## What a skin controls

A skin is a static table (`Chrome` in `src/skin/mod.rs`), not a trait object —
switching one is a pointer swap and nothing allocates per frame. It owns:

| | |
|---|---|
| **Blocks** | Per transcript-entry kind (`user`, `assistant`, `thinking`, `tool`, `notice`): the accent column, the left/right pads, the vertical padding, the background tint, and the marker leading the content. `src/skin/layout.rs`. |
| **Tool cards** | The status glyphs, the label grammar (`bash …` or `Ran …`), and the arm the output hangs off (`└`). |
| **Composer** | Framed with rules, a box, or nothing; the prompt glyph. |
| **Welcome** | Which of three home screens. |
| **Status line** | The separator, the busy phrasing, the idle hint. |
| **Spinner** | The frame sequence and its length. |

It does **not** control what Wizard is. The commands, the model, onboarding,
and everything the status line reports (mode, the `ULTRA ×N` multiplier,
background subagents, the context meter) are Wizard's under every skin. The
skin layer is deliberately given no way to add, hide, or rename a command.

Colors come from the skin's own palette (`Skin::companion_theme`, see
[usage.md](usage.md#color)). There is no separate theme setting: picking the
skin picks the palette.

## The block model, and why the order matters

The transcript used to be a flat list of lines with a two-column marker glued
to the front of each *logical* line, wrapped afterwards. A paragraph that
soft-wrapped therefore lost its marker on every row but the first. That is
invisible when the marker is two spaces, and fatal when it is a colored bar
down the side of a block.

So the order is now the one both open upstreams use: **wrap the content to the
content width first, then decorate every row that came out.**

```text
│A│PL│         content          │ PR │
│1│ 2│           flex           │  2 │
```

`decorate` in `src/skin/layout.rs` puts the accent column on every row *including
the vertical padding*, the marker on the first content row and its continuation
on the rest, and carries a tint to the block's full width so it is a rectangle
rather than the ragged shape of its text.

## Background tints

`codex` and `grok` sit blocks on slabs. A slab is only meaningful relative to
the background under it, so `src/skin/blend.rs` computes it as an **alpha over
the terminal's own background** rather than a fixed color: `Tint::Raised` lifts
off a dark terminal and settles onto a light one.

The terminal background comes from `WIZARD_BG` (an `#rrggbb` you set) or from
`COLORFGBG`. **When it cannot be known, nothing is tinted** — that is the
correct answer rather than a fallback, and it is what Codex does too. Wizard's
own `wizard`/`minimal` pairing never asks for a slab in the first place, so the
house look is unaffected either way.

## Attribution

Both borrowed looks come from open source, and code ported from them is
marked at each site. Both are Apache-2.0; Wizard is MIT, and Apache-2.0 permits
this with attribution, which is what this section is.

The attribution lives here and in `NOTICE`, not on the screen. A borrowed skin
is meant to be indistinguishable from the product it borrows from — the only
things on it that are Wizard's are its name, its commands, and the state its
own backend reports — so a credit line rendered into the welcome screen would
be one more row upstream does not have.

### OpenAI Codex — <https://github.com/openai/codex> (Apache-2.0)

Ported from `codex-rs/tui/src/`:

| Upstream | Here |
|---|---|
| `color.rs` — `blend`, `is_light` | `src/skin/blend.rs` |
| `style.rs` — `user_message_bg_rgb`'s alphas (white at 0.12 over dark, black at 0.04 over light) | `src/skin/blend.rs::Tint` |
| `shimmer.rs` — the sweeping band: raised cosine, half-width 5, 10 columns of lead-in/out, and the DIM/normal/BOLD fallback | `src/skin/motion.rs::shimmer` |
| `render/line_utils.rs` — `prefix_lines`, and the wrap-then-prefix order | `src/ui/mod.rs::prefix_rows` |
| `exec_cell/render.rs` — the `  └ ` / `    ` output arm and the head/tail elision | `src/ui/mod.rs::tool_card_lines` |
| `bottom_pane/chat_composer.rs` — the unframed composer, `LIVE_PREFIX_COLS = 2`, the `›` prompt (bold *and* dim) | `ComposerFrame::Bare` |
| `status_indicator_widget.rs` — `Working (… • esc to interrupt)` and its punctuation | `BusyStyle::Working` |

### xAI Grok Build — <https://github.com/xai-org/grok-build> (Apache-2.0)

Ported from `crates/codegen/`:

| Upstream | Here |
|---|---|
| `xai-grok-pager/src/scrollback/layout.rs` — the accent/pad/content/pad column structure and its widths (left 2, right 2 — the doc comment there says 1 and is stale) | `src/skin/layout.rs` |
| `xai-grok-pager/src/scrollback/scrollback_pane.rs` — the `┃` accent column painted down a block's full height | `src/skin/layout.rs::decorate` |
| `xai-grok-pager-render/src/theme/tokyonight.rs` — `wave_brightness`, `pulse_brightness` | `src/skin/motion.rs::wave`, `::pulse` |
| `theme/groknight.rs` — the neutral-gray base with violet/steel accents | `assets/themes/grok.toml` (approximated in xterm-256) |

Two deliberate divergences from Grok Build, both for the same reason — the
house rule that meaning never rests on hue alone, which is what keeps the TUI
readable at 16 colors and under `NO_COLOR`:

- Upstream carries tool status in color alone (the rail and a `◆` bullet).
  Wizard keeps a distinct glyph for **failure** and only for failure.
- Upstream tints tool-output panels per line. Wizard leaves them untinted,
  since the terminal background is often unknown and a half-applied panel is
  worse than none.

### A note on closed-source agents

No code from a closed-source TUI is copied here or in openw. The `claude` look
is built from its observable screen alone: openw's `crates/claudew` drives the
real program and the look through the same keys in a private tmux server and
diffs the two cell by cell (`crates/claudew/tools/compare.sh`). Nothing from
that program's install is read, copied or redistributed.

Product names and trademarks belong to their owners. The skins are an homage,
and each one says whose look it wears on its own home screen.
