//! The force-directed layout: positions in, positions out.
//!
//! Deliberately arithmetic and nothing else. There is no canvas here, no
//! toolkit, no time source and no randomness that is not seeded, because the
//! layout is the part of the explorer that has to be *pinned by tests* rather
//! than looked at: a picture nobody can reproduce is a picture nobody can
//! debug, and "the graph looks wrong today" is not a bug report.
//!
//! # Determinism
//!
//! Every number here comes out of `+ - * /`, `sqrt` and integer conversions,
//! all of which IEEE-754 requires to be correctly rounded, so the same graph
//! and the same seed produce the same positions bit for bit on any machine
//! running the same steps in the same order. Nothing calls `sin`, `cos`, `exp`
//! or `powf`, whose results are a libm implementation detail, and nothing
//! reads the clock or the system RNG: [`seed_position`] derives a node's
//! starting point from a hash of the node's own key, so adding a peer does not
//! reshuffle the ones already on screen and a layout bug found at one seed can
//! be reproduced at that seed.
//!
//! # The budget
//!
//! The plan's bar is 50 synthetic nodes at 60fps with software rendering,
//! which is 16.6ms for everything: layout, geometry and drawing. The step is
//! `O(n²)` in the repulsion pass and `O(edges)` in the rest, with no
//! allocation beyond one force buffer per call, and
//! `fifty_nodes_step_inside_the_frame_budget` holds it to a fraction of that
//! so the renderer keeps the rest.
//!
//! # Why there is no annealing schedule
//!
//! A cooling temperature converges faster and then freezes, which is what a
//! batch layout wants. This one is interactive: an operator drags a node, pins
//! it, revokes a peer and watches the graph reorganise, so it has to stay a
//! simulation that can always be nudged. Stability comes from damping, from
//! node mass, and from a hard per-step displacement cap, all three of which
//! are pure functions of the current state rather than of how many steps have
//! been taken.
//!
//! # Why a node has mass
//!
//! Without it this layout does not settle, and the reason is worth writing
//! down because it looks like a tuning problem and is not. Linearise one node
//! against a restoring force of stiffness `k` (the sum of the spring constants
//! pulling on it). The step is `v' = damping * (v - k*x)` then `x' = x + v'`,
//! whose characteristic polynomial is `λ² - (1 + damping - damping*k)λ +
//! damping`. The roots stay inside the unit circle only while
//! `damping * k < 1 + damping + 2*sqrt(damping)`, which at the default damping
//! is `k < 4.4`.
//!
//! Every peer hangs off the local node, so on a fifty-peer mesh the local
//! node's `k` is fifty spring constants added together: an order of magnitude
//! past that bound. The hub then overshoots, is thrown back harder, and the
//! whole picture jitters at the displacement cap forever. Giving each node a
//! mass of `1 + degree` divides its `k` by very nearly the same count that
//! built it, which puts every node in the mesh back inside the envelope and
//! makes settling a property of the model rather than of the parameters
//! happening to be small. It is also the physically honest reading: a hub is
//! heavy, and a leaf is not.

use std::collections::BTreeMap;

use crate::mesh::EdgeKind;

use super::model::{GraphNode, Link, MeshGraph, NodeKey, NodeKind};

/// How close two nodes may get before the layout stops believing the direction
/// between them.
///
/// Two nodes at the same point have an infinite `1/d²` repulsion and no
/// defined direction to apply it in, which is how a layout produces `NaN` and
/// then draws nothing at all. Below this separation the pair is pushed apart
/// along a direction derived from their indices, which is arbitrary but the
/// same every run.
const MIN_SEPARATION: f64 = 1.0;

/// Largest delegation count that still makes an edge stiffer.
///
/// A peer with four delegations should visibly sit closer than one with none.
/// A peer with forty thousand should not be welded to the local node, and the
/// count is a `u32` that only ever goes up.
const WEIGHT_CAP: u32 = 8;

/// Default radius of a drawn node, in world units.
pub const BASE_RADIUS: f64 = 9.0;

/// How far outside a node's own radius a click still counts as hitting it.
pub const DEFAULT_HIT_SLOP: f64 = 4.0;

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// A point in the layout's own coordinate space.
///
/// `f64` rather than the `f32` a renderer wants, because positions accumulate:
/// a step adds a velocity that is itself a sum of `n` forces, sixty times a
/// second, and the conversion at the drawing boundary costs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Distance from the origin.
    pub fn length(self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    /// Distance between two points.
    pub fn distance_to(self, other: Point) -> f64 {
        (self - other).length()
    }

    /// Whether both coordinates are finite. The invariant the whole module
    /// exists to keep: one `NaN` in a position propagates through every force
    /// in the next step and empties the canvas.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }

    /// The same direction, no longer than `limit`.
    fn clamped(self, limit: f64) -> Self {
        let length = self.length();
        if length <= limit || length == 0.0 {
            self
        } else {
            self * (limit / length)
        }
    }
}

