//! Load, unload, and the ledger that makes unload exact.
//!
//! From `docs/plugins.md`: "The reason to have a kernel at all is that unload
//! has to be exact." Without a ledger, "reload" is a leak with good intentions
//! and the third reload of a plugin in a long session is a different program
//! from the first — a tool the model can still call whose closure points at a
//! dropped VM, an event handler that still vetoes on behalf of nobody, a
//! service still answering `inject` after its provider is gone.
//!
//! So every registration a [`Ctx`](super::Ctx) makes is written into that
//! plugin's [`Ledger`] at the moment it is made, and [`dispose`] undoes all of
//! them in one step. Three details are what make it exact rather than
//! approximate:
//!
//! - **The ledger is the record, and a sweep is the backstop.** `dispose`
//!   removes each recorded name, then asks the bus and the service registry to
//!   sweep anything left that belongs to this plugin. A leaked handler is a
//!   dead plugin still blocking tool calls, which is the failure worth paying
//!   two passes for.
//! - **Effects run last, and in reverse.** [`Ctx::effect`](super::Ctx::effect)
//!   is the escape hatch for state the kernel cannot see: an open socket, a
//!   temp directory, a child process. They run after the registries are clear,
//!   so a teardown cannot be racing a tool call that is still reachable, and
//!   LIFO like `Drop`, so a plugin that opened A then B tears down B then A.
//! - **A teardown that panics does not stop the unload.** It is recorded and
//!   the next one runs. A plugin gets to leak its own socket; it does not get
//!   to leave every later plugin loaded.
//!
//! Children unload with their parent. `ctx:plugin(child, config)` records the
//! child here, and disposing the parent disposes the child first, because a
//! child that outlived its parent would hold a `Ctx` whose config slice no
//! longer exists.

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use super::bus::HandlerId;
use super::manifest::{PluginManifest, PluginSource};

/// A loaded plugin's identity, and the key every registration is recorded
/// against.
///
/// The name, not a serial: a plugin's name is unique among loaded plugins
/// (the kernel refuses a second load of a taken name) and a name is what a
/// user types into `/plugin unload`. An `Arc<str>` because it is cloned into
/// every subscription, every ledger entry and every log line.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginId(Arc<str>);

