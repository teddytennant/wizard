//! A CLI subcommand whose body ships in a plugin.
//!
//! `wizard gui` is parsed by core (it is a `clap` variant in [`crate::cli`])
//! and run by a plugin (the iced window, `--features native`). Something has
//! to join those two halves without core naming the plugin, and this is it:
//! the window `provide`s an [`Entrypoint`] under a well-known name, and the
//! dispatch chain in [`crate::run`] `inject`s one instead of calling
//! `native::run`.
//!
//! # Two shapes, because two subcommands are shaped differently
//!
//! [`Entrypoint`] is `wizard gui`: no arguments, a [`Config`], and it does not
//! return until the surface closes. [`Subcommand`] is `wizard peers`: a whole
//! clap subcommand *tree*, no config, and an exit code. Keeping them apart is
//! cheaper than one type that is half-empty either way, and the difference is
//! not cosmetic — see [`Subcommand`] for why the argument list crosses this
//! boundary unparsed.
//!
//! # Why this is a service and not a slash command
//!
//! [`Ctx::command`](crate::kernel::Ctx::command) already exists and already
//! registers something a plugin owns, so it is the obvious hook and it is the
//! wrong one. A [`PluginCommand`](crate::commands::PluginCommand) is a
//! `String -> String` body that runs *inside a session*, on a surface that is
//! already up, and `src/commands/plugin.rs` says why it deliberately cannot
//! reach further than that. `wizard gui` is the opposite in all three
//! respects: it takes no arguments, there is no session yet — the window
//! builds its own [`TaskManager`](crate::plugins::gui::tasks::TaskManager)
//! and its agents lazily, per chat — and it does not return until the window
//! closes. Registering it as a slash command would mean a `/gui` in the TUI
//! palette that opens a second surface out from under the first, which is not
//! a thing anybody asked for and is a worse answer than the one below.
//!
//! # Why not just keep the `#[cfg]`-gated arm
//!
//! Because that arm is the edge. `docs/plugins.md`'s first rule is "no core
//! module may `use crate::<plugin>`", and `src/lib.rs` calling `native::run`
//! is exactly that: the dispatch chain naming a plugin's function, gated on
//! the plugin's own cargo feature. It compiles either way, which is why it
//! survived this long — but it means core has one `#[cfg]` per plugin that
//! owns a surface, and the second such plugin doubles it. A name in a
//! registry costs core one lookup, forever.
//!
//! # What core still holds
//!
//! The name (`"gui"`) and the sentence printed when nothing answers to it.
//! That is the same split [`crate::llm::registry`] makes for a provider
//! `kind`: core may hold the *string* a user types, and the prose explaining
//! how to get the thing behind it, as long as it never names the type or
//! constructs one. The "this build has no native GUI" message in
//! [`crate::run`] is the [`None`] arm of the lookup rather than a
//! `#[cfg(not(feature = "native"))]` block, and it says the same words.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;

/// The name the native window registers under, and the one [`crate::run`]
/// looks up when it sees `wizard gui`.
///
/// A `const` rather than a literal at both ends because the two ends are in
/// different crates' worth of code — core's dispatch and a feature-gated
/// plugin — and a typo in either would compile into a build where
/// `wizard gui` reports that this binary has no window while the window sits
/// in it, registered under a name nobody asks for.
pub const GUI: &str = "gui";

/// The name the mesh registers its `wizard peers` tree under.
pub const PEERS: &str = "peers";

/// The boxed body. A `Fn` rather than a `FnOnce` because a [`Service`] is
/// shared: the registry hands out `Arc`s, and an entrypoint that consumed
/// itself could not be handed out twice even though only one caller will ever
/// run it.
///
/// [`Service`]: crate::kernel::Service
type Body = Box<dyn Fn(Config) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

/// A long-running surface a plugin owns, injected by name.
///
/// A concrete struct rather than a trait, for a mechanical reason worth
/// writing down: [`ServiceRegistry::inject_as`] is an `Arc<dyn Any>`
/// downcast, and `Arc::downcast` needs a `Sized` target, so publishing an
/// `Arc<dyn SomeTrait>` means the injector has to name
/// `Arc<Arc<dyn SomeTrait>>` to get it back. One closure in a struct is the
/// same expressiveness with none of that, and there is exactly one method to
/// have.
///
/// [`ServiceRegistry::inject_as`]: crate::kernel::ServiceRegistry::inject_as
pub struct Entrypoint {
    name: &'static str,
    body: Body,
}

impl Entrypoint {
    /// Wrap an `async fn(Config) -> Result<()>`, which is the shape every
    /// surface in the dispatch chain already has (`acp::run`, `native::run`).
    pub fn new<F, Fut>(name: &'static str, body: F) -> Self
    where
        F: Fn(Config) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        Self {
            name,
            body: Box::new(move |config| Box::pin(body(config))),
        }
    }

    /// What this entrypoint answers to. Carried on the value as well as being
    /// the registry key so a diagnostic that has the entrypoint but not the
    /// lookup can still say which one it is.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Run it. Returns when the surface exits, which for a window is when the
    /// user closes it.
    pub fn run(&self, config: Config) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        (self.body)(config)
    }
}

impl std::fmt::Debug for Entrypoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entrypoint")
            .field("name", &self.name)
            .finish()
    }
}

