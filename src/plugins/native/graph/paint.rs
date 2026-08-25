//! Liveness and trust as ink, and the one gate that keeps it honest.
//!
//! The plan's rule for this screen is a single sentence: *a graph that is
//! beautiful and lies about who is online is worse than a plain one that does
//! not.* [`Liveness::is_live`] is the model's answer to "may this be drawn as
//! up", and this module exists so that answer is consulted in exactly one
//! place. [`node_paint`] branches on it once; everything below the branch is
//! chosen inside one of the two arms; and there is no third arm, no `else if
//! trusted`, and no shortcut for the local node.
//!
//! # Three channels, and only one of them is "up"
//!
//! | channel | says | drawn as |
//! |---|---|---|
//! | interior | **is it up** | filled with the liveness colour, or hollow onto the canvas |
//! | rim | *which* state | the liveness colour, always |
//! | rim weight and bar | the recorded trust decision | thick / thin / barred |
//!
//! Splitting "up" from "which state" is what makes the picture survive a
//! monochrome theme. `assets/themes/minimal.toml` maps `error`, `warning` and
//! `accent` all to white on purpose — it is written in ANSI-16 names so it
//! survives SSH — so a design where liveness is *only* a hue is a design where
//! stale and unreachable are the same dot under the default theme. Filled
//! against hollow is not a hue, and it is the channel that carries the claim
//! that matters.
//!
//! Trust is on a channel of its own for the reason the module header of
//! [`crate::plugins::graph::model`] gives: trust must never imply liveness. A trusted
//! peer that has not answered is drawn hollow like any other silent node, and
//! the only thing its trust buys it is a heavier rim.
//!
//! # Why the interior is the canvas colour and not "no fill"
//!
//! A hollow node has edges passing behind it. Painting the interior with the
//! canvas colour punches those edges out, so the rim reads as a rim rather
//! than as a ring drawn over a line. It also gives the honesty test something
//! to assert on that is not a hue: `interior == canvas` **iff** the node is
//! not up, at every theme.

use iced::Color;

use crate::mesh::{EdgeKind, Trust};
use crate::plugins::native::theme::Palette;
use crate::plugins::graph::{GraphNode, Liveness};
use crate::theme::Token;

/// Rim weight, in world units, for a peer whose trust is merely recorded.
const RIM_THIN: f64 = 1.6;
/// Rim weight for a peer a human has trusted, and for this machine.
const RIM_THICK: f64 = 3.2;

/// How much of a link's colour survives when one of its ends is not up.
///
/// An edge to a node nobody has heard from is still a fact worth drawing — it
/// is how a pasted address that never answered is visible at all — but drawing
/// it at full strength puts the same weight on a live stream and a dead one.
const DORMANT_LINK_ALPHA: f32 = 0.34;

/// How much of anything survives being filtered out.
const FILTERED_ALPHA: f32 = 0.16;

/// How far a halo reaches past the rim of a node that is up, in world units.
const HALO_REACH: f64 = 4.5;

/// Alpha of that halo.
const HALO_ALPHA: f32 = 0.28;

/// How one node is inked.
///
/// Built only by [`node_paint`]. The fields are the three channels the module
/// header describes, plus the two derived booleans a test can hold to
/// [`Liveness::is_live`] without knowing anything about colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodePaint {
    /// What the disc is filled with.
    pub interior: Color,
    /// The rim, which is the liveness colour whether or not the node is up.
    pub rim: Color,
    /// Rim weight in world units, from the trust decision alone.
    pub rim_width: f64,
    /// The label beside the dot.
    pub label: Color,
    /// A soft ring outside the rim, drawn only around a node that is up.
    pub halo: Option<Color>,
    /// Whether the disc is filled with its liveness colour rather than with the
    /// canvas. **The one bit that claims the node is reachable.**
    pub solid: bool,
    /// Whether to strike the node through: this machine will not contact it.
    pub barred: bool,
}

