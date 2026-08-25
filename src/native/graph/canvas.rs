//! The canvas: edges, nodes, labels, and the gestures over them.
//!
//! A thin adapter by design. It converts iced's events into the vocabulary
//! [`GraphView`] already speaks and draws what that view holds, and it keeps no
//! state of its own — `type State = ()` — because state inside a
//! [`canvas::Program`] lives in the widget tree, where a headless test cannot
//! reach it and where the application update cannot reason about it. Every
//! decision a gesture implies is made in [`GraphView`] instead, which is a
//! plain struct this machine can drive with no compositor at all.
//!
//! # Why pointer messages carry the canvas size
//!
//! The transform needs the canvas's pixel size, and a `Program` only learns it
//! from the `bounds` it is handed per event. If the size travelled separately
//! — a `Resized` message published when it changes — then the first click after
//! a window resize would be answered against the previous frame's size, and
//! would select the wrong node. So every pointer message carries the bounds it
//! was measured in, and the view is told the size before the point is used.
//!
//! # Drawing order
//!
//! Links first, then halos, then discs, then labels. Not decoration: a link
//! drawn over a node makes a hollow node look filled, which is exactly the
//! claim this screen is not allowed to make by accident.

use iced::mouse;
use iced::widget::canvas::{self, Action, Frame, Geometry, Path, Stroke, Text};
use iced::{Color, Point as Screen, Rectangle, Renderer, Size, Theme};

use crate::native::theme::Palette;
use crate::plugins::graph::layout::node_radius;
use crate::plugins::graph::{GraphNode, NodeKind};

use super::paint::{filtered, halo_reach, link_paint, node_paint};
use super::view::GraphView;
use super::viewport::zoom_factor;
use super::{LABEL_SIZE, Message};

/// Smallest a node is allowed to draw, in pixels.
///
/// Zoomed far out, `node_radius * scale` goes under a pixel and the graph turns
/// into a smear of anti-aliasing. A floor keeps every node visible as a node.
const MIN_NODE_PIXELS: f32 = 1.6;

/// Zoom below which labels stop being drawn.
///
/// Fifty overlapping names is less legible than none, and shaping text that
/// nobody can read is the most expensive thing in the frame.
const LABEL_MIN_SCALE: f64 = 0.55;

/// Extra ring drawn around the selected node, in pixels past its rim.
const SELECTION_REACH: f32 = 4.0;

/// The canvas over one [`GraphView`].
///
/// Borrows rather than owns, so `view()` can build it per frame without cloning
/// a graph.
#[derive(Debug)]
pub struct GraphCanvas<'a> {
    pub view: &'a GraphView,
    pub palette: &'a Palette,
}

impl canvas::Program<Message> for GraphCanvas<'_> {
    type State = ();

    fn update(
        &self,
        _state: &mut (),
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<Message>> {
        let size = bounds.size();
        // A resize has to reach the view even when nothing is being pointed
        // at, or the first fit never happens and the graph opens off-camera.
        if let canvas::Event::Window(iced::window::Event::RedrawRequested(_)) = event {
            return (self.view.canvas() != size).then(|| Action::publish(Message::Resized(size)));
        }

        let inside = cursor.position_in(bounds);
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let at = inside?;
                Some(Action::publish(Message::Press(at, size)).and_capture())
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                // Not gated on `inside`: a drag that leaves the canvas must
                // keep dragging, exactly as a scrollbar does, or a node let go
                // over the inspector is dropped halfway.
                let at = cursor.position_from(Screen::new(bounds.x, bounds.y))?;
                self.view
                    .grabbing()
                    .then(|| Action::publish(Message::Drag(at, size)).and_capture())
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => self
                .view
                .grabbing()
                .then(|| Action::publish(Message::Release).and_capture()),
            canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let at = inside?;
                Some(Action::publish(Message::Zoom(at, zoom_factor(*delta), size)).and_capture())
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        self.draw_links(&mut frame);
        self.draw_nodes(&mut frame, bounds.size());
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &(),
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if self.view.dragging().is_some() {
            return mouse::Interaction::Grabbing;
        }
        match cursor.position_in(bounds) {
            Some(at) if self.view.hit(at).is_some() => mouse::Interaction::Pointer,
            Some(_) => mouse::Interaction::Grab,
            None => mouse::Interaction::None,
        }
    }
}

