//! The graph explorer: the mesh as a picture that does not lie.
//!
//! # Deferred: this module is not reachable from the window
//!
//! It ships compiled and tested but with no way in — no [`Screen`] variant, no
//! [`Message`] arm, no button in the sidebar. It was too unfinished to put in
//! front of users in 2.0 and is held for a later release.
//!
//! It is wired out rather than deleted because the part that is finished is the
//! part that was hard: the model and the layout below it are honest and
//! snapshot-tested, and they keep building and keep running their tests exactly
//! as before. `pub` is what stops the compiler calling this dead code.
//!
//! Putting it back is the four seams that were removed from
//! [`crate::native`], and nothing else:
//!
//! 1. `Screen::Mesh`, and a `view` arm stacking [`Explorer::view`] over `body`.
//! 2. `Message::Graph(graph::Message)`, with the `Close` arm routed to
//!    `Message::Escape` — the explorer does not own the screen it is on.
//! 3. `App.explorer`, built in `run` from `mesh::cli::open` (the ledger must be
//!    the same object on both halves; see its docs) and batched into
//!    `subscription` for the layout timer.
//! 4. `sidebar::Message::OpenMesh` and its `chrome::action` in the head row,
//!    which refreshes on open rather than on a timer.
//!
//! ---
//!
//! S1.1 of the v2 plan. Everything
//! *modelled* about the mesh already exists — [`crate::plugins::graph::model`] decides
//! liveness once and honestly, [`crate::plugins::graph::layout`] is a deterministic,
//! snapshot-tested force simulation with pinning and hit testing — so this
//! module is a renderer and nothing else. It adds three things that layer
//! deliberately does not have: a colour mapping ([`paint`]), a transform
//! between the layout's `f64` world and the canvas's `f32` pixels
//! ([`viewport`]), and the widgets ([`canvas`], [`inspector`]).
//!
//! ```text
//!   Mesh ──build──▶ MeshGraph ──▶ GraphView ──▶ GraphCanvas   (edges, nodes)
//!    ▲                  │            │  ▲  │
//!    │                  └─inspect────┼──┘  └─▶ inspector      (one node, in words)
//!    └──── set_trust ◀──── Revoke ◀──┘
//! ```
//!
//! # The rule this screen is judged by
//!
//! > A graph that is beautiful and lies about who is online is worse than a
//! > plain one that does not.
//!
//! [`Liveness::is_live`](crate::plugins::graph::Liveness::is_live) is the only predicate
//! allowed to make a node draw as up, and [`paint::node_paint`] is the only
//! place it is consulted. Trust never implies liveness: a peer a human trusted,
//! that has not answered, draws exactly as un-live as a stranger that has not
//! answered. `paint::tests::nothing_reads_as_up_without_is_live` is what keeps
//! that true over every combination the model can produce, in both shipped
//! themes.
//!
//! # A still graph costs nothing
//!
//! The layout is stepped from a 60Hz [`iced::time::every`] subscription that
//! [`GraphView::needs_step`] switches off once
//! [`kinetic_energy`](crate::plugins::graph::Layout::kinetic_energy) falls under
//! [`view::SETTLE_ENERGY`]. Not a throttle — the subscription is *dropped*, so
//! a settled explorer schedules no timer, wakes no thread and redraws nothing.
//!
//! # What is not here, on purpose
//!
//! Animation and time scrubbing over delegation history. The plan puts both in
//! 2.1 behind "a static graph with a good inspector and correct staleness", and
//! a screen that half-implemented them would be spending the budget the
//! staleness indication is supposed to have.

pub mod canvas;
pub mod inspector;
pub mod paint;
pub mod view;
pub mod viewport;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Element, Length, Padding, Point as Screen, Size, Subscription, Task};
use tokio::sync::Mutex;

use crate::mesh::{Mesh, NodeId, Trust};
use crate::native::theme::Palette;
use crate::plugins::graph::{Liveness, MeshGraph, NodeKey};
use crate::theme::Token;

use canvas::GraphCanvas;
use view::{CapabilityFilter, GraphView};

/// Body text size on this screen, matching the transcript's.
pub const BODY: f32 = 15.0;
/// The size of everything that is a value rather than prose.
pub const MONO: f32 = 12.0;
/// Node labels on the canvas.
pub const LABEL_SIZE: f32 = 11.0;

/// The step interval. 60fps, which is the plan's bar.
pub const FRAME: Duration = Duration::from_millis(16);

