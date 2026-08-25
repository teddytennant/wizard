//! The one transform between the layout's world and the canvas's pixels.
//!
//! It exists as its own module for a reason that is not tidiness. The layout is
//! `f64` and unbounded; the canvas is `f32` and bounded by whatever the window
//! is this frame; and [`Layout::hit_test`](crate::graph::Layout::hit_test)
//! takes **world** coordinates, so a click cannot be answered without inverting
//! whatever the drawing did. A renderer that computes the forward map inline in
//! `draw` and the inverse inline in `update` has written the same arithmetic
//! twice, and the day one of them grows a margin the other does not is the day
//! clicking a node selects its neighbour. So there is one struct, the two
//! directions are each other's algebraic inverse, and a test asserts the round
//! trip rather than eyeballing it.
//!
//! # What is stored
//!
//! A world point and a scale, not a matrix. The camera is a pan and a uniform
//! zoom and it will never be anything else: there is no rotation in a graph
//! explorer, and a non-uniform zoom would make a circular node an ellipse and
//! [`node_radius`](crate::graph::layout::node_radius) a lie. Two numbers and a
//! point say exactly that and nothing more, which is also why
//! [`Viewport::to_world`] can be written down rather than inverted numerically.
//!
//! # Why the scale is clamped
//!
//! [`Layout::bounds`](crate::graph::Layout::bounds) is documented never to be
//! `NaN`, but it *can* be [`Rect::EMPTY`], and a fit against a zero-width box
//! is a division by zero. Clamping into `[MIN_SCALE, MAX_SCALE]` turns every
//! degenerate input into a picture rather than into an infinity that empties
//! the canvas — the same posture the layout takes toward two nodes at the same
//! point.

use iced::{Point as Screen, Size, Vector};

use crate::graph::{Point as World, Rect};

/// Smallest pixels-per-world-unit the camera will hold.
///
/// At this zoom a default node is a third of a pixel across, which is already
/// past the point of usefulness; going further is how a scroll wheel loses a
/// graph somewhere inside a rounding error.
pub const MIN_SCALE: f64 = 0.04;

/// Largest pixels-per-world-unit the camera will hold.
pub const MAX_SCALE: f64 = 8.0;

/// Largest scale an *automatic* fit will choose.
///
/// A fit shrinks a graph that outgrew the canvas; it must never magnify one
/// that did not. Without this the rule "fill the canvas" runs the other way
/// on a sparse graph: a machine with no peers yet has a single node whose
/// bounds are one radius across, so the fit divides the canvas by ~25 world
/// units, pins to [`MAX_SCALE`], and draws one dot two hundred pixels wide
/// with its label swallowed by the disc. That is the first thing a new user
/// sees, because "no peers yet" is where everyone starts.
///
/// One is the natural size and not an arbitrary cap: a node is
/// [`BASE_RADIUS`](crate::graph::layout::BASE_RADIUS) = 9 world units, so it
/// draws as an 18-pixel dot (25 for the local one), and
/// [`LayoutParams::ideal_edge`](crate::graph::LayoutParams) puts linked nodes
/// 70 apart — a readable picture at any node count. [`MAX_SCALE`] still
/// governs the wheel, so zooming *in* past this is a thing the operator may
/// ask for. It is only not a thing the camera does on its own.
pub const FIT_MAX_SCALE: f64 = 1.0;

/// Fraction of the canvas a fitted graph is allowed to fill.
///
/// The remainder is the margin the labels hang in: a node's dot is inside
/// [`Layout::bounds`](crate::graph::Layout::bounds) and the text beside it is
/// not, so a fit with no slack clips every name on the rim.
pub const FIT_FILL: f64 = 0.82;

/// How far outside a node's drawn edge a click still counts as hitting it, in
/// **screen** pixels.
///
/// Screen rather than world, and that is the whole point of
/// [`Viewport::hit_slop`]: a fixed world slop is a generous target zoomed in
/// and an unhittable one zoomed out, while a person's aim is in pixels at every
/// zoom.
pub const HIT_SLOP_PIXELS: f64 = 5.0;

/// Multiplier applied per line of scroll wheel.
pub const ZOOM_PER_LINE: f64 = 1.12;

/// Where the camera is and how far away.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// The world point drawn at the centre of the canvas.
    pub center: World,
    /// Screen pixels per world unit. Always inside `[MIN_SCALE, MAX_SCALE]`.
    pub scale: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            center: World::ZERO,
            scale: 1.0,
        }
    }
}

