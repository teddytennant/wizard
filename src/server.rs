//! The seam `/server` goes through, when some plugin owns a model server's
//! process.
//!
//! The lifecycle itself — finding `llama-server`, downloading a GGUF, spawning
//! the process detached in its own group, recording its PID so a later stop
//! kills that process and never an unrelated one — is llama.cpp's plugin
//! (`crate::plugins::llamacpp::server`, `--features provider-llamacpp`). What
//! stays here is the two things core needs to hold: the *shape* of "something
//! that starts and stops a local model server", and the lookup that finds one
//! without naming the plugin that registered it.
//!
//! # Why the lifecycle went into the provider's plugin rather than beside it
//!
//! This was the design question of the split, and the answer is that
//! `src/server.rs` was never a general local-server manager. Every line of it
//! was llama.cpp's: `probe` reads `llama-server`'s native `GET /health`, whose
//! 503 means "still loading the GGUF"; `spawn` passes `--ctx-size`,
//! `--n-gpu-layers` and a `.gguf` path; the installer downloads from
//! `ggml-org/llama.cpp`'s release assets; `stop` refuses to signal a PID whose
//! process name is not `llama-server`; and `/server` has always answered "the
//! active provider is X — /server only manages a local llama.cpp server" to
//! everything else.
//!
//! So the edge `docs/plugins.md` predicted here — a provider plugin reaching
//! into core for `ensure_running`, which becomes plugin-to-plugin the moment
//! core stops holding it — is not cut. It is **deleted**, because the two
//! things it ran between were one thing. A separate `local-server` feature was
//! the alternative and is worse in a specific way rather than merely less tidy:
//! a `provider-llamacpp` built without it would still register
//! `kind = "llamacpp"` and would simply stop starting the server, so the user's
//! symptom would be a connection refused rather than the named "that kind is
//! not in this build" every other absent plugin produces. Degrading in
//! *behaviour* instead of in *presence* is the one degrade path this
//! architecture does not have.
//!
//! # What is left, and who asks
//!
//! Three questions and a name. The `/server` command is a built-in in
//! `COMMANDS` and stays one — its row is core's, its verbs are core's, and it
//! keeps parsing on every build — and its *body* on all three surfaces that
//! have one (the TUI, the window, the gateway) is now: ask the registry whether
//! the active provider owns a server, then ask this lookup for the thing that
//! runs it, then print what that thing said.
//!
//! Every word about llama.cpp comes back from the plugin. That is the rule
//! [`crate::app::tee`] follows, for the same reason: core printing
//! "llama-server at …: ready" would be core knowing what is on the other end of
//! this lookup, and the second backend to register one would make the sentence
//! wrong. The sentences core does own name no server at all.

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::progress::Progress;

/// The name a plugin registers its process manager under, and the one
/// [`installed`] looks up.
///
/// A `const` for the reason [`crate::entrypoint::GUI`] is one: the two ends are
/// core's `/server` dispatch and a feature-gated plugin, and a typo in either
/// compiles into a build where `/server status` reports that this binary
/// manages no server while the whole spawner sits in it.
pub const LOCAL_SERVER: &str = "local-server";

/// Something that owns a local model server's process.
///
/// Three questions, because three is what `/server` asks — and the three that
/// a user asks return a *sentence* rather than a status enum. That is
/// deliberate and it is most of why this is a trait rather than four free
/// functions behind a `#[cfg]`. `Health::Loading`, the state the old enum had
/// that a boolean does not, means "the GGUF is still being read off disk":
/// a fact about llama.cpp's startup, not about local servers in general. A core
/// enum carrying it would be core describing one backend's internals, and the
/// next backend to register here would have to either misreport itself or make
/// core grow a variant for it. The plugin knows what it is running, so the
/// plugin writes the line.
///
/// The surface still decides *where* the line goes — the TUI's transcript, a
/// chat message, the window's log — and, for [`LocalServer::start`], whether to
/// await it or spawn it. That split is why `start` takes an owned [`Progress`]
/// sink: the TUI runs it on a detached task so the composer keeps accepting
/// keystrokes through a multi-gigabyte download.
#[async_trait]
pub trait LocalServer: Send + Sync {
    /// One line about whether the server for `provider` is up, and about
    /// whether this process is the one that started it.
    async fn status(&self, provider: &ProviderConfig) -> String;

    /// Whether nothing is answering at `provider`'s address right now.
    ///
    /// The one place a surface needs a fact rather than a sentence, and it is
    /// not `/server`: switching the active provider to a local backend
    /// auto-starts its server, and the surface has to know whether it is about
    /// to do that before it says so. A tri-state would push llama.cpp's
    /// "loading" back into core for one caller that treats loading and ready
    /// identically — both mean "do not start a second one".
    async fn is_down(&self, provider: &ProviderConfig) -> bool;