impl PluginId {
    pub fn new(name: impl AsRef<str>) -> Self {
        PluginId(Arc::from(name.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for PluginId {
    fn from(name: &str) -> Self {
        PluginId::new(name)
    }
}

/// One teardown a plugin asked for.
///
/// Labelled because the only time anybody reads this is when one of them
/// failed, and "an effect panicked" without saying which is a bug report
/// nobody can act on.
pub struct Effect {
    label: String,
    run: Box<dyn FnOnce() + Send>,
}

impl Effect {
    pub fn new(label: impl Into<String>, run: impl FnOnce() + Send + 'static) -> Self {
        Self {
            label: label.into(),
            run: Box::new(run),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

impl fmt::Debug for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Effect")
            .field("label", &self.label)
            .finish()
    }
}

/// Everything one plugin registered, in the order it registered it.
///
/// Public fields would let a caller record a registration it did not make, so
/// the pushes are methods and the reads are counts. The kernel is the only
/// thing that fills one in.
#[derive(Default)]
pub struct Ledger {
    tools: Vec<String>,
    commands: Vec<String>,
    providers: Vec<String>,
    handlers: Vec<HandlerId>,
    services: Vec<String>,
    children: Vec<PluginId>,
    effects: Vec<Effect>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record_tool(&mut self, name: impl Into<String>) {
        self.tools.push(name.into());
    }

    pub(crate) fn record_command(&mut self, name: impl Into<String>) {
        self.commands.push(name.into());
    }

    pub(crate) fn record_provider(&mut self, name: impl Into<String>) {
        self.providers.push(name.into());
    }

    pub(crate) fn record_handler(&mut self, id: HandlerId) {
        self.handlers.push(id);
    }

    pub(crate) fn record_service(&mut self, name: impl Into<String>) {
        self.services.push(name.into());
    }

    pub(crate) fn record_child(&mut self, child: PluginId) {
        self.children.push(child);
    }

    pub(crate) fn record_effect(&mut self, effect: Effect) {
        self.effects.push(effect);
    }

    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    pub fn commands(&self) -> &[String] {
        &self.commands
    }

    pub fn providers(&self) -> &[String] {
        &self.providers
    }

    pub fn services(&self) -> &[String] {
        &self.services
    }

    pub fn handlers(&self) -> &[HandlerId] {
        &self.handlers
    }

    pub fn children(&self) -> &[PluginId] {
        &self.children
    }

    pub fn effect_count(&self) -> usize {
        self.effects.len()
    }

    /// Registrations of every kind. Effects count: a plugin whose whole
    /// contribution is a teardown still has something to dispose.
    pub fn len(&self) -> usize {
        self.tools.len()
            + self.commands.len()
            + self.providers.len()
            + self.handlers.len()
            + self.services.len()
            + self.children.len()
            + self.effects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take the effects out, newest first. Used by [`dispose`], and separate
    /// from the rest because effects are the only entries that are consumed
    /// rather than read.
    fn drain_effects(&mut self) -> Vec<Effect> {
        let mut effects = std::mem::take(&mut self.effects);
        effects.reverse();
        effects
    }
}

impl fmt::Debug for Ledger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ledger")
            .field("tools", &self.tools)
            .field("commands", &self.commands)
            .field("providers", &self.providers)
            .field("services", &self.services)
            .field("handlers", &self.handlers.len())
            .field("children", &self.children)
            .field("effects", &self.effects.len())
            .finish()
    }
}

/// What one unload actually undid.
///
/// Returned rather than logged so a test can assert on it and `/plugin unload`
/// can print it. The counts are what was *removed*, not what was recorded:
/// they differ exactly when something else had already taken a name over, and
/// that difference is the interesting one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DisposalReport {
    pub plugin: String,
    pub tools: usize,
    pub commands: usize,
    pub providers: usize,
    pub handlers: usize,
    pub services: usize,
    pub effects: usize,
    /// Teardowns that panicked, by label. Recorded rather than propagated: see
    /// the module docs.
    pub effect_failures: Vec<String>,
    /// Children disposed along with this plugin, in the order they went.
    pub children: Vec<String>,
}

impl DisposalReport {
    /// Everything this unload took out, children included.
    pub fn total(&self) -> usize {
        self.tools + self.commands + self.providers + self.handlers + self.services + self.effects
    }

    /// Fold a child's report into the parent's, so one unload reports one
    /// total.
    fn absorb(&mut self, child: DisposalReport) {
        self.tools += child.tools;
        self.commands += child.commands;
        self.providers += child.providers;
        self.handlers += child.handlers;
        self.services += child.services;
        self.effects += child.effects;
        self.effect_failures.extend(child.effect_failures);
        self.children.push(child.plugin);
        self.children.extend(child.children);
    }
}

/// Which world a loaded plugin's code lives in.
///
/// The kernel does not branch on this for anything a plugin can observe — that
/// is the whole point of the `Ctx` shape being identical in all three
/// languages — but disposal does: dropping a scripted plugin has to stop its
/// VM task, and dropping a Rust one has to drop an `Arc`.
///
/// The JavaScript arm is behind its feature rather than always present with a
/// value nothing constructs, so a build without `plugin-js` does not carry a
/// variant that cannot happen. That costs one `#[cfg]` per `match`, which is
/// two `match`es in the whole tree.
pub enum PluginKind {
    Rust(Arc<dyn super::Plugin>),
    Lua(super::lua::LuaPlugin),
    #[cfg(feature = "plugin-js")]
    Js(super::js::JsPlugin),
}

impl fmt::Debug for PluginKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginKind::Rust(_) => f.write_str("Rust"),
            PluginKind::Lua(_) => f.write_str("Lua"),
            #[cfg(feature = "plugin-js")]
            PluginKind::Js(_) => f.write_str("Js"),
        }
    }
}

impl PluginKind {
    /// True when disposal has an async half — a VM task to stop and teardowns
    /// to run inside it.
    ///
    /// A Rust plugin's teardowns are `FnOnce`s the ledger already ran
    /// synchronously, so it answers `false` and is dropped where it stands
    /// rather than being carried to the end of [`crate::kernel::Kernel::unload`].
    pub(crate) fn has_vm(&self) -> bool {
        match self {
            PluginKind::Rust(_) => false,
            PluginKind::Lua(_) => true,
            #[cfg(feature = "plugin-js")]
            PluginKind::Js(_) => true,
        }
    }

