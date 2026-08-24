//! Slash commands that are registered at runtime rather than compiled into
//! [`COMMANDS`](super::COMMANDS).
//!
//! # Why the table alone could not stay
//!
//! [`SlashCommand`](super::SlashCommand) is a closed enum and [`COMMANDS`] is a
//! `const` table, which together have the same hard consequence
//! `src/llm/registry.rs` describes for the old nine-variant `ProviderKind`: a
//! command can only exist if core names it, so no command can ever come from a
//! plugin. `docs/plugins.md` requires the opposite, and `ctx:command(spec)` is
//! in the ten-call `Ctx` API precisely so a plugin can add one.
//!
//! # Why the enum stayed anyway
//!
//! The obvious move — turn `SlashCommand` into a string plus a lookup, the way
//! the provider kind went — costs 260 call sites and buys nothing the built-ins
//! need. Every one of those variants carries *parsed arguments* (`Mode`,
//! `ReasoningEffort`, `UltraConfig`, `ImportSelection`) that the one dispatcher
//! matches on exhaustively; a string id would push all of that back into
//! per-command parsing at the surfaces, which is the drift `src/commands/`
//! exists to prevent.
//!
//! So the enum is now the *built-in spelling* and gains one open variant,
//! [`SlashCommand::Plugin`](super::SlashCommand::Plugin), that carries a name
//! and the raw rest of the line. A plugin command is a value in this registry
//! rather than a variant, and everything a surface asks about a command —
//! completion, the argument hint, help, the per-surface [`Execution`], the
//! handler — it asks of a [`PluginCommand`] instead of a
//! [`CommandSpec`](super::CommandSpec). The two are merged by
//! [`listing`](super::listing), and neither knows about the other.
//!
//! # Why the registry is process-wide
//!
//! This is the same call `Slots::insert_provider` had to make, and for the same
//! reason: the consumer is not the kernel. `SlashCommand::parse` is called from
//! `App::submit`, from the window's `route`, from the gateway's
//! `apply_command`, and from `run_command`'s executor — none of which hold a
//! kernel handle, and several of which have no business holding one. A
//! per-kernel registry would mean threading one into all of them, or a second
//! resolution path for plugin commands, which is exactly the second-tier bolt-on
//! this design is trying not to be.
//!
//! So a registration lands here *and* in the kernel's slot, written and swept
//! together by `Slots::insert_command` / `Slots::remove_commands`. Publishing as
//! a separate step would leave a window in which an unloaded plugin's command
//! still resolved, and exact unload is the reason there is a kernel.
//!
//! # Conflict policy: the built-in keeps the name
//!
//! [`install`] refuses a name a built-in already owns, and refuses a name
//! another plugin already holds. It never shadows.
//!
//! Shadowing was the tempting alternative — let the plugin win, since a user
//! who installed it presumably wanted it — and it is wrong here for a reason
//! specific to slash commands: a slash command is muscle memory. `/clear` is
//! typed without reading, and a plugin that quietly took it would be discovered
//! by losing a conversation. The opposite failure (a plugin's `/todo` not
//! appearing) is discovered by reading `/help`, which is a thing a user can
//! actually diagnose.
//!
//! Refusing also matches what the kernel already does for tool and provider
//! names — "Names are owned", `crate::kernel`'s `claim` — so a plugin author
//! learns one rule rather than three.
//!
//! The refusal is not fatal by construction: `Ctx::command` returns a `Result`,
//! so a plugin that would rather register under a fallback name can catch it.
//! Because a plugin that discards the error would otherwise fail silently,
//! [`install`] also logs a warning naming both sides — which keeps the "plugin
//! load logs a warning" half of the recommendation without giving up the
//! refusal.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::surface::Surface;
use super::{COMMANDS, Execution};

/// What a plugin command's handler returns: the text to show, or the error to
/// report. An empty string means "nothing to say" and prints nothing.
pub type CommandFuture = Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'static>>;

