# The GUI

`wizard gui` opens a window. One process, one binary, no webview, no loopback
HTTP, no port, and no JSON round trip per streaming token: the window links the
agent core in process and folds the same `AgentEvent`s into the same
`TranscriptModel` the TUI reads.

It is the only GUI. It carries what the browser GUI carried — settings,
onboarding and OAuth, the git rail and diff pane, the session picker, the
subagent rail, the context meter, the todo checklist, the command palette, the
gate modals, the attachment tray and the image pane — and is past it in one
place: it can answer a command that asks a question (see
[Console](#the-console)). It is short of it in
one: there is **no model picker**, only the settings sheet's model field and
`/model` (see [What was ported, and what was
cut](#what-was-ported-and-what-was-cut)). It is the surface the webview shell
`wizard app` used to be, and S3 deleted that shell in its favour; the browser
GUI itself went afterwards (see [What went with the browser
GUI](#what-went-with-the-browser-gui)).

`--native` is still accepted on `wizard gui` and does nothing. It named the
window back when a plain `wizard gui` served a page instead, and it is written
into every alias and README copy from that period; a hidden no-op flag is a
better answer than a clap error.

It is **off by default**, so it takes its own build. Two ways to get one:

```sh
# from a checkout
cargo build --release --features native
./target/release/wizard gui

# or through install.sh, which places a second binary, `wizard-native`,
# beside `wizard`
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
  | WIZARD_NATIVE=1 WIZARD_BUILD_FROM_SOURCE=1 bash
wizard-native gui
```

`WIZARD_NATIVE=1` on its own fetches `wizard-native-<target>.tar.gz`. Add
`WIZARD_BUILD_FROM_SOURCE=1` to compile the window from the same checkout
instead of downloading it.

A build without the feature refuses `wizard gui` with a message carrying both
of those lines, plus the four ways to drive a machine you are not sitting at:
the TUI over SSH, `wizard -p`, `wizard acp` from an editor, and the Telegram
gateway.

## Why a feature flag

Three reasons, and the third is the one that would be easy to lose:

1. **A terminal user never opens a window.** `wizard -p`, `wizard acp`,
   `wizard gateway` and every CI container run this binary. Linking winit, a font
   stack and a software rasterizer into all of them costs several hundred crates
   of build time for code none of them executes.
2. **Static musl builds.** The default build has no display-server link of any
   kind, which is what keeps `curl | bash` working on a headless box.
3. **The coverage ratchet.** `contrib/check-coverage.sh` measures with default
   features and gates on a line floor. A widget tree can only be exercised
   headlessly up to a point, and putting it inside that ratio would push the
   floor down for the whole codebase. Every line of iced code is behind
   `#[cfg(feature = "native")]` — including everything `src/plugins/gui/tasks.rs` adds
   for the window: the two watcher fields, `tap` / `untap` / `relay`, `attended`,
   and the one line of `handle_event` that relays. That file carries no
   `#[cfg]` of its own beyond its test module; the gating is one level up, on
   the modules themselves — `pub mod gui;` and `pub mod native;` are both
   `#[cfg(feature = "native")]` in `src/plugins/mod.rs`, so a default build
   never compiles either, compiles exactly the program it compiled before, and
   the ratchet keeps measuring the same thing.

### The window is a plugin

Both directories live under `src/plugins/` and the window registers itself with
the kernel instead of being called by name. `src/lib.rs` no longer mentions
either one: the `wizard gui` arm injects an entrypoint under the string `"gui"`
and prints the install instructions when nothing answers. See
[plugins.md](plugins.md) — "the window is a plugin, and it is the first one
that is not a provider" — for what that took. Nothing about the surface itself
changed, and `--features native` builds the same program.

### …and why off-by-default does not mean source-only

The flag is a *build* decision, not a distribution one, and S3 had to answer the
distribution question separately: a flag that is off by default and ships
nothing means `curl | bash` installs a binary that cannot open this window,
which is the failure mode that made `wizard app` a subcommand printing an error
on 100% of installs.

So the release matrix builds it. `.github/workflows/release.yml`'s `native` job
publishes `wizard-native-<target>.tar.gz` for the four gnu and darwin targets —
exactly where the four `wizard-desktop-*` assets used to be — and `install.sh`
with `WIZARD_NATIVE=1` places one as `wizard-native` beside `wizard`. Two
binaries rather than one for one remaining reason: winit reaches X11 and Wayland
through `dlopen`, which a fully static musl binary cannot do, so there is no
musl native asset, and on a host where `wizard` itself is the musl build
replacing it would trade a binary that runs anywhere for one that runs here.
`wizard update` keeps the distinction (`native_assets` in `src/update.rs`): a
binary built this way updates to a native asset and never to the plain one.

What it costs the *default* build is nothing at all — `cargo build` with no
features still links no iced, and CI's `--features native` clippy-and-suite is
what keeps the graphical build from rotting between releases.

## Dependencies

```toml
iced = { version = "0.14", default-features = false, features = [
  "tiny-skia", "x11", "wayland", "canvas", "tokio",
  "image-without-codecs", "highlighter", "lazy", "advanced",
] }
```

Every one of those is chosen against something already in the tree:

| choice | why |
|---|---|
| `default-features = false` + `tiny-skia` | links **no wgpu at all** — not a runtime fallback, an absent dependency. `cargo tree -i wgpu --features native` prints nothing. |
| `image-without-codecs`, not `image` | `image` turns on the codec default set and would undo Wizard's `png, jpeg, gif, webp` selection. |
| `linux-theme-detection` **off** | it is in iced's default set and pulls `mundy`, which runs its D-Bus query on `async-io` — a second, smol-flavoured reactor beside the tokio runtime the agent core owns. The theme comes from `crate::theme` anyway. |
| `markdown` **off** | it pins `pulldown-cmark ^0.12` against Wizard's 0.13, so enabling it links two parsers. `src/plugins/native/widget/markdown.rs` renders through the one Wizard already has. |
| `highlighter` | reaches syntect through `two-face` with `syntect-default-fancy`, byte-identical to Wizard's own selection. One syntect, one regex engine, no oniguruma and therefore no C dependency. |

`cargo tree -d --features native` shows exactly one `syntect`, one `image`, one
`pulldown-cmark`, and no `wgpu`, `naga`, `mundy` or `async-io`.

## Layout

```
src/plugins/native/
├── mod.rs               App, Message, update, view, run(), the screens
├── plugin.rs            the kernel registration: `wizard gui` → run()
├── event.rs             the tokio↔iced bridge, and the executor
├── font.rs              Inter and JetBrains Mono, embedded
├── theme.rs             crate::theme tokens → iced colours
├── command.rs           the `/` menu, routing, and `impl CommandSurface`
├── console.rs           the one gate this window claims
├── settings.rs          the settings sheet, which is also onboarding
├── sidebar.rs           the chat list
├── rail.rs              git, the context meter, subagents, todos
├── pane.rs              the diff, image and subagent panes
├── subagent.rs          one transcript model per concurrent run
├── probe.rs             test-only selectors
├── tests.rs             the end-to-end proofs
├── select/              cross-block text selection
│   ├── block.rs           what gets selected
│   ├── geometry.rs        the cosmic-text bridge
│   ├── cache.rs           shaped paragraphs, across layout passes
│   └── widget.rs          the one widget, and the gesture
└── widget/
    ├── chrome.rs       the label, the row, the quiet action, the one button
    ├── transcript.rs   TranscriptModel → blocks
    ├── markdown.rs     markdown → styled runs
    └── composer.rs     the field, and the send/stop control
```

(`graph/` is the mesh explorer, behind `--features graph` along with the plugin
it draws from. It is **deferred and unreachable in 2.0.0** —
it still compiles and its tests still run, but nothing in the window opens it.
See [graph-explorer.md](graph-explorer.md).)

Every module carries its own header explaining the part of the problem it
answers. Three are worth reading before touching anything: `select/geometry.rs`
(why it reaches past iced's own `Paragraph` trait), `select/cache.rs` (why the
cache is keyed by content and not by index) and `command.rs` (why there is no
second dispatcher).

### Where the look is specified

[`gui-design-spec.md`](gui-design-spec.md) is the design record this window is
drawn from: the canvas, surface and hairline values, the type rules, the radii,
and a pane-by-pane description of what each screen shows. `theme.rs`, `font.rs`,
`widget/chrome.rs` and `widget/markdown.rs` all cite it by name, and for the
chrome values it is the only place they are written down — everything that is a
*semantic* colour comes from `crate::theme`'s token layer instead, the same one
the TUI reads, so a palette change lands on both surfaces at once.

Read it as the brief and the record of where the drawing diverged from it,
not as a description of what is on screen today. It predates this window and
described the browser GUI as well; its *(Native: …)* notes and [What was
ported, and what was cut](#what-was-ported-and-what-was-cut) here are where the
differences are kept, and its closing "Behavior to wire" section is the
original wiring list rather than an outstanding one.

## What is reused rather than rebuilt

The point of S1's "salvage" line was that `src/plugins/gui/` is not web-specific. It is
worth stating what that turned out to mean, because the split is not where a
file list would suggest:

| what | where it lives | what the window adds |
|---|---|---|
| providers, keys, presets, probing, the step limit | `src/plugins/gui/settings.rs` | the sheet |
| OAuth sign-in (xAI, ChatGPT) | `src/plugins/gui/oauth.rs` | two rows and a poll |
| git status, diff, branches, checkout | `src/plugins/gui/git.rs` | the rail and the diff pane |
| sessions, keep-warm eviction, gates | `src/plugins/gui/tasks.rs` | a second *client* |
| every slash command applied to a live agent | `src/plugins/gui/command.rs` | nothing: it submits |
| the chat list: merge, grouping, `2m` | `src/session_registry.rs` | the sidebar |
| every slash command's semantics | `src/commands/` | `impl CommandSurface` |

Three of those moved during Phase 2 and are worth knowing about:

- **The settings view model, the probe and every provider mutation** were inside
  `src/plugins/gui/server.rs`, between an axum extractor and an axum response. They are
  now `src/plugins/gui/settings.rs`'s, and the route handlers are argument shuffling and
  status codes. Nothing about a settings screen is web-specific.
- **The slash-command executor** — `CommandCtx`, `GuiSurface`, `apply_command`
  and everything they reach — is `src/plugins/gui/command.rs` rather than the back half
  of `tasks.rs`. `tasks.rs` was four thousand lines of two separable jobs
  (owning the sessions, and executing a command against one) and is the file
  everything needs; the house rule is to split it rather than grow it, and this
  phase gave it a third caller.
- **The chat list** was half in the server's `list_tasks` and half re-derived in
  the page's JavaScript. Merging the sessions on disk with the heartbeats, grouping
  by workspace and formatting an age are facts about the session store, so they
  are `session_registry::chats`, `group_by_workspace` and `relative_age` — which
  is also what lets the TUI's `/resume` picker share them later.

### Claude Code's sessions are in the same list

`session_registry::claude_chats` lists what Claude Code recorded for a
workspace, as the same `ChatRow` the rest of the picker is made of, and
`ChatRow::origin` says which store a row came out of. It lives beside the merge
for the same reason the merge does: "what sessions exist here" is a fact about
the stores, not about a window, and `wizard resume --claude` needs the identical
answer.

Three properties fall out of that placement, and each is a real constraint:

- **Provenance is on the row, not inferred.** A Wizard id and a Claude session
  id are both uuids; nothing in the string says which is which. `Origin::Claude`
  carries the transcript's path, its leaf, and how many times it forked —
  everything `claude_resume::import` needs — so no surface re-derives them and
  the two cannot drift.
- **Opening one is a different message.** The sidebar emits
  `sidebar::Message::OpenClaude { source, leaf }`, never `Select(id)`, because
  the acts differ: one resumes a file Wizard owns, the other converts a file it
  does not. A single `Select` with a branch further in would have made them the
  same call.
- **It is not on the refresh timer.** Listing parses every transcript in the
  project — tens of megabytes for a repository worked in for months — so the
  sidebar's Claude section is folded shut and reads on open. What rides the
  five-second timer is `session_registry::claude_here`, a directory probe, which
  is also what hides the section entirely on a machine that never installed
  Claude Code.

There is exactly one chain walker (`ClaudeSession::resolve_chain`) and exactly
one importer (`claude_resume::import`); the CLI, the window and the route are
three callers of them. `~/.claude` stays read-only, and that is enforced from
two directions: `claude_session`'s source scan, which fails the build if a write
API is so much as named in that module, and
`reading_a_claude_tree_leaves_every_byte_of_it_alone`, which now drives the
listing and the import as well as the parser and compares the tree byte for
byte afterwards.

## Commands: routing, not a second dispatcher

`src/commands/` owns the table, the parser and `dispatch`, and
`no_surface_hand_rolls_a_handler_that_shadows_the_registry` scans all of `src/`
for a second `match` over `SlashCommand`. It is a text scan, so being behind a
feature flag does not exempt this window from it.

A typed line is therefore routed by the table's own column for `Surface::Gui`:

| column | where it runs |
|---|---|
| `Execution::Agent` | `TaskManager::submit_command` → the task's worker → `dispatch` against `GuiSurface`. The window writes no handler at all. |
| `Execution::Ui` | `dispatch` against `native::command::Native`, whose verbs return `Action`s the app applies. |
| `Execution::Unavailable` | also the window, so `dispatch`'s own refusal — which says what the command *is* — is what the user reads. |

The window reports itself as `Surface::Gui` rather than adding a column of its
own. Every answer in a `Native` column would be a copy of the `gui` one — both
halves run the same agent, offer the same commands and refuse the same three —
and a duplicated column is a column that drifts. The column was the browser
GUI's; when that surface went, this one inherited it.

A custom `.wizard/commands/*.md` command routes as a **message**, because
`preprocess` expands it on the way into the turn on every surface. That needs the
workspace's command list, which is why `route` takes one: without it the parser
answers `unknown command '/deploy'` for every workspace command the menu had just
offered.

## The console

The window declares `ConsoleAccess::Interactive` and claims the console gate.

A browser could not: its user was behind a socket that could drop mid-command,
and a page holding the stdin of a live child in another process is a hung
`apt install` waiting on a tab somebody closed. That was a property of the
boundary, not of graphical surfaces — and it was the most-cited way the browser
GUI degraded against the TUI, because "wizard cannot run anything that prompts"
is a whole class of task.

This window is the process the command runs in, it dies when the agent dies, and
it has a person in front of it. So `TaskManager::attended` builds its tasks with
`ConsoleAccess::Interactive`, `AgentEvent::ConsoleOpened` reaches the window
through the tap, and the composer binds to the child's stdin until
`ConsoleClosed`. While it is bound the composer says which end it is typing into
and offers Ctrl-D — the keystroke as well as the button, which was not true
until `47462b0`: the button read `end input (Ctrl-D)` while nothing anywhere
bound the key. It is in `run`'s `drops` subscription (`src/plugins/native/mod.rs`), and
it is a no-op when no console is claimed rather than a binding that only exists
in one state.

This is the **one** gate the window claims, and the asymmetry is in
`TaskShared::handle_event`: it claims plan and interview gates and parks their
reply channels, and it does nothing at all with a console. Nobody else claims one
for a GUI task, so if the window does not, `ConsoleHost::attended` stays false
and the command dies at its timeout. Claiming is not taking the ticket from the
bookkeeping; it is the only thing that stops a prompting command from timing out.
`TaskManager::with_registry`, the constructor that does not claim a keyboard,
still leaves `ConsoleAccess::None`.

## Attachments: the tray, not the upload

Files reach a turn by being **dropped on the window**. There is no upload,
because there is no boundary to upload across: a dropped path is already a path
the agent can open, and the `verify_attachments` that used to guard the upload
route defended an HTTP boundary that no longer exists anywhere. Images go to `TurnRequest::images` (the vision path),
everything else to `TurnRequest::files`, which the shared `@file` expansion
reads.

There is no file-picker button. A native picker means a new dependency (`rfd`,
which links GTK on Linux) for a gesture that drag-and-drop and `@path` already
cover, and adding a display-server dependency to this crate is the thing the
feature flag exists to avoid.

## Fonts

Inter and JetBrains Mono are embedded from `assets/fonts/`, as variable TTFs.
They are the same subsets the browser GUI served as woff2, decompressed, because
`fontdb` reads sfnt and not woff2; that directory is now the only copy. See
`assets/fonts/README.md`.

`Font::MONOSPACE` is **not** used anywhere the window draws. iced lets
`default_font` replace `Font::DEFAULT` but gives no hook for the generic
monospace family, which resolves through fontconfig to DejaVu Sans Mono — beside
the JetBrains Mono this build bundles. It compiles, it renders, and the only
symptom is that half the literals are the wrong typeface, so
`no_block_the_transcript_produces_uses_the_system_monospace` asserts it instead.

## How events reach the window

The agent emits `AgentEvent` on a bounded channel; `TaskShared` drains it and
hands each event to the one consumer that wants it:

```
Agent ──AgentEvent(256)──▶ TaskShared::handle_event ──▶ relay ──▶ tap ──▶ iced
                                                    └─▶ folds the dashboard row,
                                                        gates, queue, turn result
```

There used to be a second branch here — `Frame ──▶ JSON ──▶ WebSocket`, for the
browser GUI — and `handle_event` genuinely did fan out twice. It went with the
`Frame` enum and `src/plugins/gui/ws.rs` (see *What went with the browser GUI* below),
so what is left is `self.relay(&event)` and the fold beside it.

`TaskShared::tap` is what the window attaches, and it takes the event rather
than a rendering of it: `AgentEvent` is `Clone + Debug`, which is exactly iced's
bound on a `Message`, so it *is* the message — no wrapper enum, no `From` impl,
nothing to keep in step.

The stream side is a hand-rolled `futures_util::stream::poll_fn` over
`UnboundedReceiver::poll_recv`, rather than `tokio-stream`'s wrapper: the
wrapper crate exists to do those four lines.

Unbounded, deliberately. The producer is `handle_event`, which is synchronous and
called from the turn's own drain loop — it cannot await a full queue, and it must
not drop from one either, because a dropped `TextDelta` is a hole in the
conversation nothing later repairs. Back pressure lives upstream, on the agent's
256-deep channel. This is the same choice the browser GUI's socket channel made,
for the same reason.

**The executor is not iced's.** iced's stock tokio executor *is* a
`tokio::runtime::Runtime`: it calls `Runtime::new()` and then `block_on`s the
compositor's creation on it. Inside `wizard::run` that lands on a thread already
inside the process's runtime, where tokio panics. `native::event::Ambient` hands
iced the runtime that already exists — spawns go to its workers, `enter` installs
its context, and `block_on` is the plain futures one, which is correct because the
only future iced passes to it is the software compositor's constructor, which
awaits nothing.

## The selection layer

The kill criterion for this whole workstream: *can a user select text across a
prose paragraph, a code block and a tool row, and copy it.* Stock iced 0.14
cannot — `Selection` lives inside `text_editor` and `text_input`, both
single-buffer, and a drag across three stock widgets is three widgets each seeing
part of a gesture over text none of them shares.

So the transcript's runs stop being widgets. `Selectable` takes a `Vec<Block>`
and owns all of it: layout, hit testing, highlight, clipboard. One widget, one
gesture, and a range that spans three kinds of block is the ordinary case rather
than a special one.

**The escape hatch.** `iced_graphics::text::Paragraph::buffer()` is public and
`cosmic_text` is re-exported. That matters because iced's `Paragraph` trait is
lossy where it counts: `hit_test` returns `Hit::CharOffset(usize)`, built from
the cosmic-text cursor's `index` with its `line` **discarded**. For a paragraph
holding one logical line that is fine. For one holding a hard newline it is wrong
in the worst way — a plausible small number — because `index` is relative to the
buffer line. Every code block has newlines. So does every tool row.
`select/geometry.rs` uses `Buffer::hit` for the full
`Cursor { line, index, affinity }`, and `layout_runs()` for per-visual-line
geometry the trait does not expose at all.

**The trap.** `Buffer::hit` clamps: it answers `Some` for a point far outside the
paragraph. A widget stacking N paragraphs therefore cannot ask each one whether it
was hit — every one says yes and every click resolves to block 0. The vertical
band dispatch happens in `Selectable::anchor_at`, which knows the offsets because
it laid them out; inside a band, the clamping becomes a feature, because a drag
that strays sideways should still select to the end of the line.

**What it does:** character, word and line granularity (single, double and triple
click, with drag-extend at the same granularity), drag in either direction across
any number of blocks, exact per-visual-line highlight including blank lines and
selected line endings, `Ctrl+C` to the standard clipboard, mouse-release to the
X11 primary selection, `Ctrl+A` to select the conversation, `Escape` to clear,
and a content-keyed paragraph cache so an insert mid-transcript costs one reshape
rather than N.

**What it does not do:** autoscroll on a drag that reaches the window edge
(scrolling belongs to the `scrollable` above it, and iced 0.14 gives a child no
way to move an ancestor), and selection across images or collapsed rows, which
are not text.

**Size.** `select/` is **1,188 lines** of implementation and **515** of unit
tests, plus the end-to-end selection tests in `src/plugins/native/tests.rs`. The
renderers that feed it are a further 251 (`widget/transcript.rs`) and 375
(`widget/markdown.rs`) of implementation — 411 and 489 with their own tests.
The spike's estimate for a finished text-only layer was 1,200–1,800 plus ~400
of tests; this lands just under the bottom of that range because autoscroll,
images and folds were cut to Phase 2 rather than because it was cheaper than
expected.

## Gates

Three kinds of request can pause a turn:

| gate | this window |
|---|---|
| `PlanReady` | **answered.** The plan renders above the composer with approve/reject. Reject is two-stage: the first press reveals a feedback field, the second sends it — rejecting with no reason is the common mistake and it costs the agent the whole next turn. |
| `Interview` | **answered.** One field per question, plus Skip. Phase 1 declined these with a notice; the form is what Phase 2 added. |
| `ConsoleOpened` | **claimed.** See [The console](#the-console). |

The plan and interview tickets are still claimed exactly once, inside
`TaskShared`, and answered through `resolve_plan` / `resolve_interview`. The
window never calls `claim()` on either: doing so would take the reply channel out
of the bookkeeping that a turn's end and a disconnect both depend on. The console
gate is the exception and the reason is in `src/plugins/native/console.rs`.

## Testing, and what a headless machine cannot prove

Everything is proven without a display. `iced_test`'s `Simulator` drives a widget
tree with no compositor, and `iced::Renderer` implements `Headless`, so the
software rasterizer produces real frames into memory.

One caveat, found by using it: **`Simulator::simulate` hardcodes
`clipboard::Null`**, so a copy test written on top of it passes whether or not
the copy happened. The selection tests therefore drive `UserInterface` directly
with a recording clipboard.

What the suite does **not** cover:

- **The window itself.** No test opens one and nothing in CI does either — there
  is no Xvfb job anywhere in this repository. Compositor creation, winit event
  delivery, DPI scaling, IME, real font fallback, actual clipboard integration,
  real drag-and-drop and the system browser launch are all outside what the
  suite reaches.

  It has been opened by hand, once. On 2026-08-07 it was run under a virtual X
  display and the screenshots were read, which found **eighteen layout faults
  that no test asserts and no compiler sees**: the graph explorer drew a lone
  node as a screen-filling disc, `18d` rendered as `18c` under a floating
  scrollbar, `remove` and `end input (Ctrl-D)` were laid out at zero width and
  vanished, the diff pane wrapped where it should have scrolled sideways, and
  `settings` drew across the sidebar's divider into the chat title. All are
  fixed (`696d617`…`1030631`), and most of them were one shape, now written on
  the helper that produced them (`chrome::spread`): a `Shrink` child at index 0
  is measured against the whole row and the fixed thing beside it gets the
  remainder, which can be zero, and a control given zero width does not clip —
  it disappears.

  Two things follow. The nineteenth will be found the same way, by hand, and
  macOS and Wayland have never had a window on them at all.
- **A real OAuth round trip.** `SignIn` binds a loopback listener and talks to a
  provider; the sheet's state machine is tested, the flow is not.
- **A real provider probe.** The sheet's rendering of a verdict is tested; the
  probe reaching a provider is not. The route test that used to exercise
  `settings::probe` against a scripted endpoint went with the browser GUI, and
  nothing replaced it — this is a gap the deletion opened, named rather than
  papered over.

### The pixel snapshot

Now that the faces are bundled, a rasterized frame is a function of this
repository rather than of the machine: the same font bytes, the same `wght` axis,
the same tiny-skia rasterizer. `the_bundled_fonts_rasterize_to_a_committed_digest`
draws a prose-and-code fixture at 600×300 into memory and compares a SHA-256 of
the RGBA buffer against `tests/fixtures/native/session.pixels.sha256`.

Three deliberate choices in it:

- A **hash**, not a PNG: the image is 2.7 MB of RGBA and a binary that size in
  git says nothing a reviewer can read.
- The fixture is **Latin-only prose and code**. A `✓` or a `──` would leave the
  two bundled subsets and land in whatever the machine falls back to — which is
  exactly the machine-dependence being removed — so the tool rows are covered by
  the *structural* snapshot instead, which is unchanged from Phase 1 and covers
  glyphs, indent, fill and chrome.
- A missing golden **fails**. A snapshot that seeds itself when absent is a test
  that can only ever pass. Re-bless deliberately with
  `WIZARD_BLESS_SNAPSHOTS=1 cargo test --features native`.

### What Phase 2's tests prove that a reading of the code does not

- **The console really works.** `a_prompting_command_is_answered_from_this_window`
  runs a scripted model that calls `execute` on `read name; echo hello $name`,
  claims the gate the task announces, types a line, and asserts `hello wizard` in
  the tool's own output. Its companion asserts an ordinary `TaskManager` announces
  no console at all, so `attended` cannot be a no-op that passes both.
- **Switching really replaces.**
  `switching_chats_replaces_every_per_chat_fact_and_re_taps_the_feed` fills a chat
  with a subagent run, a context reading, todos and staged files, switches, and
  asserts every one of them is gone and the generation moved. Carrying any of them
  across is another chat's facts under this chat's name, and nothing looks wrong
  while it happens.
- **The feed is identified by what it should be.**
  `the_event_feed_is_identified_by_the_chat_and_the_generation` hashes `Feed`
  directly: the same task twice hashes the same, another task or a bumped
  generation does not, and a second `Arc` to the same task still does.
- **A branched Claude Code session really opens, and really continues.**
  `a_branched_claude_session_opens_from_the_picker_and_continues` builds a
  `~/.claude/projects`-shaped tree from the committed fixtures, lists it through
  the shared listing, draws the whole window, clicks the branched row, imports
  what the click asked for, opens the result, and runs a turn on it against a
  provider that **records what it was sent**. The last part is the one that
  matters: the fixture forks once, and the request body is the only place an
  import that took the wrong branch — or a flat top-to-bottom read that took
  both — is visible. It asserts a tool id from the resumed branch is present and
  one from the abandoned branch is not, then compares the Claude tree byte for
  byte against the snapshot it took at the start.

  The two asynchronous steps in that path (`App::open_claude` and
  `App::open_chat`) have their bodies run by the test rather than by an
  executor, because there is no compositor to drive one. Everything either side
  of them is the product's.

## What went with the browser GUI

S3 was written as "delete the JavaScript GUI". It deleted the *webview shell*
and kept the loopback server for one stated reason — remote access — and that
server has now gone too. Deleted: `gui/assets/` (13 JavaScript modules, a
stylesheet, an HTML shell and two woff2 faces), `src/plugins/gui/server.rs` (the axum
router and every route), `src/plugins/gui/ws.rs` (the per-task WebSocket), the `Frame`
enum and the replay buffer in `src/plugins/gui/tasks.rs`, `src/plugins/gui/transcript.rs` (the
replay projection), `docs/gui-protocol.md`, and the `axum` dependency. The
`--port`, `--no-open` and `--assets` flags went with them.

**Exit criterion 5 of the v2 plan — "Zero `.js` and `.css` in the repo" — now
holds literally.** It had been restated once, to keep the page for remote
access; the restatement is withdrawn and the original criterion stands.

**Remote access is no longer this surface's job.** The claim the page was kept
for — that binding 127.0.0.1 and serving HTML is the only way to reach a
headless box — was true of *graphical* Wizard and only of that. What replaces
it, in the order most people will want:

- **The TUI over SSH.** `ssh box` then `wizard`. It is the surface every feature
  ships to first, it needs no port and no forward, and it is what the page was
  competing with rather than complementing.
- **`wizard -p '<prompt>'`.** One turn, no UI at all, `--output-format json` or
  `stream-json` if something is consuming it. See [headless.md](headless.md).
- **`wizard acp` over the same SSH connection.** An ACP editor on the laptop
  drives the agent on the box, in the editor's own chat panel — which is a
  graphical surface on a machine you are not sitting at, over stdio rather than
  over a port. See [acp.md](acp.md).
- **The Telegram gateway.** `wizard gateway` on the box, an allow-listed chat on
  the phone. See [gateway.md](gateway.md).

What none of those is: the window, on a remote machine. That is still the mesh's
job — a remote session as a peer whose node you trust — and driving a peer's
session is **tier 3 (delegated work)**, which v2 cut. `wizard peers watch` can
*watch* a peer's session today and cannot type into one. The difference from
before is that this gap no longer costs a router, a page and thirteen JavaScript
modules to leave open.

### What went earlier, with the webview shell

- **`wizard app` and `src/desktop.rs`** (1,088 lines, 12 tests), the `desktop`
  feature, and `tao` + `wry` with roughly a hundred crates of GTK3 and WebKitGTK
  bindings behind them. It pointed a system webview at the loopback server: a
  worse version of this window that also needed `libwebkit2gtk` installed, so it
  printed a "no desktop shell" error on every default install. Eight downloads
  across the eight releases that shipped it, against 132 for the plain Linux
  assets of v1.8.0 alone. The four `wizard-desktop-*` release targets became the
  four `wizard-native-*` ones, and WebKitGTK left CI, `install.sh` and the flake.
- **`select_display_backend`**, which forced `GDK_BACKEND=x11` because WebKitGTK
  under GTK 3 computed a negative device pixel ratio on a fractionally scaled
  Wayland output. It was a workaround for GTK 3 having no fractional-scale
  support, and it went with GTK 3: winit handles Wayland fractional scaling, so
  reproducing it here would force this window off the display server it works
  best on. It was the one always-compiled line of that module and had exactly
  one caller.

### What was ported, and what was cut

The four Phase-2 gaps this document listed, answered:

- **The repo chip.** Done. The sidebar's footer opens the list of workspaces
  Wizard already knows about, and picking one moves where `New Chat` opens. It
  is not a directory *browser*: a native picker means linking GTK, which is the
  dependency the feature flag exists to avoid, and `wizard --cwd <path> gui`
  already opens the window anywhere at all. (`--cwd` is a top-level flag, so it
  goes *before* the subcommand; after it, clap refuses the line.)
- **The branch chip.** Done. It expands into the local branches and picking one
  runs `gui::git::checkout` — written for the browser GUI, and by the time that
  surface was deleted this was its only caller. No force and no stash: git's refusal to overwrite an
  uncommitted change is the safety property, so it is surfaced verbatim as a
  transcript notice. Covered by
  `the_branch_chip_checks_out_and_reports_a_refusal`, which asserts the refusal
  as well as the switch.
- **The model picker.** **Cut**, deliberately, and now gone rather than
  pending. `/model <tag>` and the settings sheet reach the same models, so this
  was a convenience gap and not a capability one. The browser's dropdown was fed
  by `GET /api/models`, which probed *every configured provider* over the
  network on every open; that route was deleted with the rest of the server and
  its logic was never lifted into `src/plugins/gui/settings.rs`. Building a picker here
  now means writing the probe fan-out, not moving it.
- **The feature flag.** Answered: a second release artifact. See
  […and why off-by-default does not mean source-only](#and-why-off-by-default-does-not-mean-source-only).

Cut deliberately, and not blocking:

- **A file-picker button.** Drag-and-drop and `@path` cover it; a picker means
  linking GTK. See [Attachments](#attachments-the-tray-not-the-upload).
- **A launcher entry.** `wizard app --install` wrote a `.desktop` file or a
  `Wizard.app` bundle, and that went with `src/desktop.rs`. It was ~400 lines
  and 12 tests of freedesktop and Info.plist generation for a gesture a one-line
  `.desktop` file covers, and it is not a property of the window. If the window
  earns one back it should be `wizard gui --install`, written against this
  surface rather than inherited from the webview's.
- **`GET /api/image`.** A window opens the file. The route is deleted.
- **Upload, and `verify_attachments`.** Both defended an HTTP boundary the
  window does not have. Deleted.
- **The page's markdown renderer.** Not ported: the window has its own
  `pulldown-cmark` pass (`src/plugins/native/widget/markdown.rs`), with the syntax
  highlighting the JS never had. It also renders *less*, and the difference is
  worth knowing before you expect otherwise: the parser is opened with
  `Options::ENABLE_STRIKETHROUGH` and nothing else
  (`src/plugins/native/widget/markdown.rs:138`), so no tables, no footnotes and **no
  math**. The `unicodeit` pipeline that turns LaTeX into unicode is the TUI's
  alone (`latex_to_unicode` in `src/ui/mod.rs`). The JavaScript is deleted.
- **The "MCP manager" S1 lists.** There was no such surface in the browser GUI
  either.

Still open from Phase 1, and still open:

- **Images are named, not drawn, in the transcript.** They *are* drawn in the
  image pane, which is where a person looks at one; inline thumbnails need a
  widget the selection layer can host without leaving a hole in the drag.
- **Collapsible tool rows**, for the same reason and with the same constraint.
- **Right-aligned user bubbles.** `Block` still has no alignment field.
- **Autoscroll during a drag.**