/// What the explorer can be told.
///
/// The pointer messages carry the canvas size they were measured in. See
/// [`canvas`]'s module header: the transform is meaningless without it, and a
/// size that travelled separately would answer the first click after a resize
/// against the previous frame's geometry.
#[derive(Debug, Clone)]
pub enum Message {
    /// The canvas is a different size than it was.
    Resized(Size),
    /// A button went down on the canvas.
    Press(Screen, Size),
    /// The pointer moved with the button down.
    Drag(Screen, Size),
    /// The button came up.
    Release,
    /// A wheel or trackpad gesture, as a zoom factor.
    Zoom(Screen, f64, Size),
    /// One tick of the layout.
    Step,
    /// Move the selection, from the inspector's links.
    Select(NodeKey),
    /// Let a pinned node move again.
    Unpin(NodeKey),
    /// Put the camera back on the whole graph.
    Fit,
    /// Show only the advertisers of one capability, or everybody.
    Filter(Option<CapabilityFilter>),
    /// Put a value on the clipboard.
    Copy(String),
    /// Block a peer and drop its live streams.
    Revoke(NodeId),
    /// A rebuilt snapshot, from a revocation or a refresh.
    Rebuilt(Result<MeshGraph, String>),
    /// Re-read the store.
    Refresh,
    /// Leave the explorer and go back to the chat.
    ///
    /// Handled by [`crate::native`], not here: the explorer does not own the
    /// screen it is drawn on, so all it can do is say it is finished.
    Close,
}

/// The screen.
pub struct Explorer {
    /// The one object every mutation with a security consequence goes through.
    /// Behind a lock because the revocation is asynchronous and the view is
    /// not.
    mesh: Arc<Mutex<Mesh>>,
    view: GraphView,
    palette: Palette,
    /// The last thing that went wrong, shown in the header until the next
    /// action. A revocation that failed must not look like one that worked.
    notice: Option<String>,
    /// Whether a mesh operation is in flight, so the controls that would race
    /// it are inert.
    busy: bool,
}

impl Explorer {
    /// Open the explorer over a mesh, as of `now`.
    ///
    /// Async because building the first snapshot reads the store behind the
    /// lock, and because that is the only work here that can block: everything
    /// after it is arithmetic and drawing.
    pub async fn open(mesh: Arc<Mutex<Mesh>>, now: DateTime<Utc>) -> Self {
        let graph = snapshot(&mesh, now).await;
        Self::with_graph(mesh, graph)
    }

    /// The explorer over a graph that has already been built. The seam the
    /// headless tests drive.
    pub fn with_graph(mesh: Arc<Mutex<Mesh>>, graph: MeshGraph) -> Self {
        // Seeded from the local node's own identity, so the arrangement is the
        // same every time this machine opens the screen and different from the
        // arrangement on somebody else's. `seed_position` is keyed off node
        // identity, so this is the only global the layout has.
        let seed = graph
            .nodes()
            .first()
            .map(|node| seed_of(&node.key))
            .unwrap_or_default();
        Self {
            mesh,
            view: GraphView::new(graph, seed),
            palette: Palette::active(),
            notice: None,
            busy: false,
        }
    }

    /// The view, for tests and for a host that wants to ask what is selected.
    pub fn view_state(&self) -> &GraphView {
        &self.view
    }