/// The entrypoint some plugin registered under `name`, or [`None`] on a build
/// that did not compile one in.
///
/// [`None`] is the whole contract: `docs/plugins.md`'s composability rule is
/// that an absent plugin is an `inject` that answers nothing, never a link
/// error and never a panic. The caller turns it into a sentence.
///
/// [`None`] one moment earlier than that, too: a plugin that asks this from
/// inside its own `apply` is on the thread that is inside the kernel's
/// `OnceLock` initializer, and re-entering that deadlocks. It gets the
/// absent answer rather than a hung process — which is the right answer
/// anyway, since the plugin set is by definition not finished registering
/// yet. `crate::plugins::ensure_providers` makes the same call for the same
/// reason.
pub fn installed(name: &str) -> Option<Arc<Entrypoint>> {
    if crate::plugins::loading() {
        return None;
    }
    crate::plugins::kernel()
        .services()
        .inject_as::<Entrypoint>(name)
}

/// The boxed body of a [`Subcommand`], over the raw argument list.
type ArgsBody =
    Box<dyn Fn(Vec<String>) -> Pin<Box<dyn Future<Output = Result<i32>> + Send>> + Send + Sync>;

/// A CLI subcommand *tree* a plugin owns, injected by name.
///
/// [`Entrypoint`]'s sibling, and the one that carries arguments. `wizard peers`
/// has eight subcommands, one of which takes a three-state trust decision as a
/// `clap::ValueEnum`, and that enum is the plugin's: it is the peer store's
/// recorded decision, derived on the store's own type precisely so a second
/// spelling on the argument-parsing side cannot drift into a fourth state.
///
/// # Why the arguments cross unparsed
///
/// Because the alternative is core owning a type it must not own. Core's clap
/// variant is `Peers { args: Vec<String> }` with `trailing_var_arg` and
/// `allow_hyphen_values`, so `wizard peers trust wiz1abc trusted` and
/// `wizard peers --help` both arrive here as a plain vector and are parsed by
/// the plugin's own [`clap::Parser`], which is where `Trust` lives. The two
/// things core keeps are the two things `docs/plugins.md` has always let it
/// keep: the string `"peers"` and the paragraph in `--help` describing what is
/// behind it.
///
/// The cost is real and worth naming: `wizard --help` shows `peers` with core's
/// one-line description rather than its subcommand list, and a misspelled
/// subcommand is caught by the plugin's parser rather than the top-level one.
/// Both produce the same message a user would have got anyway, one frame later.
/// The alternative — mirroring the eight variants and the trust enum into core
/// — is a build that can express a decision the store cannot record, which is
/// the failure the enum's own doc comment was written to prevent.
///
/// The exit code rather than `()` because a `wizard peers ping` that could not
/// reach a peer is a script's answer, and the dispatch chain returns `i32`.
pub struct Subcommand {
    name: &'static str,
    body: ArgsBody,
}

impl Subcommand {
    /// Wrap an `async fn(Vec<String>) -> Result<i32>`.
    pub fn new<F, Fut>(name: &'static str, body: F) -> Self
    where
        F: Fn(Vec<String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<i32>> + Send + 'static,
    {
        Self {
            name,
            body: Box::new(move |args| Box::pin(body(args))),
        }
    }

    /// What this subcommand answers to.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Parse `args` and run it. `args` excludes the subcommand's own name:
    /// `wizard peers list` arrives as `["list"]`.
    pub fn run(&self, args: Vec<String>) -> Pin<Box<dyn Future<Output = Result<i32>> + Send>> {
        (self.body)(args)
    }
}

impl std::fmt::Debug for Subcommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subcommand")
            .field("name", &self.name)
            .finish()
    }
}

/// The subcommand tree some plugin registered under `name`, or [`None`] on a
/// build that did not compile one in.
///
/// Same contract as [`installed`], including the `loading()` guard and the
/// reason for it.
pub fn installed_subcommand(name: &str) -> Option<Arc<Subcommand>> {
    if crate::plugins::loading() {
        return None;
    }
    crate::plugins::kernel()
        .services()
        .inject_as::<Subcommand>(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `None` arm is reachable and is not an error: a name nothing
    /// registered answers nothing at all. This is the path a default build
    /// takes for `"gui"`, and it has to work in a test binary too, where
    /// `crate::run` is never entered.
    #[test]
    fn an_unregistered_entrypoint_is_absent_rather_than_a_failure() {
        assert!(installed("no-such-surface").is_none());
    }

    /// The window is present exactly when its feature is. Both directions,
    /// because the interesting failure is the silent one: a `native` build
    /// whose window did not register is a `wizard gui` that prints the
    /// install instructions while the window is linked into the binary.
    #[test]
    fn the_gui_entrypoint_is_present_exactly_when_the_native_feature_is() {
        let found = installed(GUI);
        assert_eq!(found.is_some(), cfg!(feature = "native"));
        if let Some(entry) = found {
            assert_eq!(entry.name(), GUI);
        }
    }

    /// `wizard peers` is present exactly when the mesh is. The silent failure
    /// this catches is the same one as the window's, one subcommand along: a
    /// `mesh` build whose tree did not register prints "this build has no
    /// mesh" while the whole transport sits in the binary.
    #[test]
    fn the_peers_subcommand_is_present_exactly_when_the_mesh_feature_is() {
        let found = installed_subcommand(PEERS);
        assert_eq!(found.is_some(), cfg!(feature = "mesh"));
        if let Some(entry) = found {
            assert_eq!(entry.name(), PEERS);
        }
    }

    /// The two lookups are separate registries' worth of names and do not see
    /// each other: asking for a subcommand by an entrypoint's name answers
    /// nothing, which is what keeps `wizard gui` from being reachable as an
    /// argument-taking tree.
    #[test]
    fn an_unregistered_subcommand_is_absent_rather_than_a_failure() {
        assert!(installed_subcommand("no-such-subcommand").is_none());
    }
}
