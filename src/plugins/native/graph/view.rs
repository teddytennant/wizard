//! Everything the explorer knows, with no widget in sight.
//!
//! The screen is a canvas, a panel and a subscription, and none of the three is
//! where the interesting decisions are. They are here: what a press means, what
//! a drag does to the layout, when the simulation is allowed to stop, and which
//! nodes a capability filter is talking about. Holding them in a plain struct
//! with plain methods is what lets this machine — which has no display — prove
//! the behaviour by calling functions, instead of by asserting on pixels.
//!
//! # Stepping, and stopping
//!
//! The plan's bar is 60fps at fifty nodes, but the *interesting* number is what
//! a graph costs once it is still, and the honest answer has to be zero. So the
//! timer that drives [`crate::graph::Layout::step`] is a subscription that
//! [`GraphView::needs_step`] switches off, and `needs_step` is the layout's own
//! [`kinetic_energy`](crate::graph::Layout::kinetic_energy) against
//! [`SETTLE_ENERGY`].
//!
//! Kinetic energy alone is not sufficient and the gap is worth writing down,
//! because it is silent: a node that has just been unpinned has zero velocity
//! and a large force, so a graph that stops the instant its energy hits zero
//! freezes with the operator's release still un-simulated. [`GraphView::wake`]
//! is the answer — a handful of steps owed regardless of energy, spent
//! whenever anything disturbs the arrangement. The energy gate is what makes
//! it stop; the wake is what makes it start.
//!
//! # Following, then not
//!
//! The viewport fits [`Layout::bounds`](crate::graph::Layout::bounds) when the
//! screen opens, and keeps re-fitting while the simulation is still moving,
//! because the bounds on the first frame are the bounds of a seeded scatter and
//! not of a graph. The first pan, zoom or drag ends that: from then on the
//! camera is the operator's and nothing moves it without being asked.

use iced::{Point as Screen, Size, Vector};

use crate::graph::{Layout, MeshGraph, NodeKey, Point as World};
use crate::mesh::CapabilityKind;

use super::viewport::Viewport;

/// Total kinetic energy below which the simulation is considered settled.
///
/// Summed over every node, in world units squared per step. At fifty nodes this
/// is under a thousandth of a world unit of movement each, which is a small
/// fraction of a pixel at any zoom a person would use — so the picture is
/// visually still well before the timer stops, rather than stopping mid-drift.
pub const SETTLE_ENERGY: f64 = 0.02;

/// Steps owed after a disturbance, whatever the energy says.
///
/// Three rather than one: the first step turns the new forces into velocity,
/// and two more make sure a node released into a near-balanced spot does not
/// stop again inside the epsilon before it has moved at all.
pub const WAKE_STEPS: u32 = 3;

/// How far the pointer must travel before a press becomes a drag.
///
/// Without it every click pins the node it landed on, because no hand releases
/// a mouse button at exactly the pixel it pressed.
pub const DRAG_THRESHOLD: f32 = 3.0;

/// One gesture in flight: what it grabbed and where it has been.
///
/// A struct with an optional key rather than a two-variant enum, because the
/// two gestures differ only in what moves. The threshold, the origin and the
/// last position are the same bookkeeping either way, and an enum would carry
/// them twice.
#[derive(Debug, Clone, PartialEq)]
struct Grab {
    /// The node being dragged, or `None` when the gesture is panning the
    /// camera because it started on empty canvas.
    key: Option<NodeKey>,
    /// Where the button went down, for the drag threshold.
    from: Screen,
    /// Where the pointer was last seen, for the pan delta.
    last: Screen,
    /// Whether the pointer has moved far enough to be a drag rather than a
    /// click. A click selects; only a drag pins.
    dragging: bool,
}

/// A capability the operator has filtered on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityFilter {
    pub kind: CapabilityKind,
    pub name: String,
}