/// The colour of one liveness state.
///
/// Advisory, and deliberately so: it names *which* state, never *whether* the
/// node is up. Two themes are free to give two states the same hue (`minimal`
/// does) without making the picture ambiguous, because the interior channel is
/// carrying the claim.
pub fn liveness_color(liveness: Liveness, palette: &Palette) -> Color {
    palette.color(match liveness {
        // This machine, which is up by definition and is the one node the
        // operator is looking out from. The accent is what the rest of the UI
        // uses for "you are here".
        Liveness::Here => Token::Accent,
        Liveness::Live => Token::Success,
        Liveness::Stale => Token::Warning,
        Liveness::Unseen => Token::Faint,
        Liveness::Unreachable => Token::Error,
    })
}

/// The rim weight a trust decision earns. Trust's whole visual budget.
fn rim_width(trust: Trust) -> f64 {
    if trust.may_send_work() {
        RIM_THICK
    } else {
        RIM_THIN
    }
}

/// How one node is drawn.
///
/// **The only gate.** `is_live()` is asked once, here, and every field that
/// could read as "up" — the interior, the halo, the `solid` flag — is decided
/// inside that branch. Nothing about [`Trust`] reaches any of them: a trusted
/// peer that is unreachable draws unreachable.
pub fn node_paint(node: &GraphNode, palette: &Palette) -> NodePaint {
    let state = liveness_color(node.liveness, palette);
    // The single predicate. See the module header, and
    // `nothing_reads_as_up_without_is_live`.
    let up = node.liveness.is_live();
    NodePaint {
        interior: if up { state } else { palette.canvas },
        rim: state,
        rim_width: rim_width(node.trust),
        label: if up {
            palette.color(Token::Text)
        } else {
            palette.color(Token::Muted)
        },
        halo: up.then(|| fade(state, HALO_ALPHA)),
        solid: up,
        // Not a liveness question: a blocked peer that is announcing every
        // second is still one this machine refuses to contact, and the bar says
        // so beside the rim that says it is unreachable.
        barred: !node.trust.may_contact(),
    }
}

/// How one link is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkPaint {
    pub color: Color,
    /// Width in world units.
    pub width: f64,
}

/// How a link between two nodes is drawn.
///
/// `both_live` is the caller's, taken from the two endpoints' own
/// [`Liveness::is_live`], because a link is not a thing with a liveness of its
/// own: what it can honestly say is whether both ends of it are up.
pub fn link_paint(kind: EdgeKind, weight: u32, both_live: bool, palette: &Palette) -> LinkPaint {
    let (token, width) = match kind {
        // The structural edge: this machine holds that node in its store.
        EdgeKind::Peer => (Token::Border, 1.2),
        // Provenance, not topology. Quietest of the four.
        EdgeKind::Observed => (Token::Faint, 0.9),
        // Work actually flowed, and how much of it.
        EdgeKind::Delegation => (Token::Accent, 1.4 + (weight.min(8) as f64) * 0.22),
        // A stream that is running right now.
        EdgeKind::Session => (Token::ToolRunning, 1.6),
        // Never drawn; `Link::is_drawn` filters it out before this is reached.
        // Answered anyway rather than panicking, because this is a draw path.
        EdgeKind::Capability => (Token::Faint, 0.6),
    };
    LinkPaint {
        color: fade(
            palette.color(token),
            if both_live { 1.0 } else { DORMANT_LINK_ALPHA },
        ),
        width,
    }
}

/// The same colour, at `alpha` of its opacity.
pub fn fade(color: Color, alpha: f32) -> Color {
    Color {
        a: color.a * alpha,
        ..color
    }
}

/// The same colour, pushed back because a capability filter excluded it.
///
/// Excluded and not hidden: a filter that removed nodes would reflow the
/// layout under the operator's hands and would also hide the answer to "who
/// does *not* have this model", which is the more interesting half of the
/// question.
pub fn filtered(color: Color) -> Color {
    fade(color, FILTERED_ALPHA)
}