    /// Run this plugin's in-VM teardowns and stop its VM.
    ///
    /// One method rather than a `match` at the call site because the caller —
    /// `unload` — has nothing to say about which language a plugin is written
    /// in, and every place that learns the answer is a place a third backend
    /// has to be remembered.
    pub(crate) async fn shutdown(self) -> super::VmShutdown {
        match self {
            PluginKind::Rust(_) => super::VmShutdown::default(),
            PluginKind::Lua(vm) => vm.shutdown().await,
            #[cfg(feature = "plugin-js")]
            PluginKind::Js(vm) => vm.shutdown().await,
        }
    }
}

/// A plugin the kernel is holding open.
pub struct LoadedPlugin {
    pub id: PluginId,
    pub manifest: Arc<PluginManifest>,
    pub source: PluginSource,
    /// The plugin that loaded this one through `ctx:plugin`, if any.
    pub parent: Option<PluginId>,
    pub kind: PluginKind,
    pub ledger: Ledger,
}

impl fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("id", &self.id)
            .field("source", &self.source)
            .field("parent", &self.parent)
            .field("kind", &self.kind)
            .field("ledger", &self.ledger)
            .finish()
    }
}

/// Run every teardown in `effects`, newest first, and return the labels of the
/// ones that panicked.
///
/// `catch_unwind` per effect rather than around the loop: the loop is the
/// thing that must finish. A plugin whose socket close panics has leaked a
/// socket, which is its own problem; the temp directory the *next* effect
/// removes is the user's problem, and it gets removed.
fn run_effects(plugin: &PluginId, effects: Vec<Effect>) -> (usize, Vec<String>) {
    let mut ran = 0;
    let mut failures = Vec::new();
    for effect in effects {
        let Effect { label, run } = effect;
        match std::panic::catch_unwind(AssertUnwindSafe(run)) {
            Ok(()) => ran += 1,
            Err(_) => {
                tracing::error!(
                    plugin = %plugin,
                    effect = %label,
                    "a plugin teardown panicked; continuing the unload"
                );
                failures.push(label);
            }
        }
    }
    (ran, failures)
}