/// The explorer's whole state, minus the mesh handle and the widgets.
#[derive(Debug)]
pub struct GraphView {
    graph: MeshGraph,
    layout: Layout,
    viewport: Viewport,
    /// The canvas's last reported size. Nothing can be converted between world
    /// and screen without it, so it is stored rather than passed: a pointer
    /// message and a redraw see the same one.
    canvas: Size,
    /// Whether the camera is still tracking the graph. False from the first
    /// gesture onward. See the module header.
    following: bool,
    selected: Option<NodeKey>,
    filter: Option<CapabilityFilter>,
    /// Whether each node, in [`MeshGraph::nodes`] order, is inside the filter.
    /// Recomputed when the filter or the graph changes, never per frame.
    matching: Vec<bool>,
    grab: Option<Grab>,
    /// Steps owed regardless of kinetic energy.
    owed: u32,
}

impl GraphView {
    /// A view over `graph`, seeded and unsettled.
    pub fn new(graph: MeshGraph, seed: u64) -> Self {
        let layout = Layout::new(&graph, seed);
        let matching = vec![true; graph.len()];
        Self {
            graph,
            layout,
            viewport: Viewport::default(),
            canvas: Size::ZERO,
            following: true,
            selected: None,
            filter: None,
            matching,
            grab: None,
            owed: WAKE_STEPS,
        }
    }

    /// The snapshot both the canvas and the inspector read, so the two cannot
    /// disagree about whether a peer is live.
    pub fn graph(&self) -> &MeshGraph {
        &self.graph
    }

    /// Where everything is, in world coordinates.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The camera.
    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// The canvas's last reported pixel size. The canvas reads it back to know
    /// whether it needs to report a new one.
    pub fn canvas(&self) -> Size {
        self.canvas
    }

    /// What the inspector is describing.
    pub fn selected(&self) -> Option<&NodeKey> {
        self.selected.as_ref()
    }

    /// The capability being filtered on, if any.
    pub fn filter(&self) -> Option<&CapabilityFilter> {
        self.filter.as_ref()
    }

    /// Whether the camera is still tracking the graph rather than the operator.
    pub fn following(&self) -> bool {
        self.following
    }

    /// Whether node `index` is inside the active capability filter. Always true
    /// when nothing is filtered.
    pub fn matches_filter(&self, index: usize) -> bool {
        self.matching.get(index).copied().unwrap_or(true)
    }

    /// Whether the node under a key is pinned in place.
    pub fn is_pinned(&self, key: &NodeKey) -> bool {
        self.layout.is_pinned(key)
    }

    /// Whether a button is down on the canvas.
    ///
    /// What the canvas asks before it forwards a move or a release: a pointer
    /// crossing the window with nothing grabbed is not this screen's business,
    /// and publishing a message per mouse move would rebuild the widget tree
    /// sixty times a second for nothing.
    pub fn grabbing(&self) -> bool {
        self.grab.is_some()
    }

    /// The node being dragged right now, if any.
    pub fn dragging(&self) -> Option<&NodeKey> {
        self.grab
            .as_ref()
            .filter(|grab| grab.dragging)
            .and_then(|grab| grab.key.as_ref())
    }

    /// Replace the graph, keeping every position, pin and selection that still
    /// refers to something.
    ///
    /// The point of the whole keyed design: a peer answering, a session
    /// starting or a revocation landing is a change to one dot, not a picture
    /// that jumps.
    pub fn replace(&mut self, graph: MeshGraph) {
        self.graph = graph;
        self.layout.sync(&self.graph);
        if let Some(selected) = &self.selected
            && self.graph.index_of(selected).is_none()
        {
            self.selected = None;
        }
        self.recompute_filter();
        self.wake();
    }

    /// Note the canvas's size. Every transform depends on it.
    pub fn resize(&mut self, canvas: Size) {
        if self.canvas == canvas {
            return;
        }
        self.canvas = canvas;
        if self.following {
            self.viewport = Viewport::fit(self.layout.bounds(), self.canvas);
        }
    }

