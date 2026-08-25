//! CLI subcommands whose bodies ship in plugins.
//!
//! `wizard gui`, `wizard acp` and `wizard fleet` are parsed by core (they are
//! `clap` variants in [`crate::cli`]) and run by plugins (the iced window
//! behind `--features native`, the ACP server behind `acp`, the worktree fleet
//! behind `fleet`). Something has to join those two halves without core naming
//! the plugin, and this is it: the plugin `provide`s an [`Entrypoint`] under a
//! well-known name, and the dispatch chain in [`crate::run`] `inject`s one
//! instead of calling `native::run` / `acp::run` / `fleet::run`.
//!
//! # Why this is a service and not a slash command
//!
//! [`Ctx::command`](crate::kernel::Ctx::command) already exists and already
//! registers something a plugin owns, so it is the obvious hook and it is the
//! wrong one. A [`PluginCommand`](crate::commands::PluginCommand) is a
//! `String -> String` body that runs *inside a session*, on a surface that is
//! already up, and `src/commands/plugin.rs` says why it deliberately cannot
//! reach further than that. All three surfaces here are the opposite: they run
//! before there is a session (the window builds its own
//! [`TaskManager`](crate::plugins::gui::tasks::TaskManager) per chat, the ACP
//! server builds one headless agent per `session/new`, a fleet run builds one
//! for planning and another for synthesis), and none of them returns until the
//! surface is finished — a window closing, an editor closing the pipe, every
//! worker reaped. Registering any of them as a slash command would mean a
//! `/gui` in the TUI palette that opens a second surface out from under the
//! first, which is not a thing anybody asked for.
//!
//! # Why not just keep the `#[cfg]`-gated arms
//!
//! Because those arms are the edge. `docs/plugins.md`'s first rule is "no core
//! module may `use crate::<plugin>`", and `src/lib.rs` calling `native::run`
//! was exactly that: the dispatch chain naming a plugin's function, gated on
//! the plugin's own cargo feature. It compiles either way, which is why it
//! survived a year — but it means core pays one `#[cfg]` per plugin that owns
//! a surface, and this module now has three. A name in a registry costs core
//! one lookup, forever, and the third registration cost this file one generic
//! parameter and one constructor rather than a third arm's worth of `#[cfg]`.
//!
//! # What core still holds
//!
//! The names (`"gui"`, `"acp"`, `"fleet"`) and the sentence printed when
//! nothing answers to one. That is the same split [`crate::llm::registry`]
//! makes for a provider `kind`: core may hold the *string* a user types, and
//! the prose explaining how to get the thing behind it, as long as it never
//! names the type or constructs one. Each "not in this build" message in
//! [`crate::run`] is the [`None`] arm of a lookup rather than a
//! `#[cfg(not(feature = "..."))]` block.
//!
//! Core also still holds the *arguments*. `FleetCmd` is a `clap::Subcommand`
//! in [`crate::cli`] and stays there: parsing `wizard fleet run -n 3` is the
//! CLI's job, it has to keep parsing on a build with no fleet plugin (so that
//! `wizard --plan fleet status` is still rejected for the right reason), and
//! `--help` has to keep listing the subcommand. What moved is the body.

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

/// The name the ACP server registers under. See [`GUI`] for why this is a
/// constant.
pub const ACP: &str = "acp";

/// The name the worktree fleet registers under. See [`GUI`] for why this is a
/// constant.
pub const FLEET: &str = "fleet";