impl GraphCanvas<'_> {
    /// Every link this canvas will put a line on screen for.
    ///
    /// Named rather than inlined into [`GraphCanvas::draw_links`] so a test can
    /// hold it: capability links cluster the layout and are never lines,
    /// [`Link::is_drawn`](crate::plugins::graph::Link::is_drawn) is what says so, and
    /// [`MeshGraph::drawn_links`](crate::plugins::graph::MeshGraph::drawn_links) is what
    /// applies it. Iterating `links()` here instead would put a line between
    /// every pair of peers offering the same model, which on a fifty-node mesh
    /// is a solid block conveying nothing.
    fn lines(&self) -> impl Iterator<Item = &crate::plugins::graph::Link> {
        self.view.graph().drawn_links()
    }

    fn draw_links(&self, frame: &mut Frame) {
        let nodes = self.view.graph().nodes();
        for link in self.lines() {
            let (Some(from), Some(to)) = (nodes.get(link.from), nodes.get(link.to)) else {
                continue;
            };
            let (Some(a), Some(b)) = (
                self.view.layout().position_of(&from.key),
                self.view.layout().position_of(&to.key),
            ) else {
                continue;
            };
            // The link's own honesty: it is only drawn at full strength when
            // both of its ends are up, and `is_live` is the only thing asked.
            let both_live = from.liveness.is_live() && to.liveness.is_live();
            let paint = link_paint(link.kind, link.weight, both_live, self.palette);
            let color = if self.view.matches_filter(link.from) && self.view.matches_filter(link.to)
            {
                paint.color
            } else {
                filtered(paint.color)
            };
            frame.stroke(
                &Path::line(self.view.to_screen(a), self.view.to_screen(b)),
                Stroke::default()
                    .with_color(color)
                    .with_width(self.view.to_pixels(paint.width).max(0.6)),
            );
        }
    }

    fn draw_nodes(&self, frame: &mut Frame, canvas: Size) {
        let selected = self.view.selected();
        for (index, node) in self.view.graph().nodes().iter().enumerate() {
            let Some(world) = self.view.layout().position_of(&node.key) else {
                continue;
            };
            let at = self.view.to_screen(world);
            // Everything outside the canvas is still simulated and still
            // hit-testable; it just is not worth a draw call.
            if !on_canvas(at, canvas) {
                continue;
            }
            let mut paint = node_paint(node, self.palette);
            if !self.view.matches_filter(index) {
                paint.interior = filtered(paint.interior);
                paint.rim = filtered(paint.rim);
                paint.label = filtered(paint.label);
                paint.halo = paint.halo.map(filtered);
            }
            let radius = self.view.to_pixels(node_radius(node)).max(MIN_NODE_PIXELS);

            if let Some(halo) = paint.halo {
                frame.fill(
                    &Path::circle(at, radius + self.view.to_pixels(halo_reach()).max(1.0)),
                    halo,
                );
            }
            // The interior is painted for both states: for a live node it is
            // the liveness colour, and for every other node it is the canvas,
            // which punches the links out from behind the rim.
            frame.fill(&Path::circle(at, radius), paint.interior);
            frame.stroke(
                &Path::circle(at, radius),
                Stroke::default()
                    .with_color(paint.rim)
                    .with_width(self.view.to_pixels(paint.rim_width).max(1.0)),
            );
            if paint.barred {
                bar(frame, at, radius, paint.rim);
            }
            if Some(&node.key) == selected {
                frame.stroke(
                    &Path::circle(at, radius + SELECTION_REACH),
                    Stroke::default()
                        .with_color(self.palette.color(crate::theme::Token::Accent))
                        .with_width(1.0),
                );
            }
            if self.view.viewport().scale >= LABEL_MIN_SCALE {
                // Measured from the halo's rim, not the disc's. The halo
                // reaches 4.5 world units past the node and the label's own
                // gap is 5 pixels, so anchoring on the disc spent the whole
                // gap on the halo and set the first glyph down touching it.
                //
                // Reserved whether or not this node *has* a halo, because
                // halos come and go with liveness: a label that shifts a few
                // pixels when a peer goes quiet reads as the graph twitching.
                let rim = radius + self.view.to_pixels(halo_reach()).max(1.0);
                frame.fill_text(label_of(node, at, rim, paint.label));
            }
        }
    }
}

/// The strike through a node this machine will not contact.
///
/// A second, non-chromatic channel for the one state where being wrong is
/// worst: `minimal` paints `error` and `warning` the same white, so a blocked
/// peer and a stale one would otherwise differ only by being hollow in the same
/// colour.
fn bar(frame: &mut Frame, at: Screen, radius: f32, color: Color) {
    let reach = radius * 0.72;
    frame.stroke(
        &Path::line(
            Screen::new(at.x - reach, at.y - reach),
            Screen::new(at.x + reach, at.y + reach),
        ),
        Stroke::default().with_color(color).with_width(1.4),
    );
}