/// The body of a plugin-registered slash command.
///
/// One argument, one answer. A plugin command deliberately cannot reach the
/// `CommandSurface` verbs the built-ins dispatch through: those verbs are
/// `&mut Agent`, `&mut App` and a window's action list, and handing a plugin any
/// of them would make unload unsafe in a way the ledger cannot fix. A plugin
/// that wants to change the session does it through the event bus and its own
/// tools.
pub trait CommandHandler: Send + Sync + 'static {
    fn run(&self, args: String) -> CommandFuture;
}

impl<F, Fut> CommandHandler for F
where
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<String>> + Send + 'static,
{
    fn run(&self, args: String) -> CommandFuture {
        Box::pin(self(args))
    }
}

/// One `/name` a plugin added to the palette: a [`CommandSpec`] with a body
/// attached and a runtime lifetime.
///
/// The per-surface columns are three named fields rather than a set, mirroring
/// [`CommandSpec`] exactly: a fourth [`Surface`] is then a field this struct has
/// to answer for before the crate compiles again, which is the property that
/// keeps a plugin command from quietly missing a surface.
///
/// [`CommandSpec`]: super::CommandSpec
#[derive(Clone)]
pub struct PluginCommand {
    /// Without the leading slash.
    pub name: String,
    /// Argument hint shown after the name in the palette, e.g. `[id]`. Empty
    /// when the command takes none.
    pub args: String,
    /// One line for the palette and for `/help`.
    pub description: String,
    /// Completion appends a trailing space and waits for arguments instead of
    /// submitting immediately. Set by [`PluginCommand::args`].
    pub takes_args: bool,
    tui: bool,
    gui: bool,
    gateway: bool,
    handler: Arc<dyn CommandHandler>,
}

impl PluginCommand {
    /// A command available on every surface.
    ///
    /// Available everywhere is the right default because the handler is a plain
    /// `args -> text` function: there is no picker to open, no panel to draw and
    /// no keystroke to bind, so there is nothing a surface could fail to
    /// provide. A plugin narrows it with [`PluginCommand::only`] when its answer
    /// genuinely does not travel — output that is only meaningful in a terminal,
    /// say.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        handler: Arc<dyn CommandHandler>,
    ) -> Self {
        Self {
            name: name.into(),
            args: String::new(),
            description: description.into(),
            takes_args: false,
            tui: true,
            gui: true,
            gateway: true,
            handler,
        }
    }

    /// Declare an argument hint, which also makes completion wait for the
    /// arguments rather than submitting on Tab.
    pub fn args(mut self, hint: impl Into<String>) -> Self {
        self.args = hint.into();
        self.takes_args = !self.args.is_empty();
        self
    }

    /// Restrict the command to the named surfaces. Everywhere else it is
    /// [`Execution::Unavailable`]: offered nowhere, and refused by name with the
    /// same sentence a built-in gets.
    ///
    /// `only(&[])` is a command available nowhere, which is a plugin bug rather
    /// than a thing to defend against — it is registered, it shows up in
    /// [`names`], and it is refused wherever it is typed.
    pub fn only(mut self, surfaces: &[Surface]) -> Self {
        self.tui = surfaces.contains(&Surface::Tui);
        self.gui = surfaces.contains(&Surface::Gui);
        self.gateway = surfaces.contains(&Surface::Gateway);
        self
    }

    /// How `surface` runs it.
    ///
    /// [`Execution::Agent`] wherever it is available, on every surface, and that
    /// is one rule rather than a table's worth of judgement calls. The
    /// `Agent`/`Ui` split answers "which half of a two-halved surface owns this
    /// command's semantics", and a plugin command's semantics live in neither
    /// half — they are in the plugin. What the split actually *decides* is where
    /// the dispatch runs, and the agent-holding half is the honest answer: it is
    /// the half with a runtime, it is the only half the gateway has at all
    /// (nothing there may be [`Execution::Ui`]), and it puts the command's
    /// output in the transcript in the order it was typed.
    pub fn execution(&self, surface: Surface) -> Execution {
        let available = match surface {
            Surface::Tui => self.tui,
            Surface::Gui => self.gui,
            Surface::Gateway => self.gateway,
        };
        match available {
            true => Execution::Agent,
            false => Execution::Unavailable,
        }
    }

    /// Run it. `args` is the rest of the typed line, untouched — the same
    /// whole-rest-of-line rule `/btw` and `/fork` follow, because a plugin's
    /// argument grammar is the plugin's business and re-joining
    /// whitespace-split tokens would quietly rewrite it.
    pub async fn run(&self, args: impl Into<String>) -> anyhow::Result<String> {
        self.handler.run(args.into()).await
    }
}