/// How far a live node's halo reaches past its rim, in world units.
pub fn halo_reach() -> f64 {
    HALO_REACH
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;
    use crate::mesh::{Capability, Identity, Node, PeerStore, PeerText};
    use crate::plugins::graph::{MeshGraph, NodeKey, NodeKind};

    fn palette(name: &str) -> Palette {
        match name {
            "minimal" => Palette::from_theme(&crate::theme::minimal()),
            other => Palette::from_theme(&crate::theme::load(other).expect("a built-in theme")),
        }
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    /// Every node the model can produce, at every trust and every liveness,
    /// built through `MeshGraph` rather than by hand so the combinations under
    /// test are ones the model actually emits.
    fn every_drawn_node() -> Vec<GraphNode> {
        let local = Identity::from_seed([0u8; 32]).announce("here", Capability::none());
        let mut nodes = Vec::new();
        // The local node, whose liveness is `Here` and nothing else.
        let mut store = PeerStore::ephemeral();
        // One peer per (trust, last-seen) pair, which is every liveness the
        // model can reach for a peer.
        let mut seed = 1u8;
        for trust in [Trust::Blocked, Trust::Known, Trust::Trusted] {
            for last_seen in [None, Some(at(0)), Some(at(9_000))] {
                let id = Identity::from_seed([seed; 32]).id();
                let mut node = Node::new(id);
                node.name = PeerText::sanitize("peer");
                node.last_seen = last_seen;
                store.add(node, at(0));
                store.record_trust(&id, trust).expect("trust");
                if let Some(seen) = last_seen {
                    store.mark_seen(&id, seen);
                }
                seed += 1;
            }
        }
        let mut graph = MeshGraph::build(&local, &store, at(9_000));
        // …and a session hanging off each peer, so the session kind is covered
        // at every state its owner can be in.
        let owners: Vec<_> = graph
            .nodes()
            .iter()
            .filter_map(|node| match node.key {
                NodeKey::Node(id) if node.kind == NodeKind::Peer => Some(id),
                _ => None,
            })
            .collect();
        for owner in owners {
            graph.add_session(owner, &PeerText::sanitize("s-1"));
        }
        nodes.extend(graph.nodes().iter().cloned());
        nodes
    }

    /// **The rule that decides this screen.** Nothing a node can be — no trust,
    /// no kind, no recency — makes it draw as up unless
    /// [`Liveness::is_live`] says so.
    ///
    /// Both directions are asserted. "Never up when it should not be" is the
    /// safety half; "always up when it should be" is what stops the rule being
    /// kept by painting everything dead.
    #[test]
    fn nothing_reads_as_up_without_is_live() {
        for theme in ["minimal", "codex", "grok"] {
            let palette = palette(theme);
            let mut live_seen = 0;
            let mut dormant_seen = 0;
            for node in every_drawn_node() {
                let paint = node_paint(&node, &palette);
                let up = node.liveness.is_live();
                assert_eq!(paint.solid, up, "{theme}: {node:?} -> {paint:?}");
                assert_eq!(paint.halo.is_some(), up, "{theme}: {node:?}");
                assert_eq!(
                    paint.interior != palette.canvas,
                    up,
                    "{theme}: a node that is not live must be hollow: {node:?} -> {paint:?}"
                );
                if up {
                    live_seen += 1;
                } else {
                    dormant_seen += 1;
                }
            }
            assert!(live_seen > 0 && dormant_seen > 0, "{theme}: {live_seen}");
        }
    }

    /// Trust is not liveness, restated as an equality: at one liveness state,
    /// every trust decision paints the same interior, the same rim and the same
    /// claim. Only the weight of the rim moves.
    #[test]
    fn trust_moves_the_rim_and_nothing_else() {
        let palette = palette("codex");
        let mut by_liveness: std::collections::BTreeMap<Liveness, Vec<NodePaint>> =
            std::collections::BTreeMap::new();
        for node in every_drawn_node() {
            by_liveness
                .entry(node.liveness)
                .or_default()
                .push(node_paint(&node, &palette));
        }
        for (liveness, paints) in &by_liveness {
            let first = paints[0];
            for paint in paints {
                assert_eq!(paint.interior, first.interior, "{liveness:?}");
                assert_eq!(paint.rim, first.rim, "{liveness:?}");
                assert_eq!(paint.solid, first.solid, "{liveness:?}");
                assert_eq!(paint.halo, first.halo, "{liveness:?}");
            }
        }
        // And the rim really does move, or the paragraph above is describing a
        // channel that carries nothing.
        let widths: std::collections::BTreeSet<u64> = every_drawn_node()
            .iter()
            .map(|node| node_paint(node, &palette).rim_width.to_bits())
            .collect();
        assert_eq!(widths.len(), 2, "trust has exactly two weights");
    }

    /// The specific case the plan calls out: a peer a human trusted, that is
    /// not answering, draws exactly as un-live as a stranger that is not
    /// answering.
    #[test]
    fn a_trusted_peer_that_is_unreachable_draws_unreachable() {
        let palette = palette("minimal");
        let local = Identity::from_seed([0u8; 32]).announce("here", Capability::none());
        let mut store = PeerStore::ephemeral();
        let id = Identity::from_seed([7u8; 32]).id();
        store.add(Node::new(id), at(0));
        store.record_trust(&id, Trust::Trusted).expect("trust");
        // Heard from one second ago, and then blocked. Presence alone would
        // call this online.
        store.mark_seen(&id, at(9_000));
        store.record_trust(&id, Trust::Blocked).expect("block");

        let graph = MeshGraph::build(&local, &store, at(9_001));
        let node = graph.node(&NodeKey::Node(id)).expect("peer");
        assert_eq!(node.liveness, Liveness::Unreachable);
        let paint = node_paint(node, &palette);
        assert!(!paint.solid);
        assert!(paint.halo.is_none());
        assert_eq!(paint.interior, palette.canvas);
        assert!(paint.barred, "and it is struck through");
    }

    /// A hollow node is only hollow if its rim is not the canvas. If a theme
    /// ever declared a token equal to the canvas colour, the interior channel
    /// would silently stop carrying anything.
    #[test]
    fn every_liveness_colour_is_visible_against_the_canvas() {
        for theme in ["minimal", "codex", "grok"] {
            let palette = palette(theme);
            for liveness in [
                Liveness::Unreachable,
                Liveness::Unseen,
                Liveness::Stale,
                Liveness::Live,
                Liveness::Here,
            ] {
                assert_ne!(
                    liveness_color(liveness, &palette),
                    palette.canvas,
                    "{theme}: {liveness:?} is invisible"
                );
            }
        }
    }

    /// A link with a dead end is drawn, and drawn quieter. Both halves matter:
    /// hiding it loses the pasted address that never answered, and drawing it
    /// at full strength puts a dead edge beside a live stream.
    #[test]
    fn a_link_with_a_dead_end_is_quieter_and_still_drawn() {
        let palette = palette("codex");
        for kind in [
            EdgeKind::Peer,
            EdgeKind::Observed,
            EdgeKind::Delegation,
            EdgeKind::Session,
        ] {
            let live = link_paint(kind, 1, true, &palette);
            let dead = link_paint(kind, 1, false, &palette);
            assert!(dead.color.a > 0.0, "{kind:?} vanished");
            assert!(dead.color.a < live.color.a, "{kind:?}");
            assert_eq!(dead.width, live.width, "{kind:?}");
        }
        // Weight thickens a delegation edge and is capped, because the count is
        // a `u32` that only ever goes up.
        let one = link_paint(EdgeKind::Delegation, 1, true, &palette).width;
        let many = link_paint(EdgeKind::Delegation, 6, true, &palette).width;
        let absurd = link_paint(EdgeKind::Delegation, u32::MAX, true, &palette).width;
        assert!(many > one);
        assert!(absurd < one * 4.0, "{absurd}");
    }

    /// Filtering pushes something back without deleting it.
    #[test]
    fn filtering_dims_rather_than_hides() {
        let color = Color::from_rgb(0.4, 0.7, 0.9);
        let dim = filtered(color);
        assert!(dim.a > 0.0 && dim.a < color.a);
        assert_eq!((dim.r, dim.g, dim.b), (color.r, color.g, color.b));
    }
}