impl std::ops::Add for Point {
    type Output = Point;
    fn add(self, other: Point) -> Point {
        Point::new(self.x + other.x, self.y + other.y)
    }
}

impl std::ops::Sub for Point {
    type Output = Point;
    fn sub(self, other: Point) -> Point {
        Point::new(self.x - other.x, self.y - other.y)
    }
}

impl std::ops::Mul<f64> for Point {
    type Output = Point;
    fn mul(self, scale: f64) -> Point {
        Point::new(self.x * scale, self.y * scale)
    }
}

/// An axis-aligned box around the laid-out graph, for the renderer to fit a
/// viewport to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub min: Point,
    pub max: Point,
}

impl Rect {
    /// The degenerate box at the origin, which is what an empty layout has.
    pub const EMPTY: Rect = Rect {
        min: Point::ZERO,
        max: Point::ZERO,
    };

    pub fn width(&self) -> f64 {
        self.max.x - self.min.x
    }

    pub fn height(&self) -> f64 {
        self.max.y - self.min.y
    }

    pub fn center(&self) -> Point {
        Point::new(
            (self.min.x + self.max.x) / 2.0,
            (self.min.y + self.max.y) / 2.0,
        )
    }
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// The knobs on the force model.
///
/// Defaults are tuned against the 50-node synthetic mesh, which is the shape
/// the plan sets the bar with. They are all in world units, and the whole set
/// scales with [`LayoutParams::ideal_edge`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutParams {
    /// How hard every node pushes every other away, as a Coulomb `k/d²`.
    pub repulsion: f64,
    /// Spring constant for a link, per unit of stretch past `ideal_edge`.
    pub attraction: f64,
    /// Multiplier on `attraction` for the undrawn capability links. Weak on
    /// purpose: shared capability should group peers, not overrule the edges
    /// a person can actually see.
    pub cluster_attraction: f64,
    /// Pull toward the origin. Without it a component that nothing connects
    /// (a peer nobody has heard from, once every other force cancels) drifts
    /// off the canvas and never comes back.
    pub gravity: f64,
    /// Fraction of velocity carried into the next step. Under 1.0, so the
    /// simulation loses energy and settles instead of oscillating forever.
    pub damping: f64,
    /// Farthest a node may move in one step. The stability valve: it is what
    /// makes a huge force (two nodes seeded on top of each other) a nudge
    /// rather than a node flung to infinity.
    pub max_step: f64,
    /// Resting length of a link.
    pub ideal_edge: f64,
    /// Scatter of the seeded starting arrangement, before the `sqrt(n)` term.
    pub initial_spread: f64,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self {
            repulsion: 60_000.0,
            attraction: 0.35,
            cluster_attraction: 0.25,
            gravity: 0.02,
            damping: 0.82,
            max_step: 12.0,
            ideal_edge: 70.0,
            initial_spread: 30.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Seeding
// ---------------------------------------------------------------------------

/// Where a node starts, before any force has been applied.
///
/// A pure function of the node's key and the seed, *not* of its index. That
/// matters more than it looks: the graph is rebuilt from the store whenever
/// anything changes, and if the starting arrangement were index-based, adding
/// one peer would renumber every node after it and throw the whole picture in
/// the air. Keyed off the identity, a new peer appears and everything else
/// stays where the operator left it.
pub fn seed_position(key: &NodeKey, seed: u64, spread: f64) -> Point {
    let mixed = splitmix64(seed ^ key_hash(key));
    Point::new(
        unit_from(mixed as u32) * spread,
        unit_from((mixed >> 32) as u32) * spread,
    )
}

/// Seeded starting positions for every node in `graph`.
pub fn seed_positions(graph: &MeshGraph, seed: u64) -> Vec<Point> {
    let spread = spread_for(graph.len(), &LayoutParams::default());
    graph
        .nodes()
        .iter()
        .map(|node| seed_position(&node.key, seed, spread))
        .collect()
}

/// How far the seeded scatter reaches, for `n` nodes.
///
/// Grows with `sqrt(n)` so that density stays roughly constant: seeding five
/// hundred nodes into the box that suits fifty puts them all on top of each
/// other, and the first few steps are then spent undoing that rather than
/// laying anything out.
fn spread_for(n: usize, params: &LayoutParams) -> f64 {
    params.initial_spread * (n.max(1) as f64).sqrt()
}

/// A `[-1, 1]` value from 32 bits, without touching a transcendental.
fn unit_from(bits: u32) -> f64 {
    (bits as f64) / (u32::MAX as f64) * 2.0 - 1.0
}

/// FNV-1a over a node key.
///
/// Hand-rolled rather than `DefaultHasher`, whose output is explicitly not
/// guaranteed stable between Rust releases. A layout that reshuffles itself
/// when the toolchain is upgraded would make every snapshot test in this file
/// a liability.
fn key_hash(key: &NodeKey) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    let mut hash = OFFSET;
    let mut eat = |byte: u8| {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    match key {
        NodeKey::Node(id) => {
            eat(0x01);
            for byte in id.to_bytes() {
                eat(byte);
            }
        }
        NodeKey::Session(owner, name) => {
            eat(0x02);
            for byte in owner.to_bytes() {
                eat(byte);
            }
            for byte in name.as_bytes() {
                eat(*byte);
            }
        }
    }
    hash
}

/// Mix a 64-bit value so that neighbouring seeds do not give neighbouring
/// arrangements.
fn splitmix64(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------
// The step
// ---------------------------------------------------------------------------

/// How big a node is drawn, and therefore how big a target it is.
pub fn node_radius(node: &GraphNode) -> f64 {
    let base = match node.kind {
        NodeKind::Local => BASE_RADIUS * 1.4,
        NodeKind::Peer => BASE_RADIUS,
        NodeKind::Session { .. } => BASE_RADIUS * 0.6,
    };
    // Work that actually flowed makes a node heavier on screen, capped so a
    // long-lived peer does not grow into the whole canvas.
    base + (node.delegations.min(WEIGHT_CAP * 4) as f64) * 0.25
}

/// One step of the force model.
///
/// Pure in the sense that matters: it reads `graph`, `params` and `pinned`,
/// writes `positions` and `velocities`, and consults nothing else. Given the
/// same inputs it produces the same outputs, which is what makes the layout
/// snapshot-testable. In place rather than returning fresh vectors because it
/// runs sixty times a second and the allocation would be the only one.
///
/// A pinned node still *exerts* force on everything else; it just does not
/// move. That is the whole of pinning: an operator drags the node they care
/// about where they want it, and the rest of the mesh arranges itself around
/// the decision.
///
/// Mismatched lengths are a caller bug ([`Layout::sync`] is what keeps them
/// aligned) and are ignored rather than panicking: this runs inside a render
/// loop, where a torn frame beats a dead window.
pub fn step_positions(
    graph: &MeshGraph,
    params: &LayoutParams,
    positions: &mut [Point],
    velocities: &mut [Point],
    pinned: &[bool],
) {
    let n = positions.len();
    debug_assert_eq!(n, graph.len(), "layout is out of sync with its graph");
    if n != graph.len() || n != velocities.len() || n != pinned.len() {
        return;
    }

    let mut forces = vec![Point::ZERO; n];

    // Repulsion: every pair once, applied to both ends. Symmetric because
    // Newton was right and because halving the inner loop halves the cost of
    // the only quadratic pass in the frame.
    for i in 0..n {
        let pi = positions[i];
        for j in (i + 1)..n {
            let mut delta = pi - positions[j];
            let mut d2 = delta.x * delta.x + delta.y * delta.y;
            if too_close(d2) {
                delta = separation_nudge(i, j);
                d2 = delta.x * delta.x + delta.y * delta.y;
            }
            let distance = d2.sqrt();
            let push = delta * (params.repulsion / (d2 * distance));
            forces[i] = forces[i] + push;
            forces[j] = forces[j] - push;
        }
    }

    // Attraction along the links, including the undrawn capability ones.
    for link in graph.links() {
        if link.from == link.to || link.from >= n || link.to >= n {
            continue;
        }
        let mut delta = positions[link.to] - positions[link.from];
        let mut d2 = delta.x * delta.x + delta.y * delta.y;
        if too_close(d2) {
            delta = separation_nudge(link.from, link.to);
            d2 = delta.x * delta.x + delta.y * delta.y;
        }
        let distance = d2.sqrt();
        let stiffness = params.attraction * kind_stiffness(link.kind, params) * weight_scale(link);
        let pull = delta * (stiffness * (distance - params.ideal_edge) / distance);
        forces[link.from] = forces[link.from] + pull;
        forces[link.to] = forces[link.to] - pull;
    }

    // Gravity, and integrate.
    let degrees = graph.degrees();
    for i in 0..n {
        if pinned[i] {
            // A pinned node has no momentum to resume with when it is
            // released; it starts again from where the operator put it.
            velocities[i] = Point::ZERO;
            continue;
        }
        let force = forces[i] - positions[i] * params.gravity;
        let mass = node_mass(degrees.get(i).copied().unwrap_or(0));
        let velocity =
            ((velocities[i] + force * (1.0 / mass)) * params.damping).clamped(params.max_step);
        velocities[i] = velocity;
        positions[i] = positions[i] + velocity;
    }
}

/// How hard a node is to move: one, plus one per link pulling on it.
///
/// See the module's note on stability. Not a tunable, because it is not a
/// preference: it is the term that keeps the integrator's poles inside the
/// unit circle, and a knob that can be set to zero is a knob that will be.
fn node_mass(degree: u32) -> f64 {
    1.0 + degree as f64
}

/// Spring stiffness for a link kind, as a multiple of
/// [`LayoutParams::attraction`].
fn kind_stiffness(kind: EdgeKind, params: &LayoutParams) -> f64 {
    match kind {
        // The structural edge: this machine holds that node in its store.
        EdgeKind::Peer => 1.0,
        // Who introduced whom is history rather than topology. Pulling as
        // hard as a peer edge would drag introduced peers into a chain and
        // make the picture about provenance instead of about the mesh.
        EdgeKind::Observed => 0.6,
        // Work actually flowed along this one.
        EdgeKind::Delegation => 1.0,
        // A session belongs tight against the node running it.
        EdgeKind::Session => 1.5,
        // Clustering only, and never drawn.
        EdgeKind::Capability => params.cluster_attraction,
    }
}

/// How much a link's weight stiffens it: `1.0` at weight one, `2.0` from
/// [`WEIGHT_CAP`] upward.
fn weight_scale(link: &Link) -> f64 {
    1.0 + (link.weight.saturating_sub(1).min(WEIGHT_CAP) as f64) / (WEIGHT_CAP as f64)
}

/// Whether a squared distance is inside [`MIN_SEPARATION`], i.e. close enough
/// that the `1/d²` repulsion would blow up and the direction between the two
/// points is no longer meaningful.
///
/// Takes the *squared* distance because every caller already has it and the
/// square root is the expensive half; comparing against the squared threshold
/// is the same test without it.
fn too_close(d2: f64) -> bool {
    d2 < MIN_SEPARATION * MIN_SEPARATION
}

/// A deterministic direction to push two co-located nodes apart in.
fn separation_nudge(i: usize, j: usize) -> Point {
    let mixed = splitmix64(((i as u64) << 32) ^ (j as u64) ^ 0x5eed_0f5e_9a11_2b3c);
    let x = unit_from(mixed as u32);
    let y = unit_from((mixed >> 32) as u32);
    let length = (x * x + y * y).sqrt();
    if length < 1e-9 {
        // Both halves landed on the midpoint of their range. Vanishingly
        // unlikely, still a division by zero if it happens.
        Point::new(MIN_SEPARATION, 0.0)
    } else {
        Point::new(x / length * MIN_SEPARATION, y / length * MIN_SEPARATION)
    }
}

// ---------------------------------------------------------------------------
// The stateful wrapper
// ---------------------------------------------------------------------------

/// Positions, velocities and pins for one graph, held across frames.
///
/// The thin part. Everything interesting is in [`step_positions`]; this keeps
/// the buffers, reconciles them when the graph changes underneath, and answers
/// the two questions a renderer has that the force model does not: what is
/// under the pointer, and how big is the picture.
#[derive(Debug, Clone)]
pub struct Layout {
    keys: Vec<NodeKey>,
    positions: Vec<Point>,
    velocities: Vec<Point>,
    pinned: Vec<bool>,
    radii: Vec<f64>,
    params: LayoutParams,
    seed: u64,
}

impl Layout {
    /// A freshly seeded layout for `graph`.
    pub fn new(graph: &MeshGraph, seed: u64) -> Self {
        Self::with_params(graph, seed, LayoutParams::default())
    }

    /// A freshly seeded layout with the force model tuned.
    pub fn with_params(graph: &MeshGraph, seed: u64, params: LayoutParams) -> Self {
        let mut layout = Self {
            keys: Vec::new(),
            positions: Vec::new(),
            velocities: Vec::new(),
            pinned: Vec::new(),
            radii: Vec::new(),
            params,
            seed,
        };
        layout.sync(graph);
        layout
    }

    /// The force model in use.
    pub fn params(&self) -> &LayoutParams {
        &self.params
    }

    /// Retune the force model. Positions and pins are kept: the operator's
    /// arrangement is not the renderer's to throw away.
    pub fn set_params(&mut self, params: LayoutParams) {
        self.params = params;
    }

    /// Match the buffers to `graph`.
    ///
    /// Nodes that are still there keep their position, velocity and pin;
    /// nodes that appeared are seeded; nodes that went away are dropped. That
    /// is what makes a peer going stale, or a session ending, a change to one
    /// dot rather than a picture that jumps.
    pub fn sync(&mut self, graph: &MeshGraph) {
        if !self.matches(graph) {
            self.reconcile(graph);
        }
        // Radii track how heavy a node is, which changes without the node set
        // changing at all: one more delegation is the same graph with a
        // fatter dot and a bigger click target.
        self.radii.clear();
        self.radii.extend(graph.nodes().iter().map(node_radius));
    }

    /// Advance the simulation one step, syncing first.
    pub fn step(&mut self, graph: &MeshGraph) {
        self.sync(graph);
        step_positions(
            graph,
            &self.params,
            &mut self.positions,
            &mut self.velocities,
            &self.pinned,
        );
    }

    /// Advance the simulation `steps` times.
    pub fn run(&mut self, graph: &MeshGraph, steps: usize) {
        for _ in 0..steps {
            self.step(graph);
        }
    }

    /// Every position, in [`MeshGraph::nodes`] order.
    pub fn positions(&self) -> &[Point] {
        &self.positions
    }

    /// Where one node is.
    pub fn position_of(&self, key: &NodeKey) -> Option<Point> {
        self.index_of(key).map(|index| self.positions[index])
    }

    /// Hold a node at `at` until it is unpinned. Returns whether the node is
    /// in this layout.
    pub fn pin(&mut self, key: &NodeKey, at: Point) -> bool {
        let Some(index) = self.index_of(key) else {
            return false;
        };
        self.positions[index] = at;
        self.velocities[index] = Point::ZERO;
        self.pinned[index] = true;
        true
    }

    /// Let a node move again, from wherever it was pinned.
    pub fn unpin(&mut self, key: &NodeKey) -> bool {
        let Some(index) = self.index_of(key) else {
            return false;
        };
        self.pinned[index] = false;
        true
    }

    /// Whether a node is being held in place.
    pub fn is_pinned(&self, key: &NodeKey) -> bool {
        self.index_of(key)
            .map(|index| self.pinned[index])
            .unwrap_or(false)
    }

    /// Which node is under `at`, if any.
    ///
    /// Nearest-centre wins among everything within its own radius plus
    /// `slop`, so overlapping dots resolve to the one actually pointed at
    /// rather than to whichever the graph happens to list first. Needs no
    /// graph argument on purpose: it answers from the same buffers the last
    /// step wrote, so it cannot disagree with what is on screen.
    pub fn hit_test(&self, at: Point, slop: f64) -> Option<NodeKey> {
        let mut best: Option<(f64, usize)> = None;
        for (index, position) in self.positions.iter().enumerate() {
            let distance = position.distance_to(at);
            let reach = self.radii.get(index).copied().unwrap_or(BASE_RADIUS) + slop;
            if distance > reach {
                continue;
            }
            if best.is_none_or(|(closest, _)| distance < closest) {
                best = Some((distance, index));
            }
        }
        best.map(|(_, index)| self.keys[index].clone())
    }

    /// The box every node fits inside, including its radius.
    ///
    /// [`Rect::EMPTY`] for a layout with nothing in it, and never `NaN`: a
    /// renderer divides by this to fit a viewport.
    pub fn bounds(&self) -> Rect {
        let mut bounds: Option<Rect> = None;
        for (index, position) in self.positions.iter().enumerate() {
            if !position.is_finite() {
                continue;
            }
            let radius = self.radii.get(index).copied().unwrap_or(BASE_RADIUS);
            let min = Point::new(position.x - radius, position.y - radius);
            let max = Point::new(position.x + radius, position.y + radius);
            bounds = Some(match bounds {
                None => Rect { min, max },
                Some(current) => Rect {
                    min: Point::new(current.min.x.min(min.x), current.min.y.min(min.y)),
                    max: Point::new(current.max.x.max(max.x), current.max.y.max(max.y)),
                },
            });
        }
        bounds.unwrap_or(Rect::EMPTY)
    }

    /// Total kinetic energy left in the simulation.
    ///
    /// The cheap "has it settled" question, so a renderer inside a frame
    /// budget can stop stepping a graph that has stopped moving instead of
    /// burning 16ms a frame on a still picture.
    pub fn kinetic_energy(&self) -> f64 {
        self.velocities
            .iter()
            .map(|velocity| velocity.x * velocity.x + velocity.y * velocity.y)
            .sum()
    }

    fn index_of(&self, key: &NodeKey) -> Option<usize> {
        self.keys.iter().position(|held| held == key)
    }

    fn matches(&self, graph: &MeshGraph) -> bool {
        self.keys.len() == graph.len()
            && self
                .keys
                .iter()
                .zip(graph.nodes())
                .all(|(key, node)| *key == node.key)
    }

    fn reconcile(&mut self, graph: &MeshGraph) {
        let previous: BTreeMap<NodeKey, usize> = self
            .keys
            .iter()
            .enumerate()
            .map(|(index, key)| (key.clone(), index))
            .collect();
        let spread = spread_for(graph.len(), &self.params);

        let mut keys = Vec::with_capacity(graph.len());
        let mut positions = Vec::with_capacity(graph.len());
        let mut velocities = Vec::with_capacity(graph.len());
        let mut pinned = Vec::with_capacity(graph.len());
        for node in graph.nodes() {
            match previous.get(&node.key) {
                Some(&index) => {
                    positions.push(self.positions[index]);
                    velocities.push(self.velocities[index]);
                    pinned.push(self.pinned[index]);
                }
                None => {
                    positions.push(seed_position(&node.key, self.seed, spread));
                    velocities.push(Point::ZERO);
                    pinned.push(false);
                }
            }
            keys.push(node.key.clone());
        }
        self.keys = keys;
        self.positions = positions;
        self.velocities = velocities;
        self.pinned = pinned;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use chrono::{DateTime, Utc};

    use super::*;
    use crate::mesh::peer::synthetic_store;
    use crate::mesh::{Capability, Identity, Node, PeerStore, PeerText, Trust};
    use crate::plugins::graph::model::MeshGraph;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("timestamp")
    }

    fn local() -> Node {
        Identity::from_seed([0u8; 32]).announce("here", Capability::none())
    }

    /// A graph over `count` deterministic synthetic peers.
    fn synthetic(count: usize) -> MeshGraph {
        let now = at(0);
        MeshGraph::build(&local(), &synthetic_store(count, 1, now), now)
    }

    /// A digest of an arrangement, rounded before hashing.
    ///
    /// Rounded to four decimals rather than hashing the raw bits: the force
    /// model is bit-reproducible on any IEEE-754 machine, but a snapshot that
    /// fails on a one-ULP difference would be a snapshot people learn to
    /// re-bless without reading, which is worse than no snapshot. Four
    /// decimals is far tighter than any change to the model could hide in.
    fn digest(positions: &[Point]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for point in positions {
            for byte in format!("{:.4},{:.4};", point.x, point.y).as_bytes() {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }

    #[test]
    fn the_same_seed_lays_out_the_same_picture_every_time() {
        let graph = synthetic(20);
        let mut first = Layout::new(&graph, 42);
        let mut second = Layout::new(&graph, 42);
        assert_eq!(
            first.positions(),
            second.positions(),
            "seeding is a function"
        );

        first.run(&graph, 120);
        second.run(&graph, 120);
        assert_eq!(
            first.positions(),
            second.positions(),
            "the same seed and the same steps are the same arrangement, bit for bit"
        );

        let mut other = Layout::new(&graph, 43);
        other.run(&graph, 120);
        assert_ne!(
            digest(other.positions()),
            digest(first.positions()),
            "a different seed is a different arrangement"
        );
    }

    #[test]
    fn the_layout_of_a_known_mesh_is_the_arrangement_it_has_always_been() {
        // The snapshot. It fails on any change to the force model, the force
        // ordering, the seeding or the link derivation, which is the point:
        // all four are things that get "improved" by accident. To re-bless,
        // print the digest below and paste it in, having convinced yourself
        // the picture got better rather than merely different.
        let graph = synthetic(12);
        let mut layout = Layout::new(&graph, 7);
        layout.run(&graph, 200);
        assert_eq!(
            digest(layout.positions()),
            SETTLED_DIGEST,
            "the layout of a fixed mesh changed; the new digest is {:#018x} and the arrangement \
             behind it is {:?}",
            digest(layout.positions()),
            layout
                .positions()
                .iter()
                .map(|point| (format!("{:.4}", point.x), format!("{:.4}", point.y)))
                .collect::<Vec<_>>()
        );
    }

    /// Digest of `synthetic(12)` at seed 7 after 200 steps.
    const SETTLED_DIGEST: u64 = 0x4fbd_2b79_2b69_86a3;

    #[test]
    fn a_pinned_node_stays_put_and_still_pushes() {
        let graph = synthetic(12);
        let mut layout = Layout::new(&graph, 3);
        let anchor = graph.nodes()[0].key.clone();
        let neighbour = graph.nodes()[1].key.clone();

        assert!(layout.pin(&anchor, Point::new(25.0, -40.0)));
        assert!(layout.is_pinned(&anchor));
        let before = layout.position_of(&neighbour).expect("neighbour");
        layout.run(&graph, 100);

        assert_eq!(
            layout.position_of(&anchor),
            Some(Point::new(25.0, -40.0)),
            "a pinned node does not drift"
        );
        assert_ne!(
            layout.position_of(&neighbour),
            Some(before),
            "and everything else still arranges itself around it"
        );
        // The pin is what holds it: released, it moves like anything else.
        assert!(layout.unpin(&anchor));
        assert!(!layout.is_pinned(&anchor));
        layout.run(&graph, 50);
        assert_ne!(layout.position_of(&anchor), Some(Point::new(25.0, -40.0)));

        // A key that is not in this graph is a miss, not a panic.
        let stranger = NodeKey::Session(Identity::from_seed([99u8; 32]).id(), "nowhere".into());
        assert!(!layout.pin(&stranger, Point::ZERO));
        assert!(!layout.unpin(&stranger));
        assert!(!layout.is_pinned(&stranger));
        assert!(layout.position_of(&stranger).is_none());
    }

    #[test]
    fn an_empty_mesh_and_an_unreachable_peer_both_lay_out() {
        // Zero peers: one node, no links, no division by a zero extent.
        let now = at(0);
        let empty = MeshGraph::build(&local(), &PeerStore::ephemeral(), now);
        let mut layout = Layout::new(&empty, 11);
        layout.run(&empty, 60);
        assert_eq!(layout.positions().len(), 1);
        assert!(layout.positions()[0].is_finite());
        let bounds = layout.bounds();
        assert!(bounds.width().is_finite() && bounds.width() > 0.0);
        assert!(
            layout
                .hit_test(Point::new(10_000.0, 10_000.0), 4.0)
                .is_none()
        );

        // One peer, blocked, seen a second ago. It is drawn, it is placed,
        // and nothing about the layout makes it look reachable.
        let mut store = PeerStore::ephemeral();
        let id = Identity::from_seed([5u8; 32]).id();
        store.add(Node::new(id), now);
        store.record_trust(&id, Trust::Blocked).expect("block");
        store.mark_seen(&id, now);
        let graph = MeshGraph::build(&local(), &store, now);
        let mut layout = Layout::new(&graph, 11);
        layout.run(&graph, 60);
        assert_eq!(layout.positions().len(), 2);
        assert!(layout.positions().iter().all(|point| point.is_finite()));
        assert!(
            !graph
                .node(&NodeKey::Node(id))
                .expect("peer")
                .liveness
                .is_live()
        );
    }

    #[test]
    fn nodes_that_start_on_top_of_each_other_separate_instead_of_going_nan() {
        // The classic way a force layout dies: `1/d²` at `d == 0`. Every node
        // is dropped on the origin, which is also what a badly chosen seed
        // could do to a pair of them.
        let graph = synthetic(15);
        let mut layout = Layout::new(&graph, 1);
        for key in graph.nodes().iter().map(|node| node.key.clone()) {
            layout.pin(&key, Point::ZERO);
            layout.unpin(&key);
        }
        assert!(layout.positions().iter().all(|point| point == &Point::ZERO));

        layout.run(&graph, 200);
        assert!(
            layout.positions().iter().all(|point| point.is_finite()),
            "{:?}",
            layout.positions()
        );
        // …and they actually came apart, rather than all staying stacked.
        let mut closest = f64::INFINITY;
        for (i, a) in layout.positions().iter().enumerate() {
            for b in &layout.positions()[i + 1..] {
                closest = closest.min(a.distance_to(*b));
            }
        }
        assert!(closest > 1.0, "closest pair is {closest}");
    }

    #[test]
    fn hit_testing_picks_the_node_under_the_pointer() {
        let graph = synthetic(6);
        let mut layout = Layout::new(&graph, 2);
        let first = graph.nodes()[0].key.clone();
        let second = graph.nodes()[1].key.clone();
        let third = graph.nodes()[2].key.clone();
        layout.pin(&first, Point::new(0.0, 0.0));
        layout.pin(&second, Point::new(14.0, 0.0));
        layout.pin(&third, Point::new(500.0, 500.0));

        // Dead centre, and just off it.
        assert_eq!(
            layout.hit_test(Point::new(0.0, 0.0), 0.0),
            Some(first.clone())
        );
        assert_eq!(
            layout.hit_test(Point::new(500.0, 500.0), 0.0),
            Some(third.clone())
        );
        // Between two overlapping dots: the nearer centre wins.
        assert_eq!(layout.hit_test(Point::new(9.0, 0.0), 0.0), Some(second));
        assert_eq!(layout.hit_test(Point::new(5.0, 0.0), 0.0), Some(first));
        // Empty space is a miss, and slop is what makes a near miss a hit.
        assert_eq!(layout.hit_test(Point::new(0.0, 200.0), 0.0), None);
        assert!(layout.hit_test(Point::new(0.0, 14.0), 0.0).is_none());
        assert!(layout.hit_test(Point::new(0.0, 14.0), 6.0).is_some());
    }

    #[test]
    fn adding_and_dropping_a_node_leaves_the_others_where_they_were() {
        // The graph is rebuilt from the store every time anything changes, so
        // "one peer answered" must not be "the whole picture jumped".
        let now = at(0);
        let mut store = synthetic_store(10, 4, now);
        let graph = MeshGraph::build(&local(), &store, now);
        let mut layout = Layout::new(&graph, 8);
        layout.run(&graph, 80);
        let before: Vec<(NodeKey, Point)> = graph
            .nodes()
            .iter()
            .map(|node| {
                (
                    node.key.clone(),
                    layout.position_of(&node.key).expect("placed"),
                )
            })
            .collect();

        let newcomer = Identity::from_seed([77u8; 32]).id();
        store.add(Node::new(newcomer), now);
        let grown = MeshGraph::build(&local(), &store, now);
        layout.sync(&grown);
        assert_eq!(layout.positions().len(), grown.len());
        for (key, position) in &before {
            assert_eq!(
                layout.position_of(key),
                Some(*position),
                "{key:?} moved when a different peer was added"
            );
        }
        assert!(layout.position_of(&NodeKey::Node(newcomer)).is_some());

        // And the same on the way out.
        store.forget(&newcomer);
        let shrunk = MeshGraph::build(&local(), &store, now);
        layout.sync(&shrunk);
        assert!(layout.position_of(&NodeKey::Node(newcomer)).is_none());
        for (key, position) in &before {
            assert_eq!(layout.position_of(key), Some(*position), "{key:?} moved");
        }
    }

    #[test]
    fn a_session_appearing_does_not_disturb_the_mesh_around_it() {
        let now = at(0);
        let store = synthetic_store(8, 6, now);
        let graph = MeshGraph::build(&local(), &store, now);
        let mut layout = Layout::new(&graph, 9);
        layout.run(&graph, 60);
        let anchor = graph.nodes()[3].key.clone();
        let before = layout.position_of(&anchor).expect("placed");

        let mut with_session = graph.clone();
        let NodeKey::Node(owner) = graph.nodes()[3].key.clone() else {
            panic!("a peer vertex");
        };
        assert!(with_session.add_session(owner, &PeerText::sanitize("s-7")));
        layout.sync(&with_session);
        assert_eq!(layout.position_of(&anchor), Some(before));
        assert!(
            layout
                .position_of(&NodeKey::Session(owner, "s-7".to_string()))
                .is_some()
        );
    }

    #[test]
    fn the_layout_settles_rather_than_flying_apart() {
        let graph = synthetic(50);
        let mut layout = Layout::new(&graph, 12);
        layout.run(&graph, 400);
        let bounds = layout.bounds();
        assert!(bounds.width().is_finite() && bounds.height().is_finite());
        assert!(
            bounds.width() < 20_000.0 && bounds.height() < 20_000.0,
            "the mesh drifted off the canvas: {bounds:?}"
        );
        assert!(
            layout.kinetic_energy() < graph.len() as f64,
            "still moving after 400 steps: {}",
            layout.kinetic_energy()
        );
        assert!(layout.positions().iter().all(|point| point.is_finite()));
    }

    #[test]
    fn fifty_nodes_step_inside_the_frame_budget() {
        // The plan's bar: 50 synthetic nodes at 60fps with software
        // rendering. 16.6ms is the whole frame, so the layout gets a slice of
        // it and the threshold here is deliberately a fraction: if the step
        // ever costs half a frame there is nothing left to draw with.
        //
        // Held loose enough to survive a debug build on a loaded CI box and
        // tight enough that an accidental `O(n³)` or a per-frame allocation
        // in the inner loop fails it.
        let graph = synthetic(50);
        assert_eq!(graph.len(), 51, "fifty peers plus this machine");
        let mut layout = Layout::new(&graph, 1);
        layout.run(&graph, 20); // warm up: page in the buffers

        let steps = 120;
        let started = Instant::now();
        layout.run(&graph, steps);
        let per_step = started.elapsed() / steps as u32;
        println!("layout step at n={}: {per_step:?}", graph.len());
        assert!(
            per_step < std::time::Duration::from_micros(4_000),
            "one step at n={} took {per_step:?}, which does not leave a frame to draw in",
            graph.len()
        );
    }

    #[test]
    fn the_cost_of_a_step_is_measured_at_five_hundred_nodes_too() {
        // Not a bar the plan sets, and reported rather than enforced: 500
        // nodes is ten times the target mesh and the repulsion pass is
        // quadratic, so this exists to make the shape of that curve visible
        // when somebody proposes raising the node count. The assertion is
        // only that it is still a layout and not a hang.
        let graph = synthetic(500);
        let mut layout = Layout::new(&graph, 1);
        layout.run(&graph, 5);

        let steps = 20;
        let started = Instant::now();
        layout.run(&graph, steps);
        let per_step = started.elapsed() / steps as u32;
        println!("layout step at n={}: {per_step:?}", graph.len());
        assert!(
            per_step < std::time::Duration::from_millis(250),
            "one step at n={} took {per_step:?}",
            graph.len()
        );
        assert!(layout.positions().iter().all(|point| point.is_finite()));
    }
}
