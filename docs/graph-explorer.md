# The graph explorer

> **Deferred — not in 2.0.0.** This screen is compiled and tested but there is
> no way to open it: the window has no mesh button, no `Screen::Mesh` and no
> `Message::Graph`. It was too unfinished to ship, and it is held for a later
> release. Everything below describes what it does when it is wired back in —
> `src/plugins/native/graph/mod.rs` lists the four seams that do that. The mesh itself
> is unaffected: `wizard peers` works, and [mesh.md](mesh.md) is current.

The mesh, drawn. One screen in the native GUI (`--features native`) that answers
three questions about the peers this machine knows about: **who is there**,
**what is this machine's decision about them**, and — the one everything else is
subordinate to — **who is actually up right now**.

It is deliberately not a pretty picture with a network behind it. Obsidian's
graph view is years of polish over a local file set with no network, no trust
states and no liveness; this one has all three, and the rule it is built to is:

> A graph that is beautiful and lies about who is online is worse than a plain
> one that does not.

## What is on screen

```
┌──────────────────────────────────────────────────────────────┬───────────────┐
│ 1 here  12 live  6 stale  4 unseen  3 unreachable   fit  ⟳   │  capability   │
├──────────────────────────────────────────────────────────────┤   filter      │
│                                                              ├───────────────┤
│                  ●───────○      the canvas                   │               │
│                  │        \                                  │   inspector   │
│                  ◉─────────●                                 │               │
│                                                              │               │
└──────────────────────────────────────────────────────────────┴───────────────┘
```

- **The header** counts every liveness state, coloured by the same function the
  canvas colours the dots with, so a swatch cannot drift from what it explains.
  `fit` puts the camera back on the whole graph; `refresh` re-reads the peer
  store.
- **The canvas** is the force-directed graph: pan by dragging the background,
  zoom with the wheel (the point under the pointer stays put), click a node to
  select it, drag a node to pin it.
- **The capability filter** lists every model, tool, skill and subagent anybody
  advertises, with how many nodes advertise it. Choosing one pushes everybody
  else back; it never removes them, because "who does *not* have this model" is
  the more interesting half of the question and because removing nodes would
  reflow the layout under your hands.
- **The inspector** is one node in words: name, liveness and staleness, trust,
  what it claims it will do, its address and fingerprint (both copyable), its
  capabilities grouped by kind, the sessions it is running and the peers it
  introduced — and, only where there is trust to take away, **revoke**.

## How a node is drawn

Three channels, and only one of them claims the node is reachable.

| channel | says | drawn as |
|---|---|---|
| **interior** | is it up | filled with the liveness colour, or hollow onto the canvas |
| **rim** | *which* state it is in | the liveness colour, always |
| **rim weight, and a bar** | the recorded trust decision | thick / thin, struck through when blocked |

Splitting "up" from "which state" is what makes the picture survive a
monochrome theme. The default theme (`minimal`) is written entirely in ANSI-16
names so it survives SSH, and in it `error`, `warning` and `accent` are all
white — so a design where liveness is only a hue would draw a stale peer and a
blocked one identically. Filled against hollow is not a hue.

| liveness | means | token |
|---|---|---|
| `here` | this machine | `accent` |
| `live` | heard from inside the freshness window, and contactable | `success` |
| `stale` | heard from once, not recently | `warning` |
| `unseen` | in the store, never heard from — a pasted address that has not answered | `faint` |
| `unreachable` | blocked; this machine will not contact it, whatever the timestamps say | `error` |

**Trust never implies liveness.** A peer a human trusted, that is not answering,
draws exactly as un-live as a stranger that is not answering; all its trust buys
it is a heavier rim. This is enforced rather than asserted: `Liveness::is_live()`
is consulted in exactly one place (`src/plugins/native/graph/paint.rs`,
`node_paint`), and `nothing_reads_as_up_without_is_live` walks every combination
of trust, liveness and node kind the model can produce, in both shipped themes,
and requires that the interior is the canvas colour **iff** the node is not up.

## Edges

| edge | meaning | drawn |
|---|---|---|
| peer | this machine holds that node in its store | yes |
| observed | a peer's peer, learned rather than pasted | yes, quietly |
| delegation | work was sent along this edge, weighted by count | modelled, never drawn today — see below |
| session | a session stream running on that node | yes |
| capability | a shared model, tool, skill or subagent | **no** — it clusters the layout only |

The delegation edge cannot appear in 2.0, and it is listed for what the model
holds rather than for what you will see. Delegated work — sending a task to a
peer and having it run — was cut from this release: there is no task frame on
the wire, and nothing in the shipping code path calls `record_delegation`, so
every peer's count is zero and the edge is never drawn. The same goes for the
inspector's `delegations` row, which is written to appear only above zero. Both
stay in the model because the weight is what the layout would use if the work
lands in a later release, not because anything produces one now.

Capability edges are never lines. On a mesh where fifty peers all offer
`read_file`, drawing them would be a solid block conveying nothing; pulling on
them puts the peers that share a model near each other, which is the part worth
having.

An edge with either end not up is drawn at a third of its opacity. Still drawn:
that is how a pasted address that never answered is visible at all.

## Interaction