impl Viewport {
    /// The camera that shows all of `bounds` inside `canvas`.
    ///
    /// Falls back to the default camera for a canvas or a graph with no extent,
    /// which happens on the first frame (nothing has been laid out yet) and for
    /// a layout holding nothing at all. A fallback rather than an assertion
    /// because this runs in a draw path, where a wrong picture beats a dead
    /// window.
    pub fn fit(bounds: Rect, canvas: Size) -> Self {
        let (width, height) = (bounds.width(), bounds.height());
        let usable = width > 0.0
            && height > 0.0
            && canvas.width > 0.0
            && canvas.height > 0.0
            && bounds.center().is_finite();
        if !usable {
            return Self::default();
        }
        let scale = ((canvas.width as f64 / width).min(canvas.height as f64 / height) * FIT_FILL)
            .clamp(MIN_SCALE, FIT_MAX_SCALE);
        Self {
            center: bounds.center(),
            scale,
        }
    }

    /// Where a world point lands on the canvas.
    pub fn to_screen(&self, world: World, canvas: Size) -> Screen {
        Screen::new(
            ((world.x - self.center.x) * self.scale) as f32 + canvas.width / 2.0,
            ((world.y - self.center.y) * self.scale) as f32 + canvas.height / 2.0,
        )
    }

    /// Where a canvas point lands in the world. The exact inverse of
    /// [`Viewport::to_screen`].
    pub fn to_world(&self, screen: Screen, canvas: Size) -> World {
        World::new(
            (screen.x - canvas.width / 2.0) as f64 / self.scale + self.center.x,
            (screen.y - canvas.height / 2.0) as f64 / self.scale + self.center.y,
        )
    }

    /// A world length in pixels: radii, stroke widths, text sizes.
    pub fn to_pixels(&self, world_length: f64) -> f32 {
        (world_length * self.scale) as f32
    }

    /// The world slop [`Layout::hit_test`](crate::graph::Layout::hit_test)
    /// should be given, so that a click lands within [`HIT_SLOP_PIXELS`] of the
    /// drawn dot whatever the zoom is.
    pub fn hit_slop(&self) -> f64 {
        HIT_SLOP_PIXELS / self.scale
    }

    /// Drag the picture by a screen-space delta.
    ///
    /// The content follows the pointer, so the camera moves the other way. That
    /// sign is the difference between "grab the paper" and "steer a camera",
    /// and everyone who has used a map expects the former.
    pub fn pan_by(&mut self, delta: Vector) {
        self.center = World::new(
            self.center.x - delta.x as f64 / self.scale,
            self.center.y - delta.y as f64 / self.scale,
        );
    }

    /// Zoom by `factor`, keeping whatever world point is under `anchor` under
    /// it afterwards.
    ///
    /// The fixed point is what makes a scroll wheel feel like a zoom rather
    /// than a jump: without it, zooming in on a node in the corner walks the
    /// node off the canvas.
    pub fn zoom_at(&mut self, anchor: Screen, factor: f64, canvas: Size) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        let held = self.to_world(anchor, canvas);
        self.scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        // Solve `to_screen(held) == anchor` for the centre, at the new scale.
        self.center = World::new(
            held.x - (anchor.x - canvas.width / 2.0) as f64 / self.scale,
            held.y - (anchor.y - canvas.height / 2.0) as f64 / self.scale,
        );
    }
}