/// The boxed body. A `Fn` rather than a `FnOnce` because a [`Service`] is
/// shared: the registry hands out `Arc`s, and an entrypoint that consumed
/// itself could not be handed out twice even though only one caller will ever
/// run it.
///
/// [`Service`]: crate::kernel::Service
type Body<A> = Box<dyn Fn(A) -> Pin<Box<dyn Future<Output = Result<i32>> + Send>> + Send + Sync>;

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
/// # Why the argument is a type parameter
///
/// The first two surfaces through this door take a [`Config`] and the third
/// does not: `wizard fleet` is a subcommand *tree*, so its body needs the
/// parsed [`FleetCmd`](crate::cli::FleetCmd) and loads config itself further
/// down (only `fleet run` drives an agent; `status` and `stop` read
/// `.wizard/fleet/`). The three ways to absorb that were an enum of argument
/// shapes in core — which is core enumerating its plugins again, one variant
/// per surface — an `Arc<dyn Any>` the plugin downcasts, which moves the type
/// error from compile time to a silent `None` at runtime, or a type
/// parameter. The parameter is nearly free: [`ServiceRegistry::inject_as`]
/// keys on `TypeId` already, so `Entrypoint<Config>` and
/// `Entrypoint<FleetCmd>` are simply different services and the downcast that
/// resolves them is the one that was already there.
///
/// The default is [`Config`] because that is what a surface with nothing to
/// parse takes, and it keeps the common `Entrypoint` spelling unqualified.
///
/// [`ServiceRegistry::inject_as`]: crate::kernel::ServiceRegistry::inject_as
pub struct Entrypoint<A = Config> {
    name: &'static str,
    body: Body<A>,
}

