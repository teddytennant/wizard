//! CLI subcommands whose bodies ship in plugins.
//!
//! `wizard gui`, `wizard acp`, `wizard fleet` and the two gateway surfaces are
//! parsed by core (they are `clap` variants in [`crate::cli`]) and run by
//! plugins (the iced window behind `--features native`, the ACP server behind
//! `acp`, the worktree fleet behind `fleet`, the Telegram bot and its service
//! installer behind `gateway`). Something has to join those two halves without
//! core naming the plugin, and this is it: the plugin `provide`s an
//! [`Entrypoint`] under a well-known name, and the dispatch chain in
//! [`crate::run`] `inject`s one instead of calling `native::run` /
//! `acp::run` / `fleet::run` / `gateway::run`.
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
//! a surface, and this module now has five names over four plugins. A name in
//! a registry costs core one lookup, forever; the third registration cost this
//! file one generic parameter and one constructor rather than a third arm's
//! worth of `#[cfg]`, and the fifth cost nothing at all, which is the point.
//!
//! # What core still holds
//!
//! The names (`"gui"`, `"acp"`, `"fleet"`, `"gateway"`, `"gateway-service"`)
//! and the sentence printed when nothing answers to one. That is the same split [`crate::llm::registry`]
//! makes for a provider `kind`: core may hold the *string* a user types, and
//! the prose explaining how to get the thing behind it, as long as it never
//! names the type or constructs one. Each "not in this build" message in
//! [`crate::run`] is the [`None`] arm of a lookup rather than a
//! `#[cfg(not(feature = "..."))]` block.
//!
//! What core stopped holding is the *present-tense* description. `wizard
//! --help` used to print one hand-written line per surface whether or not the
//! build had it, so a `--no-default-features` binary advertised an ACP server
//! it could not start. The line is now [`Entrypoint::about`] /
//! [`Subcommand::about`], read off whatever registered, and
//! [`crate::cli::command`] decides what a row with nothing behind it looks
//! like.
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

/// The name the mesh registers its `wizard peers` tree under.
pub const PEERS: &str = "peers";
/// The name the ACP server registers under. See [`GUI`] for why this is a
/// constant.
pub const ACP: &str = "acp";

/// The name the worktree fleet registers under. See [`GUI`] for why this is a
/// constant.
pub const FLEET: &str = "fleet";

/// The name the messaging gateway registers `wizard --gateway` under: the
/// long-running bot process itself.
pub const GATEWAY: &str = "gateway";