    /// Whether the simulation still has anywhere to go.
    ///
    /// The whole of the "a still graph costs nothing" claim: while this is
    /// false the screen subscribes to no timer at all.
    pub fn needs_step(&self) -> bool {
        self.owed > 0 || self.layout.kinetic_energy() > SETTLE_ENERGY
    }

    /// Advance the simulation one step.
    pub fn step(&mut self) {
        self.owed = self.owed.saturating_sub(1);
        self.layout.step(&self.graph);
        if self.following {
            self.viewport = Viewport::fit(self.layout.bounds(), self.canvas);
        }
    }

    /// Owe the simulation a few steps, whatever its energy says.
    pub fn wake(&mut self) {
        self.owed = WAKE_STEPS;
    }

    /// Put the camera back on the whole graph, and let it track again.
    pub fn fit(&mut self) {
        self.following = true;
        self.viewport = Viewport::fit(self.layout.bounds(), self.canvas);
    }

    /// Select a node, or clear the selection with `None`.
    pub fn select(&mut self, key: Option<NodeKey>) {
        self.selected = key.filter(|key| self.graph.index_of(key).is_some());
    }

    /// Let a pinned node move again.
    pub fn unpin(&mut self, key: &NodeKey) {
        if self.layout.unpin(key) {
            self.wake();
        }
    }

    /// Which node is under a canvas point, if any.
    ///
    /// The composition the module docs promise: invert the transform, then ask
    /// the layout in its own coordinates, with a slop that is constant in
    /// pixels rather than in world units.
    pub fn hit(&self, at: Screen) -> Option<NodeKey> {
        self.layout.hit_test(
            self.viewport.to_world(at, self.canvas),
            self.viewport.hit_slop(),
        )
    }

    /// A press on the canvas. Selects whatever is under it, and starts either a
    /// node drag or a pan.
    pub fn press(&mut self, at: Screen) {
        let key = self.hit(at);
        // A press on empty canvas clears the selection, so the inspector is
        // never describing a node the operator has stopped looking at.
        self.selected = key.clone();
        self.grab = Some(Grab {
            key,
            from: at,
            last: at,
            dragging: false,
        });
    }

    /// The pointer moved to `at` with the button still down.
    pub fn drag_to(&mut self, at: Screen) {
        let Some(mut grab) = self.grab.take() else {
            return;
        };
        if !grab.dragging && (at.x - grab.from.x).hypot(at.y - grab.from.y) >= DRAG_THRESHOLD {
            grab.dragging = true;
        }
        if grab.dragging {
            // Either gesture is the operator taking the camera: a pan moves it
            // outright, and a node drag is an arrangement decision the
            // automatic fit must stop overriding.
            self.take_camera();
            match &grab.key {
                // Pinning *is* dragging: the node is held where the pointer put
                // it and everything else arranges itself around the decision.
                // `Layout::pin` zeroes the velocity, so a released node starts
                // from rest rather than from whatever momentum the drag implied.
                Some(key) => {
                    self.layout
                        .pin(key, self.viewport.to_world(at, self.canvas));
                    self.owed = WAKE_STEPS;
                }
                None => self
                    .viewport
                    .pan_by(Vector::new(at.x - grab.last.x, at.y - grab.last.y)),
            }
        }
        grab.last = at;
        self.grab = Some(grab);
    }

    /// The button came up. A dragged node stays pinned where it was let go.
    pub fn release(&mut self) {
        if let Some(grab) = self.grab.take()
            && grab.dragging
            && grab.key.is_some()
        {
            self.wake();
        }
    }

    /// A scroll wheel or trackpad gesture over `at`.
    pub fn zoom(&mut self, at: Screen, factor: f64) {
        self.take_camera();
        self.viewport.zoom_at(at, factor, self.canvas);
    }