impl<A: 'static> Entrypoint<A> {
    /// Wrap an `async fn(A) -> Result<()>`: a surface whose only outcomes are
    /// "it ran" and "it failed", which is both the window and the ACP server.
    ///
    /// Exiting 0 is this constructor's whole opinion, and it is core's to
    /// hold: a process that did what it was asked and has nothing to report
    /// has succeeded. A surface that wants to say more than that uses
    /// [`Entrypoint::with_status`].
    pub fn new<F, Fut>(name: &'static str, body: F) -> Self
    where
        F: Fn(A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        Self::with_status(name, move |arg| {
            let fut = body(arg);
            async move { fut.await.map(|()| 0) }
        })
    }

    /// Wrap an `async fn(A) -> Result<i32>`, for a surface that chooses its
    /// own exit code.
    ///
    /// `wizard fleet stop` with no fleet running exits 1 while printing an
    /// ordinary sentence rather than an error, which is a distinction only the
    /// plugin can make: it is not a failure worth an `Err` and a backtrace,
    /// and it is not a success a script should branch on. Collapsing it into
    /// [`Entrypoint::new`] would have meant the plugin returning `Err` to get
    /// a non-zero exit, which changes what the user sees in order to make a
    /// signature tidier.
    pub fn with_status<F, Fut>(name: &'static str, body: F) -> Self
    where
        F: Fn(A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<i32>> + Send + 'static,
    {
        Self {
            name,
            body: Box::new(move |arg| Box::pin(body(arg))),
        }
    }

    /// What this entrypoint answers to. Carried on the value as well as being
    /// the registry key so a diagnostic that has the entrypoint but not the
    /// lookup can still say which one it is.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Run it, and hand back the process exit code. Returns when the surface
    /// exits, which for a window is when the user closes it, for the ACP
    /// server when the editor closes the pipe, and for a fleet run when every
    /// worker has been reaped and the synthesis turn is done.
    pub fn run(&self, arg: A) -> Pin<Box<dyn Future<Output = Result<i32>> + Send>> {
        (self.body)(arg)
    }
}

impl<A> std::fmt::Debug for Entrypoint<A> {
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
/// error and never a panic. The caller turns it into a sentence — see
/// [`absent`].
///
/// `A` has to match what the plugin registered, because the lookup is a
/// `TypeId` downcast. A mismatch answers [`None`] rather than failing to
/// compile, which is the one sharp edge the type parameter brought with it; the
/// `..._is_present_exactly_when_..._feature_is` tests below are what keep it
/// from shipping, since a build with the feature on and the wrong argument
/// type reads exactly like a build with the feature off.
///
/// [`None`] one moment earlier than that, too: a plugin that asks this from
/// inside its own `apply` is on the thread that is inside the kernel's
/// `OnceLock` initializer, and re-entering that deadlocks. It gets the
/// absent answer rather than a hung process — which is the right answer
/// anyway, since the plugin set is by definition not finished registering
/// yet. `crate::plugins::ensure_providers` makes the same call for the same
/// reason.
pub fn installed<A: Send + Sync + 'static>(name: &str) -> Option<Arc<Entrypoint<A>>> {
    if crate::plugins::loading() {
        return None;
    }
    crate::plugins::kernel()
        .services()
        .inject_as::<Entrypoint<A>>(name)
}

/// The error a dispatch arm returns when nothing answers to `name`.
///
/// Two of the three arms print exactly this, and they are the two whose
/// feature is on by default: `wizard acp` and `wizard fleet` are in every
/// stock build and in every published binary, so the only way to be reading
/// this message is to have built the tree yourself with the feature off, and
/// the one thing worth saying is which flag puts it back. `detail` is the
/// surface's own sentence about what it would have done, because "this build
/// has no `acp`" tells somebody who typed it by mistake nothing at all.
///
/// `wizard gui` does **not** use this and keeps its own longer message. Its
/// feature is off by default and the window ships as a separate release asset,
/// so "rebuild with this flag" is only half of its answer — the other half is
/// `install.sh WIZARD_NATIVE=1`, and a build flag offered as the sole route to
/// a thing that is one `curl` away is how `wizard app` spent a year telling
/// people to compile iced.
pub fn absent(name: &str, feature: &str, detail: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`wizard {name}` is not in this build — it was compiled without the `{feature}` \
         feature.\n\
         \n\
         {detail}\n\
         \n\
         To get it: `cargo build --release --features {feature}` from a checkout, or install \
         a stock release binary, which has it — `{feature}` is on by default and every \
         published `wizard` carries it."
    )
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
        assert!(installed::<Config>("no-such-surface").is_none());
    }

    /// Which surface is present on which build is asserted one row at a time
    /// in `plugins::an_entrypoint_is_registered_exactly_when_its_plugin_is_compiled_in`,
    /// beside the provider and tool tables it belongs with. What is left here
    /// is the lookup itself.
    ///
    /// Asking for the wrong argument type is a `None`, not a panic and not a
    /// wrong-shaped call. Pinned because it is the failure mode the type
    /// parameter introduced: it looks exactly like an absent plugin, so the
    /// behaviour under it had better be the absent plugin's.
    #[test]
    #[cfg(feature = "fleet")]
    fn an_entrypoint_asked_for_under_the_wrong_argument_type_is_absent() {
        assert!(installed::<crate::cli::FleetCmd>(FLEET).is_some());
        assert!(installed::<Config>(FLEET).is_none());
    }

    /// The absent message names the subcommand, the flag that brings it back
    /// and what the surface is for. All three are load-bearing: somebody
    /// reading it has just typed a subcommand that `--help` still lists,
    /// because the `clap` variant stays in core whether or not the body does.
    #[test]
    fn the_absent_message_names_the_subcommand_the_feature_and_the_reason() {
        let message = absent(ACP, "acp", "It serves editors over stdio.").to_string();
        assert!(message.contains("wizard acp"), "{message}");
        assert!(message.contains("--features acp"), "{message}");
        assert!(message.contains("serves editors"), "{message}");
    }

    /// An exit code chosen by the surface survives the boxing, and a unit
    /// body still exits 0. The two constructors differ only here, and
    /// `wizard fleet stop` on a project with no fleet is the case that made
    /// the difference necessary.
    #[tokio::test]
    async fn with_status_carries_the_surfaces_own_exit_code() {
        let entry = Entrypoint::with_status("test", |code: i32| async move { Ok(code) });
        assert_eq!(entry.run(3).await.expect("ran"), 3);

        let unit = Entrypoint::new("test", |()| async move { Ok(()) });
        assert_eq!(unit.run(()).await.expect("ran"), 0);
    }
}