/// The name the messaging gateway registers `wizard gateway <verb>` under:
/// setting a bot up and running it as a background service.
///
/// A *second* name rather than a second argument type under [`GATEWAY`], and
/// this is the constant that documents why. [`Entrypoint`]'s type parameter
/// separates a lookup at the wrong type from a lookup at the right one, but it
/// is not a second dimension of the registry key:
/// [`ServiceRegistry`](crate::kernel::ServiceRegistry) is a
/// `HashMap<String, _>` whose `provide` *replaces* a name that is already
/// taken — deliberately, so a reload can put a service back with no window in
/// which injectors see [`None`]. Registering both gateway surfaces under
/// `"gateway"` would therefore have left whichever one applied second, and the
/// other would have read exactly like a plugin that was never compiled in.
/// The gateway is the first plugin to own two surfaces and so the first to
/// find this out; `docs/plugins.md` has the write-up.
pub const GATEWAY_SERVICE: &str = "gateway-service";

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
/// What the parameter is *not* is a second dimension of the registry key. The
/// registry is a `HashMap<String, _>` and `provide` replaces a name already
/// taken, so two surfaces under one name leave whichever applied last. What
/// the downcast buys is that the loser then reads as [`None`] — an absent
/// plugin — rather than as the wrong body being handed the wrong argument.
/// See [`GATEWAY_SERVICE`], which is the second name the first two-surface
/// plugin needed.
///
/// The default is [`Config`] because that is what a surface with nothing to
/// parse takes, and it keeps the common `Entrypoint` spelling unqualified.
///
/// [`ServiceRegistry::inject_as`]: crate::kernel::ServiceRegistry::inject_as
pub struct Entrypoint<A = Config> {
    name: &'static str,
    about: &'static str,
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
    pub fn new<F, Fut>(name: &'static str, about: &'static str, body: F) -> Self
    where
        F: Fn(A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        Self::with_status(name, about, move |arg| {
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
    pub fn with_status<F, Fut>(name: &'static str, about: &'static str, body: F) -> Self
    where
        F: Fn(A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<i32>> + Send + 'static,
    {
        Self {
            name,
            about,
            body: Box::new(move |arg| Box::pin(body(arg))),
        }
    }

    /// What this entrypoint answers to. Carried on the value as well as being
    /// the registry key so a diagnostic that has the entrypoint but not the
    /// lookup can still say which one it is.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The line `wizard --help` gives this subcommand. See
    /// [`Subcommand::about`] for why the surface holds it rather than core,
    /// and why it carries no trailing full stop.
    pub fn about(&self) -> &'static str {
        self.about
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
            .field("about", &self.about)
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
    about: &'static str,
    body: ArgsBody,
}

impl Subcommand {
    /// Wrap an `async fn(Vec<String>) -> Result<i32>`.
    pub fn new<F, Fut>(name: &'static str, about: &'static str, body: F) -> Self
    where
        F: Fn(Vec<String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<i32>> + Send + 'static,
    {
        Self {
            name,
            about,
            body: Box::new(move |args| Box::pin(body(args))),
        }
    }

    /// What this subcommand answers to.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The line `wizard --help` gives this subcommand.
    ///
    /// Core held this as a doc comment on the `clap` variant until now, which
    /// meant a build that had left the plugin out still described in the
    /// present tense a surface it does not have. It belongs to whoever
    /// implements the surface, for the same reason
    /// [`ProviderDescriptor::display_name`] belongs to the backend: it is a
    /// claim about what the thing does, and core does not have the thing.
    /// What core keeps is the sentence for when *nothing* answers — see
    /// [`crate::cli::command`].
    ///
    /// `&'static str` rather than `String` so [`crate::cli::command`] can copy
    /// it out of the `Arc` the registry hands back and give it to a
    /// `clap::Command` that outlives the lookup. A surface's description is
    /// written in its source; there is nothing to compute.
    ///
    /// No trailing full stop, because this is a cell in `--help`'s subcommand
    /// table rather than a sentence in a paragraph — which is also why `clap`
    /// strips one off a doc comment on its way into the same slot.
    ///
    /// [`ProviderDescriptor::display_name`]: crate::llm::registry::ProviderDescriptor::display_name
    pub fn about(&self) -> &'static str {
        self.about
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
            .field("about", &self.about)
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

/// The lookup itself. Which surface is present on which build is asserted a
/// row at a time in
/// `plugins::an_entrypoint_is_registered_exactly_when_its_plugin_is_compiled_in`,
/// beside the provider and tool tables it belongs with.
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
    /// and what the surface is for. All three are load-bearing: the `clap`
    /// variant stays in core whether or not the body does, so the subcommand
    /// still parses and somebody with it in a script or in muscle memory
    /// still types it — `--help` having stopped listing it is what makes the
    /// message the *only* place they find out.
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
        let entry = Entrypoint::with_status("test", "a test", |code: i32| async move { Ok(code) });
        assert_eq!(entry.run(3).await.expect("ran"), 3);

        let unit = Entrypoint::new("test", "a test", |()| async move { Ok(()) });
        assert_eq!(unit.run(()).await.expect("ran"), 0);
    }

    /// Every surface this build compiled in says what it is, and says it in
    /// the shape `--help`'s subcommand table wants.
    ///
    /// The table renders one line per subcommand, so an empty description is
    /// a blank row and a trailing full stop is a stop `clap` would have
    /// stripped off a doc comment. Both are the kind of thing that is noticed
    /// by a reader and not by a compiler, which is why they are asserted here
    /// rather than left to whoever writes the next plugin.
    #[test]
    fn a_registered_surface_describes_itself_for_the_subcommand_table() {
        let mut abouts: Vec<&str> = Vec::new();
        if let Some(entry) = installed::<Config>(GUI) {
            abouts.push(entry.about());
        }
        if let Some(entry) = installed::<Config>(ACP) {
            abouts.push(entry.about());
        }
        if let Some(entry) = installed::<crate::cli::FleetCmd>(FLEET) {
            abouts.push(entry.about());
        }
        if let Some(entry) = installed_subcommand(PEERS) {
            abouts.push(entry.about());
        }
        assert_eq!(
            abouts.len(),
            [
                cfg!(feature = "native"),
                cfg!(feature = "acp"),
                cfg!(feature = "fleet"),
                cfg!(feature = "mesh"),
            ]
            .iter()
            .filter(|on| **on)
            .count(),
            "every compiled-in surface should have been found"
        );
        for about in abouts {
            assert!(!about.trim().is_empty(), "a surface described itself blank");
            assert!(!about.ends_with('.'), "{about}");
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

    /// Two surfaces owned by one plugin are two names, and neither answers at
    /// the other's argument type. The gateway is the case that proved a single
    /// name could not carry both — see [`GATEWAY_SERVICE`] — so both halves of
    /// that are asserted here rather than only in the plugin's own tests,
    /// where a build without the feature would not compile them at all.
    #[test]
    #[cfg(feature = "gateway")]
    fn the_gateways_two_surfaces_are_two_names_at_two_argument_types() {
        assert!(installed::<Config>(GATEWAY).is_some());
        assert!(installed::<crate::cli::GatewayCmd>(GATEWAY_SERVICE).is_some());
        assert!(installed::<crate::cli::GatewayCmd>(GATEWAY).is_none());
        assert!(installed::<Config>(GATEWAY_SERVICE).is_none());
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