| gesture | effect |
|---|---|
| click a node | select it; the inspector follows |
| click the background | clear the selection |
| drag a node | **pin** it there. The rest of the mesh rearranges around the decision |
| drag the background | pan |
| wheel / trackpad | zoom, anchored under the pointer |
| `release pin` in the inspector | let a pinned node move again |
| `fit` | re-fit the camera, and let it track the graph again |

The camera fits the graph when the screen opens and keeps re-fitting while the
simulation is still moving — the bounds on the first frame are the bounds of a
seeded scatter, not of a graph. The first pan, zoom or drag ends that for good:
from then on the camera is yours and nothing moves it without being asked.

A fit shrinks; it never magnifies. Fitting to fill would mean a machine with no
peers gets one node blown up to the height of the canvas, which is what this
screen used to do — the first thing a new install saw was a single white disc
two hundred pixels across with its own label drawn inside it. So the automatic
fit stops at natural size, where a node is its own handful of pixels and linked
nodes sit a readable distance apart, and a small graph simply sits in the middle
of a larger canvas. The wheel is not capped the same way: zooming in past that
is a thing you can ask for, just not a thing the camera does on its own.

## A still graph costs nothing

The layout is stepped from a 60Hz subscription that is **dropped**, not
throttled, once the simulation's total kinetic energy falls under
`view::SETTLE_ENERGY`. A settled explorer schedules no timer, wakes no thread
and redraws nothing.

Energy alone is not sufficient, and the gap is silent: a node that has just been
unpinned has zero velocity and a large force, so a screen gated on energy alone
would freeze with your release un-simulated. Any disturbance therefore owes the
simulation a few steps regardless — the energy gate is what makes it stop, and
the wake is what makes it start.

Fifty synthetic peers settle in about 500 steps from a cold seed.

## Revoking

The one destructive control, and the plan calls it the acceptance bar most
likely to be faked. It calls `Mesh::set_trust(&id, Trust::Blocked)`, which is
three things that must not come apart: the decision is recorded, every live
subscription in **both** directions is severed through the transport, and the
store is written to disk. Only then is the graph rebuilt and redrawn.

The order matters. Snapshotting first, or redrawing from the cached graph, would
draw a peer whose stream has already ended as though it were still up.

`tests/graph_explorer.rs` is the proof, and it is an integration test on purpose:
a unit test can assert `Trust::Blocked` was written and still pass against an
implementation that never touches the transport. That test opens a real
subscription, publishes through it to show it was carrying traffic, presses the
same function the button presses, and then requires that the stream ends, that
the transport has no subscriber left, and that the redrawn graph paints the peer
hollow.

Revoking is not forgetting. A blocked peer stays in the store and stays on the
graph, drawn unreachable and struck through, because "this peer is knocking and
I am refusing it" is worth being able to see.

## What is not here

Animation and time scrubbing over delegation history. Both are 2.1, behind a
static graph with a good inspector and correct staleness indication. The model
is shaped to take them — a `MeshGraph` is a snapshot at an instant, and building
one for a past instant is the same call with a different clock — and nothing
pretends they exist yet.

## Where the code is

| file | what it owns |
|---|---|
| `src/graph/model.rs` | the mesh as drawable data; liveness decided once, honestly |
| `src/graph/layout.rs` | the force model as deterministic arithmetic, with pinning and hit testing |
| `src/plugins/native/graph/paint.rs` | liveness and trust as ink, and the single `is_live()` gate |
| `src/plugins/native/graph/viewport.rs` | the one transform between the layout's world and the canvas's pixels |
| `src/plugins/native/graph/view.rs` | every decision a gesture implies, in a struct with no widget in it |
| `src/plugins/native/graph/canvas.rs` | the `canvas::Program`: edges, nodes, labels, gestures |
| `src/plugins/native/graph/inspector.rs` | one node, in words |
| `src/plugins/native/graph/mod.rs` | the screen: state, messages, the settle subscription, revoke |

The split between the last four and the first two is the point: nothing in
`src/graph/` links a GUI crate, and nothing in `src/plugins/native/graph/` decides
anything about the mesh. See also [`mesh.md`](mesh.md) and
| `src/plugins/graph/model.rs` | the mesh as drawable data; liveness decided once, honestly |
| `src/plugins/graph/layout.rs` | the force model as deterministic arithmetic, with pinning and hit testing |
| `src/native/graph/paint.rs` | liveness and trust as ink, and the single `is_live()` gate |
| `src/native/graph/viewport.rs` | the one transform between the layout's world and the canvas's pixels |
| `src/native/graph/view.rs` | every decision a gesture implies, in a struct with no widget in it |
| `src/native/graph/canvas.rs` | the `canvas::Program`: edges, nodes, labels, gestures |
| `src/native/graph/inspector.rs` | one node, in words |
| `src/native/graph/mod.rs` | the screen: state, messages, the settle subscription, revoke |

The split between the last four and the first two is the point: nothing in
`src/plugins/graph/` links a GUI crate, and nothing in `src/native/graph/`
decides anything about the mesh.

That split is now a cargo feature as well as a directory. The model and the
layout are the `graph` plugin (`--features graph`, on by default) and the six
files under `src/native/graph/` are gated on it in step, so a window built
without it is a window with no explorer — which is the window that ships today,
since the screen is not yet reachable from the UI. See
[`plugins.md`](plugins.md) for why a plugin that registers nothing through
`Ctx` is still a plugin, and also [`mesh.md`](mesh.md) and
[`native-gui.md`](native-gui.md).