    /// Bring the server for `provider` up, reporting slow work through
    /// `progress`, and answer with what to tell the user.
    ///
    /// `Ok` is a notice and `Err` is an error, a distinction two of the three
    /// surfaces render differently. An already-running server is `Ok`: the user
    /// asked for a running server and there is one.
    ///
    /// Owned arguments rather than borrows because the returned future outlives
    /// the call on every surface that backgrounds it, and a boxed future
    /// borrowing its arguments needs a lifetime the service registry cannot
    /// express. Same trade [`crate::app::tee::TeeFactory`] makes.
    async fn start(
        &self,
        provider: ProviderConfig,
        progress: Box<dyn Progress>,
    ) -> Result<String, String>;

    /// Take the server down, and answer with what to tell the user.
    ///
    /// Infallible from the caller's side on purpose: "there was nothing to
    /// stop", "it had already exited" and "that PID is somebody else's process
    /// now" are all things the user needs to read and none of them is a failure
    /// of this call.
    fn stop(&self) -> String;
}

/// The registered [`LocalServer`], boxed into something the service registry
/// can hand back.
///
/// A newtype for the mechanical reason [`crate::entrypoint::Entrypoint`]'s doc
/// comment gives: `inject_as` is an `Arc<dyn Any>` downcast and
/// `Arc::downcast` needs a `Sized` target, so publishing an
/// `Arc<dyn LocalServer>` would mean the injector naming
/// `Arc<Arc<dyn LocalServer>>` to get it back. The mesh solved the same problem
/// by injecting a *factory*; there is nothing to build here — a process manager
/// reads a PID file and probes a URL, it holds no state a constructor would set
/// up — so the wrapper is the whole of it.
pub struct LocalServerHandle(Box<dyn LocalServer>);

impl LocalServerHandle {
    /// Wrap a plugin's implementation for registration.
    pub fn new(server: impl LocalServer + 'static) -> Self {
        Self(Box::new(server))
    }
}

impl std::ops::Deref for LocalServerHandle {
    type Target = dyn LocalServer;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl std::fmt::Debug for LocalServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalServerHandle").finish()
    }
}

/// The process manager some plugin registered, or [`None`] on a build that
/// compiled none in.
///
/// [`None`] is the whole contract, exactly as it is for an entrypoint or a tee.
/// In practice a surface reaches this only after `manages_local_server` has
/// already said the active backend owns a server, and that descriptor comes
/// from the same plugin — so the two answers agree by construction and the
/// [`None`] arm is defensive. It is still written out rather than `expect`ed,
/// because "defensive" and "unreachable" are different claims and only one of
/// them is true here: a second backend could register a descriptor with
/// `with_local_server` and no manager, and the honest outcome for that is a
/// sentence.
pub fn installed() -> Option<std::sync::Arc<LocalServerHandle>> {
    crate::plugins::kernel()
        .services()
        .inject_as::<LocalServerHandle>(LOCAL_SERVER)
}

/// What `/server` says when the active provider is not one whose process
/// anything here manages.
///
/// Core's own sentence, and it names no backend — which is what lets it be
/// core's. It is reached two ways worth keeping apart in the reader's head but
/// not in the code: the ordinary case, where the user is on a cloud provider
/// and `/server` simply does not apply, and the stripped-build case, where the
/// local backend is not compiled in and so its `kind` resolves to nothing. Both
/// leave the user in the same place — this command has no server to act on, and
/// the way to give it one is to configure a backend that brings its own — so
/// both get the same line rather than two that differ in a detail the user
/// cannot act on.
pub fn not_managed(name: &str, kind: &str) -> String {
    format!(
        "/server manages a model server running on this machine, and the active provider \
         '{name}' ({kind}) is not one. Configure a local backend to use it."
    )
}

/// What `/server` says when the active provider claims to own a server and
/// nothing registered one.
///
/// Unreachable on every build this tree ships, since the descriptor and the
/// manager come from the same plugin. It exists because the alternative to a
/// sentence is a panic, and `docs/plugins.md`'s rule about an absent plugin is
/// that it is never either.
pub fn absent() -> String {
    "the active provider says it manages a local server, but this build has no code that \
     runs one — rebuild with `--features provider-llamacpp`, which is on by default."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manager is registered exactly when the plugin that registers it is
    /// compiled in. Both directions: a `provider-llamacpp` build whose manager
    /// did not register is a `/server start` that reports no spawner while the
    /// whole spawner sits in the binary, and that failure reads exactly like a
    /// build with the feature off.
    #[test]
    fn the_local_server_is_present_exactly_when_its_plugin_is() {
        assert_eq!(installed().is_some(), cfg!(feature = "provider-llamacpp"));
    }

    /// The sentence core owns names no backend, which is the property that lets
    /// core own it. A second local backend registering here must not make it
    /// wrong.
    #[test]
    fn cores_own_sentence_names_no_backend() {
        let not_mine = not_managed("work", "anthropic");
        assert!(not_mine.contains("work"), "{not_mine}");
        assert!(not_mine.contains("anthropic"), "{not_mine}");
        assert!(
            !not_mine.contains("llama") && !not_mine.contains("ollama"),
            "core named a backend: {not_mine}"
        );
    }
}
