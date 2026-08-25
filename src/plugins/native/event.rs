//! Getting a turn's events from tokio into iced, and iced onto tokio.
//!
//! Two adaptations live here, and neither is a translation: the point of an
//! in-process GUI is that nothing is serialized on the way to the screen.
//!
//! # The stream
//!
//! [`AgentEvent`] is `Clone + Debug`, which is exactly iced's bound on a
//! `Message`, so it *is* the message — no wrapper type, no `From` impl, no
//! second enum to keep in step with the first. The channel is the one
//! [`TaskShared::tap`](crate::plugins::gui::tasks::TaskShared::tap) hands out, and the
//! adapter is a hand-rolled [`stream::poll_fn`] over `poll_recv` rather than
//! `tokio_stream::wrappers::UnboundedReceiverStream`: the wrapper crate exists
//! to do these four lines, and a dependency that is four lines long is four
//! lines of dependency-tree, MSRV and audit surface for nothing.
//!
//! ## Subscriptions are identified by a hash, not by a handle
//!
//! `Subscription::run_with(data, builder)` takes a **function pointer**, so the
//! builder cannot capture anything: everything the stream needs has to travel in
//! `data`, and `data` has to be `Hash` because its hash is what tells iced
//! whether this is the same subscription it was already running or a new one.
//! [`Feed`] is that carrier. It hashes as `(task id, generation)` — deliberately
//! *not* including the `Arc`'s address, which would make an otherwise identical
//! subscription look new every time the pointer moved — so switching to another
//! task tears the old tap down and stands a new one up, and re-rendering the
//! same task does neither.
//!
//! The tap is released when the stream is dropped, by a guard that travels
//! inside it. Nothing else can do it: iced drops a retired subscription's stream
//! and tells nobody, so a release that lived in `update` would leak a tap every
//! time a subscription ended for a reason the app did not initiate.
//!
//! # The executor
//!
//! iced's stock tokio executor is `tokio::runtime::Runtime` and it calls
//! `Runtime::new()` — a *second* runtime — then `block_on`s the compositor's
//! creation on it. Inside `wizard`, that lands on a thread that is already
//! inside the process's runtime (`main` blocks on the top-level future there),
//! and tokio panics on a `block_on` from within a runtime context. So
//! [`Ambient`] hands iced the runtime that already exists: spawns go to its
//! worker threads, `enter` installs its context so anything constructed under it
//! can reach a reactor, and `block_on` is the plain futures one, which is
//! correct here because the only future iced ever passes to it is the
//! software-renderer compositor's constructor, which awaits nothing.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use iced::Subscription;
use iced::futures::Stream;
use tokio::sync::mpsc;

use crate::agent::AgentEvent;
use crate::plugins::gui::tasks::TaskShared;

/// Which task's events a subscription is carrying.
///
/// `generation` is the app's, not the tap's: bumping it is how the window asks
/// for the stream to be rebuilt (after a task is swapped, or a worker restarts)
/// without changing which task it is watching.
pub struct Feed {
    pub task: Arc<TaskShared>,
    pub generation: u64,
}

impl Hash for Feed {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.task.id.hash(hasher);
        self.generation.hash(hasher);
    }
}

/// Release a tap when the stream carrying it goes away.
struct Tap {
    task: Arc<TaskShared>,
    generation: u64,
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.task.untap(self.generation);
    }
}

/// The subscription that carries one task's turn into the window.
pub fn events(feed: Feed) -> Subscription<AgentEvent> {
    Subscription::run_with(feed, |feed| stream_for(Arc::clone(&feed.task)))
}

/// One task's events as a stream, with the tap released on drop.
fn stream_for(task: Arc<TaskShared>) -> impl Stream<Item = AgentEvent> {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let generation = task.tap(sender);
    let tap = Tap { task, generation };
    futures_util::stream::poll_fn(move |context| {
        // The guard lives in the closure's captures, so the tap is released
        // exactly when iced drops the stream — which is the only moment anything
        // learns that this subscription has ended.
        let _ = &tap;
        receiver.poll_recv(context)
    })
}

/// An [`iced::Executor`] over the runtime that already exists.
///
/// See the module header for why the stock one cannot be used from inside
/// `wizard::run`.
pub struct Ambient(tokio::runtime::Handle);

impl iced::Executor for Ambient {
    fn new() -> Result<Self, iced::futures::io::Error> {
        tokio::runtime::Handle::try_current()
            .map(Self)
            .map_err(|_| {
                iced::futures::io::Error::other(
                    "the native GUI must be started from inside a tokio runtime",
                )
            })
    }

    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let _handle = self.0.spawn(future);
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        // Deliberately not `Handle::block_on`: this is called from the winit
        // event loop, which is running on a thread that is already inside this
        // very runtime, and tokio refuses to block a runtime thread on itself.
        // The one future iced passes here creates the tiny-skia compositor and
        // performs no I/O, so a plain executor drives it to completion.
        iced::futures::executor::block_on(future)
    }

    fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.0.enter();
        f()
    }
}