    /// The last failure, if the most recent mesh operation had one.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Apply one message.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Resized(size) => self.view.resize(size),
            Message::Press(at, size) => {
                self.view.resize(size);
                self.view.press(at);
            }
            Message::Drag(at, size) => {
                self.view.resize(size);
                self.view.drag_to(at);
            }
            Message::Release => self.view.release(),
            Message::Zoom(at, factor, size) => {
                self.view.resize(size);
                self.view.zoom(at, factor);
            }
            Message::Step => self.view.step(),
            Message::Select(key) => self.view.select(Some(key)),
            Message::Unpin(key) => self.view.unpin(&key),
            Message::Fit => self.view.fit(),
            Message::Filter(filter) => self.view.set_filter(filter),
            Message::Copy(value) => return iced::clipboard::write(value),
            Message::Revoke(id) => {
                self.busy = true;
                self.notice = None;
                let mesh = Arc::clone(&self.mesh);
                return Task::perform(
                    async move { revoke_and_rebuild(mesh, id, Utc::now()).await },
                    Message::Rebuilt,
                );
            }
            // Nothing to do here; `native::update` reads this one and puts
            // the chat back. Matched explicitly so the compiler keeps this
            // honest if the enum grows.
            Message::Close => {}
            Message::Refresh => {
                let mesh = Arc::clone(&self.mesh);
                return Task::perform(
                    async move { Ok(snapshot(&mesh, Utc::now()).await) },
                    Message::Rebuilt,
                );
            }
            Message::Rebuilt(result) => {
                self.busy = false;
                match result {
                    Ok(graph) => self.view.replace(graph),
                    Err(why) => self.notice = Some(why),
                }
            }
        }
        Task::none()
    }

    /// The layout's clock, and nothing else.
    ///
    /// Dropped entirely once the graph has settled: see the module header.
    pub fn subscription(&self) -> Subscription<Message> {
        if self.view.needs_step() {
            iced::time::every(FRAME).map(|_| Message::Step)
        } else {
            Subscription::none()
        }
    }

    /// Draw the screen: header, canvas, inspector.
    pub fn view(&self) -> Element<'_, Message> {
        let body = row![
            // Clipped: a canvas draws wherever its geometry says, and a node
            // near the right-hand edge put its label straight across the
            // inspector's rows — text over text, on the one screen where that
            // text is an identity somebody is deciding whether to trust.
            container(
                iced::widget::canvas(GraphCanvas {
                    view: &self.view,
                    palette: &self.palette,
                })
                .width(Length::Fill)
                .height(Length::Fill)
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .clip(true),
            self.side(),
        ]
        .spacing(12);

        container(column![self.header(), body].spacing(10))
            .padding(Padding::new(12.0))
            .width(Length::Fill)
            .height(Length::Fill)
            .style({
                let canvas = self.palette.canvas;
                move |_theme| container::Style {
                    background: Some(iced::Background::Color(canvas)),
                    ..container::Style::default()
                }
            })
            .into()
    }

    /// The count of every liveness state, the legend, and the two controls that
    /// are not about one node.
    fn header(&self) -> Element<'_, Message> {
        let mut counts = row![].spacing(14);
        for liveness in [
            Liveness::Here,
            Liveness::Live,
            Liveness::Stale,
            Liveness::Unseen,
            Liveness::Unreachable,
        ] {
            let count = self.view.graph().count_of(liveness);
            counts = counts.push(
                text(format!("{} {}", count, liveness.label()))
                    .size(MONO)
                    // One line each. Without this the clipped container
                    // wraps the last count onto a second row instead of
                    // truncating it, and the header grows a line — which
                    // moves the canvas under it every time the window
                    // crosses that width.
                    .wrapping(iced::widget::text::Wrapping::None)
                    // The legend is coloured by the same function the canvas
                    // uses, so a swatch cannot drift from the dot it explains.
                    .color(paint::liveness_color(liveness, &self.palette)),
            );
        }

        let mut controls = row![
            // `Fill` and clipped, not `Shrink`, and it is the spacer too. A
            // `row!` lays its `Fill` children out last, from whatever the
            // fixed-size ones left, so a `Shrink` block at index 0 takes its
            // full intrinsic width first and everything after it is pushed
            // past the right edge. Five liveness counts plus three buttons is
            // about 570 px; at a 500 px window `close` was sliced through its
            // last glyph, and narrower it was gone. The counts are the part
            // that can afford to lose a word — the way out of the screen is
            // not.
            container(counts).width(Length::Fill).clip(true),
            button(
                text(if self.view.following() {
                    "fitting"
                } else {
                    "fit"
                })
                .size(MONO)
            )
            .on_press(Message::Fit),
            button(text("refresh").size(MONO)).on_press_maybe(
                // A second refresh over a revocation in flight would race the
                // snapshot that revocation is about to produce.
                (!self.busy).then_some(Message::Refresh)
            ),
            // Escape has always closed this screen, and nothing on it said so.
            // The explorer covers the whole window — sidebar included — so
            // somebody who opened it and did not already know the keystroke
            // had no visible way back to their conversation.
            button(text("close").size(MONO)).on_press(Message::Close),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center);

        if let Some(notice) = &self.notice {
            controls = controls.push(
                text(notice.clone())
                    .size(MONO)
                    .color(self.palette.color(Token::Error)),
            );
        }
        controls.into()
    }

    /// The right-hand column: the capability filter above the inspector.
    fn side(&self) -> Element<'_, Message> {
        column![
            self.filter(),
            inspector::inspector(
                self.view
                    .selected()
                    .and_then(|key| self.view.graph().inspect(key)),
                self.view
                    .selected()
                    .is_some_and(|key| self.view.is_pinned(key)),
                &self.palette,
            ),
        ]
        .spacing(10)
        .width(Length::Fixed(300.0))
        .into()
    }

    /// Every capability anybody advertises, as a filter.
    ///
    /// From [`MeshGraph::capabilities`], which is ordered, so the list does not
    /// reshuffle between frames.
    fn filter(&self) -> Element<'_, Message> {
        let active = self.view.filter();
        let mut list = column![
            row![
                text("capability")
                    .size(MONO)
                    .color(self.palette.color(Token::Faint)),
                space().width(Length::Fill),
                button(text("all").size(MONO))
                    .on_press_maybe(active.map(|_| Message::Filter(None))),
            ]
            .spacing(6)
        ]
        .spacing(2);
        for (kind, name, advertisers) in self.view.graph().capabilities() {
            let chosen = active.is_some_and(|filter| filter.kind == kind && filter.name == name);
            list = list.push(
                button(
                    text(format!("{} {} · {}", kind.label(), name, advertisers.len()))
                        .size(MONO)
                        .font(iced::Font::MONOSPACE)
                        // Peer-chosen, exactly as in the inspector: capped in
                        // characters, not in width, and not guaranteed to
                        // contain a space for `Word` wrapping to break on.
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .color(self.palette.color(if chosen {
                            Token::Accent
                        } else {
                            Token::Muted
                        })),
                )
                .on_press(Message::Filter(Some(CapabilityFilter {
                    kind,
                    name: name.to_string(),
                }))),
            );
        }
        // 150 is a ceiling, not a height. As a fixed height this box reserved
        // the full 150 px whether it held twenty capabilities or none, and
        // none is the ordinary case — a machine with no peers has nothing to
        // filter on, so the first thing the screen showed was an empty panel
        // the size of a paragraph. Shrinking to the content and capping the
        // growth gives the canvas that space back until there is something to
        // put in it.
        container(scrollable(list).height(Length::Shrink))
            .max_height(150.0)
            .width(Length::Fill)
            .padding(Padding::new(10.0))
            .style({
                let surface = self.palette.surface;
                move |_theme| container::Style {
                    background: Some(iced::Background::Color(surface)),
                    ..container::Style::default()
                }
            })
            .into()
    }
}