impl fmt::Debug for PluginCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginCommand")
            .field("name", &self.name)
            .field("args", &self.args)
            .field("description", &self.description)
            .field("tui", &self.tui)
            .field("gui", &self.gui)
            .field("gateway", &self.gateway)
            .finish()
    }
}

/* ---------------------------------------------------------------------- */
/* The registry                                                           */
/* ---------------------------------------------------------------------- */

/// Command words the parser answers to that are not rows of [`COMMANDS`].
///
/// `/q` is `/quit`'s short alias and has no row of its own, so a plugin that
/// took it would shadow a built-in without [`install`] ever seeing a table row
/// to refuse against. `a_plugin_cannot_take_a_word_the_parser_already_answers_to`
/// holds this list to the parser.
const PARSE_ALIASES: &[&str] = &["q"];

/// The process-wide registry every unknown `/word` resolves against.
///
/// `BTreeMap` so [`all`] and [`names`] come out in a stable order: a palette
/// that reordered itself between keystrokes because a `HashMap` rehashed would
/// be a real bug, and sorting on every read is worse.
static REGISTERED: LazyLock<RwLock<BTreeMap<String, PluginCommand>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

fn read() -> RwLockReadGuard<'static, BTreeMap<String, PluginCommand>> {
    REGISTERED
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write() -> RwLockWriteGuard<'static, BTreeMap<String, PluginCommand>> {
    REGISTERED
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Why a name could not be taken. Names the holder, because "already
/// registered" without saying by whom is a bug report about the wrong plugin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the slash command '/{name}' is already {holder}")]
pub struct NameTaken {
    pub name: String,
    /// What holds it: `a built-in`, or `registered by another plugin`.
    pub holder: String,
}

/// Whether `name` is a built-in command word — a row of [`COMMANDS`] or one of
/// the [`PARSE_ALIASES`]. Plugin commands are deliberately not included: this is
/// the question "may a plugin take this name", asked by [`install`].
pub fn is_builtin(name: &str) -> bool {
    COMMANDS.iter().any(|spec| spec.name == name) || PARSE_ALIASES.contains(&name)
}

/// Register a command process-wide, or refuse because its name is taken.
///
/// `owner` is the plugin, used only in the refusal and the warning. See the
/// module docs for why this refuses rather than shadows.
pub fn install(owner: &str, command: PluginCommand) -> Result<(), NameTaken> {
    let refuse = |holder: &str, why: &str| {
        // Logged as well as returned: a plugin is free to discard the error and
        // carry on, and a `/name` that silently never appeared is the hardest
        // kind of missing to diagnose.
        tracing::warn!(
            plugin = owner,
            command = command.name.as_str(),
            "plugin slash command refused: {why}"
        );
        Err(NameTaken {
            name: command.name.clone(),
            holder: holder.to_string(),
        })
    };
    if is_builtin(&command.name) {
        return refuse(
            "a built-in",
            "the name is a built-in command and the built-in keeps it",
        );
    }
    let mut map = write();
    if map.contains_key(&command.name) {
        return refuse(
            "registered by another plugin",
            "another plugin registered the name first",
        );
    }
    map.insert(command.name.clone(), command);
    Ok(())
}

