//! The graph explorer: the Obsidian-style view over the mesh, minus the
//! drawing.
//!
//! This is S1.1 of the v2 plan, and it is deliberately toolkit-independent.
//! Nothing here links a GUI crate, opens a window or knows what a pixel is,
//! because the two hard parts of an explorer are not the rendering:
//!
//! - [`model`] turns [`crate::mesh`]'s cached peer state into something
//!   drawable, at a clock the caller supplies. It decides liveness once,
//!   honestly, folding the recorded trust decision together with what the
//!   store last observed, so a renderer cannot draw a blocked peer green by
//!   forgetting a match arm. The plan's line is that "a graph that is
//!   beautiful and lies about who is online is worse than a plain one that
//!   does not", and that is a property of this layer, not of the canvas.
//! - [`layout`] is the force model as pure arithmetic: seeded, deterministic,
//!   snapshot-tested, with pinning and a measured per-step cost. A layout
//!   nobody can reproduce is a layout nobody can debug.
//!
//! ```text
//! PeerStore ──build──▶ MeshGraph ──seed/step──▶ Layout ──hit_test──▶ NodeKey
//!                          │                                           │
//!                          └──────────────── inspect ◀─────────────────┘
//! ```
//!
//! # Peer text is attacker-controlled
//!
//! A peer picks its own name, and that name ends up as a label on somebody
//! else's screen. [`crate::mesh::PeerText`] sanitises at the wire boundary;
//! this layer bounds again anyway ([`model::MAX_LABEL_COLUMNS`], in display
//! columns *and* characters, with invisible formatting characters neutralised
//! a second time) and gives every node a discriminator taken from its key
//! fingerprint, so two peers that call themselves the same thing are told
//! apart by something a peer cannot choose. Trust state is a plain field on
//! every node rather than something to derive, because the plan's acceptance
//! bar is that it is unambiguous from the model alone.
//!
//! # What is not here
//!
//! Animation and time scrubbing over delegation history: the plan puts both in
//! 2.1, behind a static graph with a good inspector and correct staleness. The
//! model is shaped to take them (a [`model::MeshGraph`] is a snapshot at an
//! instant, and building one for a past instant is the same call with a
//! different clock) and nothing here pretends they exist yet.

use crate::kernel::{Ctx, Plugin, PluginManifest};

pub mod layout;
pub mod model;

pub use layout::{Layout, LayoutParams, Point, Rect, seed_position, step_positions};
pub use model::{
    CapabilityRef, DisplayName, GraphNode, Inspection, Link, Liveness, MeshGraph, NodeKey, NodeKind,
};

/// `graph`: the mesh explorer's model and layout, and nothing else.
///
/// The first plugin whose `apply` is empty, and the reason is worth writing
/// down rather than leaving as an oversight. `Ctx` registers the four things a
/// plugin can hand the *kernel* — a tool, a slash command, a provider, an
/// event handler — and this plugin hands over none of them. What it produces
/// is a data structure ([`MeshGraph`]) and a solver over it ([`Layout`]),
/// which one screen in `src/native/graph/` constructs by name. There is no
/// registration for "a type another module builds", and inventing a service
/// nobody injects, just to have something in this function, would be
/// decoration.
///
/// It is a plugin in the two senses `docs/plugins.md` says are load-bearing:
/// it is behind a cargo feature and can be left out, and no core module names
/// it. Its consumer is the GUI, which is behind a feature of its own and is
/// itself on the list of things to become a plugin; with `graph` off the
/// window builds and runs without the explorer screen, which is what it does
/// today anyway — `src/native/mod.rs` records that screen as "deferred, not
/// reachable". The manifest is here so `compiled_in()` stays what it claims to
/// be, the one table naming every Rust plugin, and so the kernel can say what
/// this build has.
pub struct GraphPlugin {
    manifest: PluginManifest,
}

impl GraphPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "graph".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "Mesh graph explorer model and layout".to_string(),
                // Pure arithmetic over a `PeerStore` the caller already holds:
                // no socket, no file, no subprocess, no tokens.
                capabilities: Vec::new(),
                optional_deps: Vec::new(),
                // Not in `server` or `pi`: both are headless, and the only
                // consumer is a window.
                profiles: vec!["full".to_string()],
            },
        }
    }
}

impl Default for GraphPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for GraphPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, _ctx: &mut Ctx) -> anyhow::Result<()> {
        Ok(())
    }
}