/// Undo everything in `ledger`, in the order that leaves nothing reachable at
/// any point in between.
///
/// Registries first, then children, then effects. The ordering is not
/// cosmetic: deregistering the tools before running the teardowns means no
/// concurrent turn can start a call into a plugin whose socket is closing, and
/// disposing children before running the parent's effects means a child's own
/// teardown cannot depend on state the parent has already dropped.
///
/// `dispose_child` is how recursion reaches back into the kernel — it takes a
/// child's id and returns that child's report — rather than this function
/// holding a kernel reference of its own, which would make the lock discipline
/// impossible to see from here.
pub(crate) fn dispose(
    slots: &super::Slots,
    plugin: &PluginId,
    mut ledger: Ledger,
    mut dispose_child: impl FnMut(&PluginId) -> Option<DisposalReport>,
) -> DisposalReport {
    let mut report = DisposalReport {
        plugin: plugin.to_string(),
        ..DisposalReport::default()
    };

    report.tools = slots.remove_tools(ledger.tools());
    report.commands = slots.remove_commands(ledger.commands());
    report.providers = slots.remove_providers(ledger.providers());

    for name in ledger.services() {
        if slots.services.withdraw_owned(plugin, name).is_some() {
            report.services += 1;
        }
    }
    for id in ledger.handlers() {
        if slots.bus.unsubscribe(*id) {
            report.handlers += 1;
        }
    }

    // The sweep. Everything above went through a recorded name; these two
    // catch anything registered through a path that forgot to record, which is
    // the only class of leak the ledger cannot see.
    report.handlers += slots.bus.unsubscribe_plugin(plugin);
    report.services += slots.services.withdraw_all(plugin);

    for child in ledger.children() {
        if let Some(child_report) = dispose_child(child) {
            report.absorb(child_report);
        }
    }

    // `+=`, not `=`: the loop above already folded every child's effect count
    // in, and assigning here would report a parent's own teardowns as though
    // they were all of them. The child's socket really did close; a report that
    // says otherwise is how a leak gets closed as "cannot reproduce".
    let (ran, failures) = run_effects(plugin, ledger.drain_effects());
    report.effects += ran;
    report.effect_failures.extend(failures);

    report
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn a_plugin_id_is_its_name() {
        let id = PluginId::from("web");
        assert_eq!(id.as_str(), "web");
        assert_eq!(id.to_string(), "web");
        assert_eq!(id, PluginId::new(String::from("web")));
        assert_ne!(id, PluginId::new("git"));
    }

    #[test]
    fn an_empty_ledger_has_nothing_to_dispose() {
        let ledger = Ledger::new();
        assert!(ledger.is_empty());
        assert_eq!(ledger.len(), 0);
        assert_eq!(ledger.effect_count(), 0);
        assert!(ledger.tools().is_empty());
        assert!(ledger.commands().is_empty());
        assert!(ledger.providers().is_empty());
        assert!(ledger.services().is_empty());
        assert!(ledger.handlers().is_empty());
        assert!(ledger.children().is_empty());
    }

    #[test]
    fn a_ledger_counts_every_kind_of_registration() {
        let mut ledger = Ledger::new();
        ledger.record_tool("todo");
        ledger.record_command("todo");
        ledger.record_provider("local");
        ledger.record_service("todo");
        ledger.record_child(PluginId::new("todo-ui"));
        ledger.record_effect(Effect::new("close the socket", || {}));
        assert_eq!(ledger.len(), 6);
        assert!(!ledger.is_empty());
        // The Debug impl is what a stuck unload gets read through, so it has
        // to name what is in the ledger rather than its size alone.
        let rendered = format!("{ledger:?}");
        assert!(rendered.contains("todo"), "{rendered}");
        assert!(rendered.contains("local"), "{rendered}");
    }

    #[test]
    fn effects_run_newest_first_like_drop() {
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let mut ledger = Ledger::new();
        for tag in ["first", "second", "third"] {
            let order = Arc::clone(&order);
            ledger.record_effect(Effect::new(tag, move || order.lock().unwrap().push(tag)));
        }
        let (ran, failures) = run_effects(&PluginId::new("p"), ledger.drain_effects());
        assert_eq!(ran, 3);
        assert!(failures.is_empty());
        assert_eq!(*order.lock().unwrap(), ["third", "second", "first"]);
        assert_eq!(ledger.effect_count(), 0, "draining consumes them");
    }

    #[test]
    fn a_teardown_that_panics_does_not_stop_the_ones_after_it() {
        let ran = Arc::new(AtomicUsize::new(0));
        let mut ledger = Ledger::new();
        let counter = Arc::clone(&ran);
        ledger.record_effect(Effect::new("innocent", move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        ledger.record_effect(Effect::new("guilty", || {
            panic!("the socket was already closed")
        }));

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let (ok, failures) = run_effects(&PluginId::new("p"), ledger.drain_effects());
        std::panic::set_hook(previous);

        assert_eq!(ok, 1);
        assert_eq!(failures, ["guilty"]);
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "the effect registered before the panicking one still ran"
        );
    }

    #[test]
    fn an_effect_says_what_it_is() {
        let effect = Effect::new("remove the temp dir", || {});
        assert_eq!(effect.label(), "remove the temp dir");
        assert!(format!("{effect:?}").contains("remove the temp dir"));
    }

    #[test]
    fn a_report_folds_a_childs_numbers_into_the_parents() {
        let mut parent = DisposalReport {
            plugin: "parent".into(),
            tools: 1,
            handlers: 2,
            ..DisposalReport::default()
        };
        parent.absorb(DisposalReport {
            plugin: "child".into(),
            tools: 3,
            services: 1,
            effect_failures: vec!["a socket".into()],
            children: vec!["grandchild".into()],
            ..DisposalReport::default()
        });
        assert_eq!(parent.tools, 4);
        assert_eq!(parent.services, 1);
        assert_eq!(parent.handlers, 2);
        assert_eq!(parent.total(), 7);
        assert_eq!(parent.children, ["child", "grandchild"]);
        assert_eq!(parent.effect_failures, ["a socket"]);
    }

    #[test]
    fn plugin_kind_debug_names_the_language_and_nothing_else() {
        struct Noop;
        impl super::super::Plugin for Noop {
            fn manifest(&self) -> &PluginManifest {
                unreachable!("not called by this test")
            }
            fn apply(&self, _ctx: &mut super::super::Ctx) -> anyhow::Result<()> {
                Ok(())
            }
        }
        assert_eq!(format!("{:?}", PluginKind::Rust(Arc::new(Noop))), "Rust");
    }
}