/// Withdraw a command, e.g. because the plugin that registered it unloaded.
pub fn uninstall(name: &str) -> bool {
    write().remove(name).is_some()
}

/// The command registered under `name`, if any.
///
/// Cloned out rather than handed back behind the lock: the struct is three
/// `String`s and an `Arc`, and a caller holding a read guard across an `await`
/// on the handler would block every registration for the length of the call.
pub fn get(name: &str) -> Option<PluginCommand> {
    read().get(name).cloned()
}

/// Every registered command, in name order.
pub fn all() -> Vec<PluginCommand> {
    read().values().cloned().collect()
}

/// Every registered name, in order.
///
/// The counterpart of `Kernel::command_names` for code that has no kernel
/// handle — which, per the module docs, is most of the tree.
pub fn names() -> Vec<String> {
    read().keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Listing, SlashCommand, available, listing};

    /// A registration that withdraws itself, because these tests share one
    /// process-wide registry with every other test in the binary and a leaked
    /// `/name` would show up in somebody else's palette assertion.
    struct Registered(String);

    impl Drop for Registered {
        fn drop(&mut self) {
            uninstall(&self.0);
        }
    }

    /// Names are deliberately odd: the suggestion tests elsewhere assert exact
    /// prefix and substring matches over the merged palette, so a probe called
    /// anything ordinary would join those lists while this test held it.
    fn probe(name: &str) -> PluginCommand {
        PluginCommand::new(
            name,
            "a probe",
            Arc::new(|args: String| async move { Ok(format!("ran with '{args}'")) }),
        )
    }

    fn register(command: PluginCommand) -> Registered {
        let name = command.name.clone();
        install("probe-plugin", command).expect("the name is free");
        Registered(name)
    }

    fn row(rows: &[Listing], name: &str) -> Option<Listing> {
        rows.iter().find(|row| row.name == name).cloned()
    }

    #[test]
    fn a_registered_command_parses_into_the_open_variant_with_the_raw_tail() {
        let _held = register(probe("zzprobe").args("[thing]"));
        assert_eq!(
            SlashCommand::parse("/zzprobe  two  words "),
            Some(Ok(SlashCommand::Plugin {
                name: "zzprobe".to_string(),
                // Whitespace inside the tail is the plugin's to interpret, so
                // it survives; only the leading and trailing runs are trimmed.
                args: "two  words".to_string(),
            }))
        );
        assert_eq!(
            SlashCommand::parse("/zzprobe"),
            Some(Ok(SlashCommand::Plugin {
                name: "zzprobe".to_string(),
                args: String::new(),
            }))
        );
    }

    #[test]
    fn a_registered_command_is_a_palette_row_on_every_surface() {
        let _held = register(probe("zzpalette").args("[thing]"));
        for &at in Surface::ALL {
            let row = row(&listing(at), "zzpalette").expect("listed");
            assert_eq!(row.execution, Execution::Agent);
            assert_eq!(row.args, "[thing]");
            assert!(row.takes_args, "an argument hint waits for arguments");
            assert!(row.from_plugin);
            assert_eq!(
                row.description, "a probe",
                "the plugin's own description, not a placeholder"
            );
        }
        // Built-ins keep the table's order and plugin rows follow them, rather
        // than being interleaved into a list people navigate by position.
        let rows = listing(Surface::Tui);
        let first_plugin = rows.iter().position(|row| row.from_plugin).expect("listed");
        assert!(rows[..first_plugin].iter().all(|row| !row.from_plugin));
        assert_eq!(rows[0].name, COMMANDS[0].name);
    }

    #[test]
    fn a_registered_command_can_be_restricted_to_one_surface() {
        let _held = register(probe("zzonlytui").only(&[Surface::Tui]));
        let command = get("zzonlytui").expect("registered");
        assert_eq!(command.execution(Surface::Tui), Execution::Agent);
        assert_eq!(command.execution(Surface::Gui), Execution::Unavailable);
        assert_eq!(command.execution(Surface::Gateway), Execution::Unavailable);

        // And the restriction is what every surface reads, not a second rule
        // each of them has to remember.
        assert!(row(&available(Surface::Tui), "zzonlytui").is_some());
        assert!(row(&available(Surface::Gui), "zzonlytui").is_none());
        assert!(row(&available(Surface::Gateway), "zzonlytui").is_none());

        let parsed = SlashCommand::parse("/zzonlytui")
            .expect("known")
            .expect("ok");
        assert_eq!(parsed.execution(Surface::Tui), Execution::Agent);
        assert_eq!(parsed.execution(Surface::Gui), Execution::Unavailable);
    }

    /// The conflict policy, in one test: the built-in keeps the name, and the
    /// refused claim leaves nothing behind.
    #[test]
    fn a_built_in_keeps_a_name_a_plugin_asks_for() {
        let err = install("impostor", probe("clear")).expect_err("a built-in holds it");
        assert_eq!(err.holder, "a built-in");
        assert!(get("clear").is_none(), "nothing was left behind");
        assert_eq!(SlashCommand::parse("/clear"), Some(Ok(SlashCommand::Clear)));
    }

    /// `/q` has no row of its own — it is an arm of the parser — so the table
    /// alone would not have refused it.
    #[test]
    fn a_plugin_cannot_take_a_word_the_parser_already_answers_to() {
        for word in ["q", "exit", "quit", "genie", "sovereign"] {
            assert!(
                is_builtin(word),
                "/{word} is a word the parser answers to and must not be takeable"
            );
            assert!(install("impostor", probe(word)).is_err(), "/{word}");
        }
        assert_eq!(SlashCommand::parse("/q"), Some(Ok(SlashCommand::Quit)));
    }

    #[test]
    fn the_first_plugin_to_register_a_name_keeps_it() {
        let _held = register(probe("zzcontested"));
        let err = install("second", probe("zzcontested")).expect_err("taken");
        assert_eq!(err.holder, "registered by another plugin");
        // The refusal did not replace the handler that was already there.
        assert_eq!(
            get("zzcontested").expect("still there").description,
            "a probe"
        );
    }

    #[test]
    fn an_unregistered_word_is_still_an_unknown_command() {
        assert!(get("zzabsent").is_none());
        assert!(
            matches!(SlashCommand::parse("/zzabsent"), Some(Err(message)) if message.contains("unknown command"))
        );
        assert!(!crate::commands::is_known("zzabsent"));
    }

    /// Withdrawing is what makes an unload exact from the palette's side: the
    /// word goes back to being an unknown command.
    #[test]
    fn withdrawing_takes_the_word_back_out_of_the_parser() {
        {
            let _held = register(probe("zzwithdraw"));
            assert!(SlashCommand::parse("/zzwithdraw").expect("known").is_ok());
        }
        assert!(matches!(SlashCommand::parse("/zzwithdraw"), Some(Err(_))));
        assert!(row(&listing(Surface::Tui), "zzwithdraw").is_none());
    }

    /// A plugin command is not on the agent's `run_command` allowlist, and the
    /// refusal says where to look instead. See `SlashCommand::agent_runnable`.
    #[test]
    fn the_agent_is_not_offered_a_plugin_command() {
        let _held = register(probe("zzagent"));
        let parsed = SlashCommand::parse("/zzagent").expect("known").expect("ok");
        let refusal = parsed.agent_runnable().expect_err("not the agent's");
        assert!(refusal.contains("plugin"), "{refusal}");
    }

    #[tokio::test]
    async fn the_handler_gets_the_raw_tail_and_its_answer_comes_back() {
        let _held = register(probe("zzhandler"));
        let command = get("zzhandler").expect("registered");
        assert_eq!(
            command.run("a  b").await.expect("ran"),
            "ran with 'a  b'".to_string()
        );
    }
}