    /// Filter to the nodes advertising one capability, or clear it with `None`.
    pub fn set_filter(&mut self, filter: Option<CapabilityFilter>) {
        self.filter = filter;
        self.recompute_filter();
    }

    /// The operator has taken the camera; stop following the graph with it.
    fn take_camera(&mut self) {
        self.following = false;
    }

    fn recompute_filter(&mut self) {
        self.matching = match &self.filter {
            None => vec![true; self.graph.len()],
            Some(filter) => {
                let mut matching = vec![false; self.graph.len()];
                for &index in self.graph.advertisers(filter.kind, &filter.name) {
                    if let Some(slot) = matching.get_mut(index) {
                        *slot = true;
                    }
                }
                matching
            }
        };
    }

    /// Where a world point lands on the canvas, at the current camera.
    pub fn to_screen(&self, world: World) -> Screen {
        self.viewport.to_screen(world, self.canvas)
    }

    /// A world length in canvas pixels.
    pub fn to_pixels(&self, world_length: f64) -> f32 {
        self.viewport.to_pixels(world_length)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;
    use crate::graph::{Liveness, NodeKind};
    use crate::mesh::peer::synthetic_store;
    use crate::mesh::{Capability, Identity, Node, PeerStore, Trust};

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    fn local() -> Node {
        Identity::from_seed([0u8; 32]).announce("here", Capability::none())
    }

    pub(super) fn synthetic(count: usize) -> MeshGraph {
        let now = at(0);
        MeshGraph::build(&local(), &synthetic_store(count, 1, now), now)
    }

    fn opened(count: usize) -> GraphView {
        let mut view = GraphView::new(synthetic(count), 4);
        view.resize(Size::new(900.0, 700.0));
        view
    }

    /// Run until the timer would switch itself off, and say how long it took.
    fn settle(view: &mut GraphView, cap: usize) -> usize {
        let mut steps = 0;
        while view.needs_step() && steps < cap {
            view.step();
            steps += 1;
        }
        steps
    }

    /// The claim behind the whole subscription design: a graph that has stopped
    /// moving stops asking for frames.
    #[test]
    fn a_settled_graph_asks_for_no_more_frames() {
        let mut view = opened(50);
        assert!(view.needs_step(), "a fresh graph has somewhere to go");
        let steps = settle(&mut view, 20_000);
        println!("50 nodes settled in {steps} steps");
        assert!(steps < 20_000, "it never settled");
        assert!(!view.needs_step());
        // And it stays off: stepping a settled graph is not what wakes it.
        assert!(view.layout().positions().iter().all(|p| p.is_finite()));
    }

    /// The gap kinetic energy alone leaves. Releasing a pinned node gives it
    /// zero velocity and a large force, so a screen gated on energy alone
    /// freezes with the operator's release un-simulated.
    #[test]
    fn releasing_a_pinned_node_wakes_a_settled_graph() {
        let mut view = opened(12);
        settle(&mut view, 20_000);
        assert!(!view.needs_step());

        let key = view.graph().nodes()[3].key.clone();
        // Drag it somewhere absurd and let go.
        view.press(view.to_screen(view.layout().position_of(&key).expect("placed")));
        view.drag_to(Screen::new(40.0, 40.0));
        view.drag_to(Screen::new(60.0, 60.0));
        view.release();
        assert!(view.is_pinned(&key), "a drag pins");

        settle(&mut view, 20_000);
        assert!(!view.needs_step());
        let held = view.layout().position_of(&key).expect("placed");

        view.unpin(&key);
        assert!(
            view.needs_step(),
            "a released node has no velocity and every reason to move"
        );
        settle(&mut view, 20_000);
        assert_ne!(
            view.layout().position_of(&key),
            Some(held),
            "and it actually moved"
        );
    }

    /// Clicking selects. Dragging pins, and the rest of the mesh rearranges
    /// around the decision — which is the plan's acceptance sentence, as a
    /// test.
    #[test]
    fn clicking_selects_and_dragging_pins_while_the_rest_rearranges() {
        let mut view = opened(12);
        settle(&mut view, 20_000);
        let key = view.graph().nodes()[5].key.clone();
        let neighbour = view.graph().nodes()[6].key.clone();
        let at = view.to_screen(view.layout().position_of(&key).expect("placed"));

        // A click: selects, does not pin.
        view.press(at);
        view.drag_to(Screen::new(at.x + 1.0, at.y));
        view.release();
        assert_eq!(view.selected(), Some(&key));
        assert!(!view.is_pinned(&key), "a click is not a drag");

        let before = view.layout().position_of(&neighbour).expect("placed");
        // A drag: pins, and holds.
        view.press(at);
        view.drag_to(Screen::new(at.x + 120.0, at.y + 90.0));
        view.release();
        assert!(view.is_pinned(&key));
        let pinned = view.layout().position_of(&key).expect("placed");

        settle(&mut view, 20_000);
        assert_eq!(
            view.layout().position_of(&key),
            Some(pinned),
            "a pinned node does not drift"
        );
        assert_ne!(
            view.layout().position_of(&neighbour),
            Some(before),
            "and the mesh rearranged around it"
        );

        // Pressing empty canvas clears the selection rather than leaving the
        // inspector describing something nobody is looking at.
        view.press(Screen::new(2.0, 2.0));
        assert!(view.selected().is_none());
    }

    /// A press on empty canvas pans, and the first gesture takes the camera
    /// away from the automatic fit for good.
    #[test]
    fn panning_takes_the_camera_from_the_fit() {
        let mut view = opened(12);
        assert!(view.following());
        let before = view.viewport();

        view.press(Screen::new(4.0, 4.0));
        view.drag_to(Screen::new(104.0, 4.0));
        view.release();
        assert!(!view.following());
        assert_ne!(view.viewport(), before);

        // Stepping no longer moves it.
        let held = view.viewport();
        for _ in 0..50 {
            view.step();
        }
        assert_eq!(view.viewport(), held);

        // …until it is asked to fit again.
        view.fit();
        assert!(view.following());
    }

    /// Hit testing is the inverse of drawing, through a pan and a zoom. This is
    /// the composition the two modules exist to keep honest: get the sign of
    /// the pan wrong and clicking a node selects its neighbour.
    #[test]
    fn every_node_is_clickable_where_it_is_drawn() {
        let mut view = opened(20);
        settle(&mut view, 20_000);
        view.zoom(Screen::new(300.0, 200.0), 1.6);
        view.press(Screen::new(10.0, 10.0));
        view.drag_to(Screen::new(70.0, 40.0));
        view.release();

        let mut hits = 0;
        for node in view.graph().nodes() {
            let world = view.layout().position_of(&node.key).expect("placed");
            let at = view.to_screen(world);
            match view.hit(at) {
                Some(key) => {
                    assert_eq!(key, node.key, "{:?} is drawn where {key:?} is", node.key);
                    hits += 1;
                }
                // Two dots can overlap, and then the nearer centre wins. That
                // is `Layout::hit_test`'s documented behaviour and not a
                // transform bug, so it is allowed — but not for everybody.
                None => panic!("{:?} is drawn where nothing is clickable", node.key),
            }
        }
        assert_eq!(hits, view.graph().len());

        // Empty canvas is a miss.
        let mut far = view.to_screen(view.layout().bounds().max);
        far.x += 400.0;
        far.y += 400.0;
        assert!(view.hit(far).is_none());
    }

    /// A capability filter says which nodes it is talking about, and keeps the
    /// others in the picture.
    #[test]
    fn a_capability_filter_marks_advertisers_without_removing_anyone() {
        let mut view = opened(30);
        let (kind, name, advertisers) = view
            .graph()
            .capabilities()
            .map(|(kind, name, nodes)| (kind, name.to_string(), nodes.to_vec()))
            .find(|(_, _, nodes)| nodes.len() > 1 && nodes.len() < view.graph().len())
            .expect("the synthetic mesh shares capabilities");

        view.set_filter(Some(CapabilityFilter {
            kind,
            name: name.clone(),
        }));
        let before = view.graph().len();
        for index in 0..before {
            assert_eq!(
                view.matches_filter(index),
                advertisers.contains(&index),
                "node {index} against {name}"
            );
        }
        assert_eq!(view.graph().len(), before, "nobody left the graph");

        view.set_filter(None);
        assert!((0..before).all(|index| view.matches_filter(index)));

        // A capability nobody has matches nobody, and does not panic.
        view.set_filter(Some(CapabilityFilter {
            kind,
            name: "nothing-advertises-this".to_string(),
        }));
        assert!((0..before).all(|index| !view.matches_filter(index)));
    }

    /// The zero-peer case: one node, a camera, and no division by an empty
    /// extent.
    #[test]
    fn a_mesh_of_one_renders_and_settles() {
        let graph = MeshGraph::build(&local(), &PeerStore::ephemeral(), at(0));
        let mut view = GraphView::new(graph, 1);
        view.resize(Size::new(600.0, 400.0));
        let steps = settle(&mut view, 20_000);
        assert!(steps < 20_000);
        assert_eq!(view.graph().len(), 1);
        assert!(view.viewport().scale.is_finite() && view.viewport().scale > 0.0);
        let key = view.graph().nodes()[0].key.clone();
        assert_eq!(
            view.hit(view.to_screen(view.layout().position_of(&key).unwrap())),
            Some(key)
        );
    }

    /// An unreachable peer is in the picture, is clickable, and the model still
    /// refuses to call it live.
    #[test]
    fn an_unreachable_peer_is_drawn_and_is_not_live() {
        let mut store = PeerStore::ephemeral();
        let id = Identity::from_seed([5u8; 32]).id();
        store.add(Node::new(id), at(0));
        store.record_trust(&id, Trust::Blocked).expect("block");
        store.mark_seen(&id, at(0));
        let graph = MeshGraph::build(&local(), &store, at(1));

        let mut view = GraphView::new(graph, 2);
        view.resize(Size::new(600.0, 400.0));
        settle(&mut view, 20_000);
        let node = &view.graph().nodes()[1];
        assert_eq!(node.kind, NodeKind::Peer);
        assert_eq!(node.liveness, Liveness::Unreachable);
        assert!(!node.liveness.is_live());
        let key = node.key.clone();
        assert_eq!(
            view.hit(view.to_screen(view.layout().position_of(&key).unwrap())),
            Some(key)
        );
    }

    /// Replacing the graph keeps the arrangement: this is what makes a
    /// revocation redraw one dot rather than throw the picture in the air.
    #[test]
    fn replacing_the_graph_keeps_positions_pins_and_a_live_selection() {
        let mut view = opened(10);
        settle(&mut view, 20_000);
        let key = view.graph().nodes()[4].key.clone();
        view.select(Some(key.clone()));
        view.press(view.to_screen(view.layout().position_of(&key).expect("placed")));
        view.drag_to(Screen::new(500.0, 300.0));
        view.release();
        let pinned = view.layout().position_of(&key).expect("placed");

        // The same mesh at a later clock: every peer is staler, nobody left.
        let later = MeshGraph::build(&local(), &synthetic_store(10, 1, at(0)), at(50_000));
        view.replace(later);
        assert_eq!(view.layout().position_of(&key), Some(pinned));
        assert!(view.is_pinned(&key));
        assert_eq!(view.selected(), Some(&key));

        // …and a graph the selection is not in clears it rather than pointing
        // at a node that is gone.
        view.replace(MeshGraph::build(&local(), &PeerStore::ephemeral(), at(0)));
        assert!(view.selected().is_none());
    }
}