/// The text beside a dot: the peer's own name, the separator, and the
/// fingerprint prefix it cannot choose.
///
/// [`DisplayName::rendered`](crate::plugins::graph::DisplayName) always carries the
/// discriminator, so two peers calling themselves the same thing are told apart
/// on the canvas and not only in the inspector.
fn label_of(node: &GraphNode, at: Screen, radius: f32, color: Color) -> Text {
    Text {
        content: node.name.rendered(),
        position: Screen::new(at.x + radius + 5.0, at.y),
        color,
        size: iced::Pixels(if node.kind == NodeKind::Local {
            LABEL_SIZE + 1.0
        } else {
            LABEL_SIZE
        }),
        align_y: iced::alignment::Vertical::Center,
        ..Text::default()
    }
}

/// Whether a node at `at` could put ink on a canvas of `size`.
///
/// Generous: the margin is wider than any label, because a node just off the
/// left edge still draws its name across the canvas.
fn on_canvas(at: Screen, size: Size) -> bool {
    const MARGIN: f32 = 320.0;
    at.x > -MARGIN && at.y > -MARGIN && at.x < size.width + MARGIN && at.y < size.height + MARGIN
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::graph::Liveness;

    fn palette() -> Palette {
        Palette::from_theme(&crate::theme::minimal())
    }

    /// Capability links cluster and are never drawn. A canvas that iterated
    /// `links()` instead of `drawn_links()` would put a line between every pair
    /// of peers offering the same model, which on a fifty-node mesh is a solid
    /// block conveying nothing.
    #[test]
    fn capability_links_are_never_lines() {
        let view = super::super::tests::settled_view(20);
        let palette = palette();
        let canvas = GraphCanvas {
            view: &view,
            palette: &palette,
        };
        // Asserted on what the canvas will draw, not on what the graph offers:
        // the failure this guards against is a draw loop that iterates the
        // wrong collection.
        let clustering = view
            .graph()
            .links()
            .iter()
            .filter(|link| link.kind == crate::mesh::EdgeKind::Capability)
            .count();
        assert!(clustering > 0, "the synthetic mesh shares capabilities");
        assert_eq!(
            canvas.lines().count(),
            view.graph().links().len() - clustering
        );
        assert!(
            canvas
                .lines()
                .all(|link| link.kind != crate::mesh::EdgeKind::Capability)
        );
    }

    /// A live node's label is brighter than a dormant one's, and the dormant
    /// one is still legible. Reading a peer's name should not require it to be
    /// online.
    #[test]
    fn a_dormant_node_still_has_a_readable_label() {
        let view = super::super::tests::settled_view(30);
        let palette = palette();
        let mut live = None;
        let mut dormant = None;
        for node in view.graph().nodes() {
            let paint = node_paint(node, &palette);
            if node.liveness.is_live() {
                live = Some(paint.label);
            } else {
                dormant = Some(paint.label);
            }
        }
        let (live, dormant) = (live.expect("a live node"), dormant.expect("a dormant node"));
        assert_ne!(live, dormant);
        assert!(dormant.a > 0.0);
    }

    /// Nodes far off-camera are skipped, and nodes on it are not. A margin that
    /// was too tight would clip a label; one that was absent would cost a draw
    /// call per node in a graph nobody is looking at.
    #[test]
    fn only_what_could_put_ink_on_the_canvas_is_drawn() {
        let size = Size::new(800.0, 600.0);
        assert!(on_canvas(Screen::new(0.0, 0.0), size));
        assert!(on_canvas(Screen::new(799.0, 599.0), size));
        assert!(on_canvas(Screen::new(-100.0, 300.0), size), "its label is");
        assert!(!on_canvas(Screen::new(-4000.0, 300.0), size));
        assert!(!on_canvas(Screen::new(300.0, 9000.0), size));
    }

    /// This machine is drawn heavier than a peer, and labelled larger, because
    /// "which of these is me" is the first question anybody asks of a mesh
    /// graph.
    ///
    /// Compared against a peer with no delegations, which is the honest
    /// comparison: [`node_radius`] also grows with delegation count, so a peer
    /// that has been sent a lot of work legitimately draws bigger than the
    /// local node, and asserting otherwise would be asserting that the weight
    /// channel does not exist.
    #[test]
    fn this_machine_is_drawn_heavier_than_a_peer() {
        let view = super::super::tests::settled_view(20);
        let nodes = view.graph().nodes();
        let local = &nodes[0];
        assert_eq!(local.kind, NodeKind::Local);
        assert_eq!(local.liveness, Liveness::Here);
        let quiet = nodes[1..]
            .iter()
            .find(|node| node.delegations == 0)
            .expect("a synthetic peer with no delegations");
        assert!(node_radius(local) > node_radius(quiet));

        let at = Screen::new(10.0, 10.0);
        assert!(
            label_of(local, at, 4.0, Color::WHITE).size.0
                > label_of(quiet, at, 4.0, Color::WHITE).size.0
        );
    }
}
