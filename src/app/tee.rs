//! The seam a live session's events leave this process through, when some
//! plugin has somewhere to send them.
//!
//! The tee itself — a QUIC listener, an identity, a consent ledger, a peer
//! watching a transcript — is the mesh plugin's
//! (`crate::plugins::mesh::tee`, `--features mesh`). What stays here is the
//! two things core needs to hold: the *shape* of "something that takes this
//! session's events", and the lookup that finds one without naming the plugin
//! that registered it.
//!
//! # Why this is a service and not the plugin's type
//!
//! [`App`](super::App) has held `Option<MeshTee>` since the tee landed, and
//! `App::handle_agent_event` called `MeshTee::publish` by name. That is
//! `docs/plugins.md`'s first rule broken twice over — a core struct with a
//! plugin's type in a field, and a core method calling a plugin's inherent
//! method — and it compiled either way, which is how it survived. The field is
//! now a `Box<dyn SessionTee>` and the constructor is a
//! [`TeeFactory`](TeeFactory) injected by name, so a build without the mesh has
//! a `None` there and one fewer socket, and core changed nothing to get it.
//!
//! # Why a trait here and a struct in [`crate::entrypoint`]
//!
//! [`Entrypoint`](crate::entrypoint::Entrypoint) is one closure, so a struct
//! holding it is the same expressiveness with none of the `Arc<dyn Any>`
//! downcast trouble. A tee is not: it is a live object with four questions to
//! answer over a session's lifetime, one of which consumes it. The
//! `Arc::downcast` problem does not arise because what is *injected* is the
//! factory — a struct, like `Entrypoint` — and the trait object is what the
//! factory returns.
//!
//! # What core still holds
//!
//! The name `"session-tee"`, and nothing else. Every word a user reads about
//! the mesh listening, or failing to, comes back from the plugin: see
//! [`SessionTee::joined_notice`] and the error [`join`] passes through
//! untouched. Core printing "mesh: listening on ..." would be core knowing
//! that the thing on the other end of this lookup is a mesh, and the next
//! plugin to register one would make that sentence wrong.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

use crate::agent::AgentEvent;
use crate::config::Config;

/// The name a plugin registers its tee factory under, and the one [`join`]
/// looks up.
///
/// A `const` for the reason [`crate::entrypoint::GUI`] is one: the two ends are
/// core's session startup and a feature-gated plugin, and a typo in either
/// compiles into a build that silently never tees.
pub const SESSION_TEE: &str = "session-tee";

/// One session's events on their way off this machine.
///
/// Four methods, because four is what a session's lifetime asks: hand over an
/// event, say what to tell the user at startup, and shut down cleanly. The
/// shutdown consumes, so it is `self: Box<Self>` — closing a QUIC endpoint
/// politely means telling the far end, which is an async conversation a
/// `Drop` cannot have.
///
/// [`Debug`](std::fmt::Debug) is a supertrait because [`App`](super::App)
/// derives `Debug` and a field that broke it would push a hand-written impl
/// onto a 4000-line struct. Implementors are expected to keep the peer list
/// out of it, for the reason the mesh's own impl gives.
pub trait SessionTee: Send + Sync + std::fmt::Debug {
    /// Hand one of this session's events to whoever is watching, and answer
    /// how many took it.
    ///
    /// `0` when nobody is watching *and* `0` when the event does not cross at
    /// all: the caller has nothing different to do about the two, and a
    /// surface that had to tell them apart at the call site would eventually
    /// stop trying.
    fn publish(&self, event: &AgentEvent) -> usize;

    /// The line the surface prints once the tee is up.
    ///
    /// The plugin's words, not core's. A socket this process opened because a
    /// config file asked it to is exactly the thing a user should not have to
    /// go and check for, so this is said out loud every time — and only the
    /// thing that opened it can say what it opened.
    fn joined_notice(&self) -> String;

    /// Say the session ended and close whatever was open.
    ///
    /// Consuming rather than [`Drop`] for the reason above. `Box<Self>` rather
    /// than `self` so the trait stays object-safe.
    fn leave(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// The future a [`TeeFactory`] hands back: a tee, a reasoned absence, or a
/// failure already worded by the plugin that tried.
type Opening = Pin<Box<dyn Future<Output = Result<Option<Box<dyn SessionTee>>>> + Send>>;

/// The boxed constructor. `Fn` rather than `FnOnce` because a
/// [`Service`](crate::kernel::Service) is shared: the registry hands out
/// `Arc`s, and only one caller per process will ever run it.
type Body = Box<dyn Fn(Config, String) -> Opening + Send + Sync>;

/// How a plugin says "I can tee a session", injected by name.
///
/// Takes an owned [`Config`] and session id rather than borrows because the
/// future it returns outlives the call — a boxed future borrowing its
/// arguments needs a lifetime the service registry cannot express. One clone
/// of `Config` per session start is not a cost worth a higher-ranked trait
/// bound.
pub struct TeeFactory {
    body: Body,
}

impl TeeFactory {
    /// Wrap an `async fn(Config, String) -> Result<Option<Box<dyn SessionTee>>>`.
    ///
    /// The three answers are all real and all different. `Ok(Some)` is a tee.
    /// `Ok(None)` is "configured not to" — the mesh's `[mesh] listen = false`,
    /// which is the default install and must cost nothing and say nothing.
    /// `Err` is "meant to, could not", which the surface says out loud.
    pub fn new<F, Fut>(body: F) -> Self
    where
        F: Fn(Config, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<Box<dyn SessionTee>>>> + Send + 'static,
    {
        Self {
            body: Box::new(move |config, session| Box::pin(body(config, session))),
        }
    }

    /// Build the tee for one session.
    pub fn open(&self, config: Config, session: String) -> Opening {
        (self.body)(config, session)
    }
}

impl std::fmt::Debug for TeeFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeeFactory").finish()
    }
}

/// Open this session's tee, or answer `None` because nothing is listening —
/// either because no plugin registered a factory, or because the one that did
/// is configured not to.
///
/// The two `None`s are deliberately the same answer. A caller that could tell
/// "no mesh compiled in" from "mesh compiled in, `listen = false`" would have
/// two silent paths to keep in step, and the honest surface behaviour is
/// identical: no socket, no notice, a session that runs normally.
pub async fn join(config: &Config, session: &str) -> Result<Option<Box<dyn SessionTee>>> {
    let Some(factory) = crate::plugins::kernel()
        .services()
        .inject_as::<TeeFactory>(SESSION_TEE)
    else {
        return Ok(None);
    };
    factory.open(config.clone(), session.to_string()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build with no tee plugin joins nothing, quietly, and is not an error.
    /// This is the `--no-default-features` path and also the path every test
    /// binary takes, since none of them turns `[mesh] listen` on.
    #[tokio::test]
    async fn a_build_with_no_tee_plugin_joins_nothing() {
        let config = Config::default();
        let joined = join(&config, "session-1")
            .await
            .expect("absent is not an error");
        assert!(joined.is_none());
    }

    /// The factory is registered exactly when the plugin that registers it is
    /// compiled in. Both directions: a `mesh` build whose factory did not
    /// register is a session that silently never tees while the whole
    /// transport sits in the binary.
    #[test]
    fn the_tee_factory_is_present_exactly_when_the_mesh_feature_is() {
        let found = crate::plugins::kernel()
            .services()
            .inject_as::<TeeFactory>(SESSION_TEE);
        assert_eq!(found.is_some(), cfg!(feature = "mesh"));
    }
}