/// One scroll gesture as a zoom factor.
///
/// Lines and pixels are the two shapes a wheel reports and they are not the
/// same unit: a notch is one line or roughly fifty pixels depending on the
/// device, so applying [`ZOOM_PER_LINE`] to a pixel delta would zoom fifty
/// notches per notch on a trackpad.
pub fn zoom_factor(delta: iced::mouse::ScrollDelta) -> f64 {
    let lines = match delta {
        iced::mouse::ScrollDelta::Lines { y, .. } => y as f64,
        iced::mouse::ScrollDelta::Pixels { y, .. } => y as f64 / 50.0,
    };
    if !lines.is_finite() {
        return 1.0;
    }
    // Bounded per event: a fling on a trackpad reports a very large pixel
    // delta, and an unbounded exponent turns one flick into MAX_SCALE.
    ZOOM_PER_LINE.powf(lines.clamp(-8.0, 8.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas() -> Size {
        Size::new(800.0, 600.0)
    }

    /// The property the whole module exists for: hit testing inverts drawing.
    /// A margin added to one direction and not the other shows up here as a
    /// drift, at every zoom and every pan.
    #[test]
    fn screen_and_world_are_each_others_inverse() {
        let mut viewport = Viewport::fit(
            Rect {
                min: World::new(-300.0, -180.0),
                max: World::new(420.0, 260.0),
            },
            canvas(),
        );
        viewport.pan_by(Vector::new(37.0, -19.0));
        viewport.zoom_at(Screen::new(123.0, 456.0), 1.7, canvas());

        for world in [
            World::ZERO,
            World::new(-300.0, -180.0),
            World::new(420.0, 260.0),
            World::new(1e4, -1e4),
        ] {
            let back = viewport.to_world(viewport.to_screen(world, canvas()), canvas());
            assert!(
                (back.x - world.x).abs() < 1e-3 && (back.y - world.y).abs() < 1e-3,
                "{world:?} came back as {back:?}"
            );
        }
    }

    /// A fit puts the graph in the middle and leaves room for the labels.
    ///
    /// The graph here is deliberately bigger than the canvas (2000×1000 in
    /// 800×600), because filling the canvas is what a fit does when it has to
    /// *shrink*. A graph smaller than the canvas is the other case and is
    /// covered by `a_fit_never_magnifies_a_graph_smaller_than_the_canvas`.
    #[test]
    fn a_fit_centres_the_graph_and_keeps_a_margin() {
        let bounds = Rect {
            min: World::new(-1000.0, -500.0),
            max: World::new(1000.0, 500.0),
        };
        let viewport = Viewport::fit(bounds, canvas());
        assert_eq!(viewport.center, bounds.center());

        let top_left = viewport.to_screen(bounds.min, canvas());
        let bottom_right = viewport.to_screen(bounds.max, canvas());
        assert!(top_left.x > 0.0 && top_left.y > 0.0, "{top_left:?}");
        assert!(
            bottom_right.x < canvas().width && bottom_right.y < canvas().height,
            "{bottom_right:?}"
        );
        // The whole graph, not a slab of it: the fit is driven by whichever
        // axis is tighter. A 200×100 graph in an 800×600 canvas is bound by
        // its width (4 px per unit against 6), so the width is what fills.
        let drawn_width = bottom_right.x - top_left.x;
        let drawn_height = bottom_right.y - top_left.y;
        assert!(
            (drawn_width - canvas().width * FIT_FILL as f32).abs() < 1.0,
            "{drawn_width}"
        );
        assert!(
            drawn_height < canvas().height * FIT_FILL as f32,
            "the looser axis has slack: {drawn_height}"
        );
    }

    /// The case the `fit` rule used to get backwards: one node, no peers.
    ///
    /// `Layout::bounds` around a single local node is one diameter across —
    /// 25.2 world units, `BASE_RADIUS * 1.4 * 2`. Dividing an 800×600 canvas
    /// by that asks for a scale near 20, which pinned to `MAX_SCALE` and drew
    /// a single dot roughly 200 pixels wide, its label starting inside the
    /// disc. This is what a machine with no peers yet shows on the first
    /// open, so it was also the most-seen frame in the screen.
    #[test]
    fn a_fit_never_magnifies_a_graph_smaller_than_the_canvas() {
        let radius = 9.0 * 1.4;
        let one_node = Rect {
            min: World::new(-radius, -radius),
            max: World::new(radius, radius),
        };
        let viewport = Viewport::fit(one_node, canvas());
        assert!(
            viewport.scale <= FIT_MAX_SCALE,
            "a fit magnified a graph that already fitted: {}",
            viewport.scale
        );
        // And the dot it draws is a dot: a couple of dozen pixels, not a
        // couple of hundred.
        let drawn = (one_node.width() * viewport.scale) as f32;
        assert!(
            (10.0..=40.0).contains(&drawn),
            "one node should draw as a dot, got {drawn} px"
        );

        // The margin still exists, and the node is still centred.
        assert_eq!(viewport.center, one_node.center());
        let at = viewport.to_screen(one_node.center(), canvas());
        assert!((at.x - canvas().width / 2.0).abs() < 0.5);
        assert!((at.y - canvas().height / 2.0).abs() < 0.5);
    }

    /// `Rect::EMPTY` is what a layout with nothing in it returns, and a zero
    /// canvas is what the first frame has. Neither may produce an infinity.
    #[test]
    fn a_degenerate_fit_is_a_camera_rather_than_an_infinity() {
        for (bounds, canvas) in [
            (Rect::EMPTY, canvas()),
            (
                Rect {
                    min: World::new(-1.0, -1.0),
                    max: World::new(1.0, 1.0),
                },
                Size::new(0.0, 0.0),
            ),
        ] {
            let viewport = Viewport::fit(bounds, canvas);
            assert!(viewport.scale.is_finite() && viewport.scale > 0.0);
            assert!(viewport.center.is_finite());
        }
    }

    /// Zooming holds the point under the pointer still. Without it, zooming
    /// into a node in the corner walks that node off the canvas.
    #[test]
    fn zooming_keeps_the_point_under_the_pointer_where_it_was() {
        let mut viewport = Viewport::default();
        let anchor = Screen::new(700.0, 90.0);
        let held = viewport.to_world(anchor, canvas());
        for factor in [1.4, 1.4, 0.5, 3.0] {
            viewport.zoom_at(anchor, factor, canvas());
            let now = viewport.to_screen(held, canvas());
            assert!(
                (now.x - anchor.x).abs() < 1e-2 && (now.y - anchor.y).abs() < 1e-2,
                "at {factor}: {now:?} should still be {anchor:?}"
            );
        }
    }

    /// The clamps hold at both ends, and a wheel spun forever does not walk the
    /// scale into an infinity or a zero.
    #[test]
    fn the_zoom_is_bounded_at_both_ends() {
        let mut viewport = Viewport::default();
        for _ in 0..500 {
            viewport.zoom_at(Screen::new(10.0, 10.0), 2.0, canvas());
        }
        assert_eq!(viewport.scale, MAX_SCALE);
        for _ in 0..500 {
            viewport.zoom_at(Screen::new(10.0, 10.0), 0.5, canvas());
        }
        assert_eq!(viewport.scale, MIN_SCALE);
        assert!(viewport.center.is_finite());

        // A factor that is not a factor changes nothing rather than poisoning
        // the camera with a NaN that empties the canvas forever.
        let before = viewport;
        viewport.zoom_at(Screen::new(1.0, 1.0), f64::NAN, canvas());
        viewport.zoom_at(Screen::new(1.0, 1.0), 0.0, canvas());
        assert_eq!(viewport, before);
    }

    /// The pointer's slop is in pixels, so the target is the same size on
    /// screen however far out the graph is zoomed.
    #[test]
    fn the_hit_slop_is_constant_in_pixels() {
        let mut viewport = Viewport::default();
        assert_eq!(viewport.hit_slop() * viewport.scale, HIT_SLOP_PIXELS);
        viewport.scale = 0.1;
        assert_eq!(viewport.hit_slop(), HIT_SLOP_PIXELS / 0.1);
        assert!(
            viewport.hit_slop() > HIT_SLOP_PIXELS,
            "zoomed out, a pixel is worth more world"
        );
    }

    /// Dragging moves the content with the pointer.
    #[test]
    fn panning_moves_the_picture_with_the_pointer() {
        let mut viewport = Viewport::default();
        let world = World::new(20.0, 20.0);
        let before = viewport.to_screen(world, canvas());
        viewport.pan_by(Vector::new(50.0, 0.0));
        let after = viewport.to_screen(world, canvas());
        assert!(
            (after.x - before.x - 50.0).abs() < 1e-3,
            "{before:?} -> {after:?}"
        );
    }

    /// Lines and pixels are different units and must not be applied as one.
    #[test]
    fn a_wheel_and_a_trackpad_zoom_by_comparable_amounts() {
        let wheel = zoom_factor(iced::mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 });
        let trackpad = zoom_factor(iced::mouse::ScrollDelta::Pixels { x: 0.0, y: 50.0 });
        assert!((wheel - trackpad).abs() < 1e-9, "{wheel} vs {trackpad}");
        assert!(wheel > 1.0);
        assert!(zoom_factor(iced::mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 }) < 1.0);
        // A trackpad fling is one gesture, not a hundred notches.
        let fling = zoom_factor(iced::mouse::ScrollDelta::Pixels {
            x: 0.0,
            y: 40_000.0,
        });
        assert!(fling <= ZOOM_PER_LINE.powi(8) + 1e-9, "{fling}");
        assert_eq!(
            zoom_factor(iced::mouse::ScrollDelta::Lines {
                x: 0.0,
                y: f32::NAN
            }),
            1.0
        );
    }
}