/// The graph as the store now reads it.
async fn snapshot(mesh: &Arc<Mutex<Mesh>>, now: DateTime<Utc>) -> MeshGraph {
    let mesh = mesh.lock().await;
    MeshGraph::build(&mesh.local_node(), mesh.store(), now)
}

/// Block a peer, then read the store back.
///
/// The two halves are one call on purpose, and the order is the whole point.
/// [`Mesh::set_trust`] is what severs the peer's live subscriptions in both
/// directions and writes the decision to disk; the snapshot afterwards is what
/// the canvas redraws from. Taking the snapshot first, or taking it from a
/// cached graph, would draw a peer whose stream has already ended as though it
/// were still up — which is the exact failure this screen exists not to have.
///
/// Public because it is the seam the acceptance test drives:
/// `tests/graph_explorer.rs` calls this against a real [`Mesh`] with a real
/// live subscription open, and asserts the stream ends and the returned graph
/// draws the peer unreachable.
pub async fn revoke_and_rebuild(
    mesh: Arc<Mutex<Mesh>>,
    id: NodeId,
    now: DateTime<Utc>,
) -> Result<MeshGraph, String> {
    {
        let mut guard = mesh.lock().await;
        guard
            .set_trust(&id, Trust::Blocked)
            .await
            .map_err(|why| format!("could not revoke {}: {why}", id.short()))?;
    }
    Ok(snapshot(&mesh, now).await)
}

/// A layout seed from a node key.
///
/// FNV-1a over the identity's bytes, for the same reason
/// [`crate::plugins::graph::layout`] hand-rolls its own: `DefaultHasher` is explicitly
/// not stable between Rust releases, and an arrangement that reshuffles on a
/// toolchain upgrade is one nobody can navigate by memory.
fn seed_of(key: &NodeKey) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let bytes = match key {
        NodeKey::Node(id) => id.to_bytes().to_vec(),
        NodeKey::Session(owner, name) => {
            let mut bytes = owner.to_bytes().to_vec();
            bytes.extend_from_slice(name.as_bytes());
            bytes
        }
    };
    for byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
