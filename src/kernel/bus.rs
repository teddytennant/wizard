//! The async event bus: ordered handlers that may observe, rewrite, or veto.
//!
//! This is the interception surface `src/hooks/` has today, generalised. A
//! `hooks.toml` entry can rewrite `pre_tool_use` arguments, append context, or
//! exit 2 to block; every one of those is a [`Verdict`] here, and a shell hook
//! becomes a plugin that subscribes rather than a second mechanism with its own
//! payload format and its own failure semantics.
//!
//! Three properties are load-bearing, and the tests at the bottom exist for
//! them rather than for the happy path:
//!
//! - **Order is total and stable.** Handlers run by ascending priority, and
//!   handlers that tie run in subscription order. A bus whose order depends on
//!   `HashMap` iteration is a bus where a plugin's veto lands or does not land
//!   depending on the allocator.
//! - **A rewrite is visible to everything downstream.** A handler that returns
//!   [`Verdict::Rewrite`] changes the payload the *next* handler sees, not just
//!   the one the caller gets back. Otherwise "rewrite the arguments" means
//!   "rewrite them unless somebody else also did", which is not a contract
//!   anybody can write against.
//! - **A broken handler cannot wedge a turn.** An error is logged and skipped;
//!   so is a panic, which is why every handler future is driven inside
//!   `catch_unwind`. `src/hooks/` gives this guarantee today for shell hooks
//!   (any exit code that is not 0 or 2 is a warning and the pipeline
//!   continues), and a plugin bus that gave less would be a regression dressed
//!   as a generalisation.
//!
//! A veto stops the chain. That is the one place where a handler's decision is
//! allowed to cost another handler its run, and it has to be: a `pre_tool_use`
//! subscriber that blocked a command would look ridiculous if a lower-priority
//! subscriber then rewrote the arguments of a call that is not going to happen.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::FutureExt;
use serde_json::Value;

use super::lifecycle::PluginId;

/// A lifecycle point a plugin can subscribe to.
///
/// The six that [`crate::hooks::HookEvent`] already has keep their exact names,
/// so a `hooks.toml` file and a plugin subscription name the same instant. The
/// rest are the points Lua plugins need and `hooks.toml` never had: the model
/// call itself, compaction, checkpoints, and the plugin graph changing under
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Event {
    SessionStart,
    SessionEnd,
    UserPrompt,
    TurnStart,
    TurnEnd,
    PreToolUse,
    PostToolUse,
    PreModelCall,
    PostModelCall,
    Compaction,
    Checkpoint,
    PluginLoaded,
    PluginUnloaded,
    ConfigReload,
}

impl Event {
    /// Every event, in the order `docs/plugins.md` lists them. A test walks
    /// this so a name added to the enum without a spelling fails loudly.
    pub const ALL: [Event; 14] = [
        Event::SessionStart,
        Event::SessionEnd,
        Event::UserPrompt,
        Event::TurnStart,
        Event::TurnEnd,
        Event::PreToolUse,
        Event::PostToolUse,
        Event::PreModelCall,
        Event::PostModelCall,
        Event::Compaction,
        Event::Checkpoint,
        Event::PluginLoaded,
        Event::PluginUnloaded,
        Event::ConfigReload,
    ];

    /// The snake_case spelling a plugin subscribes with, from Lua or from
    /// `hooks.toml`.
    pub fn name(self) -> &'static str {
        match self {
            Event::SessionStart => "session_start",
            Event::SessionEnd => "session_end",
            Event::UserPrompt => "user_prompt",
            Event::TurnStart => "turn_start",
            Event::TurnEnd => "turn_end",
            Event::PreToolUse => "pre_tool_use",
            Event::PostToolUse => "post_tool_use",
            Event::PreModelCall => "pre_model_call",
            Event::PostModelCall => "post_model_call",
            Event::Compaction => "compaction",
            Event::Checkpoint => "checkpoint",
            Event::PluginLoaded => "plugin_loaded",
            Event::PluginUnloaded => "plugin_unloaded",
            Event::ConfigReload => "config_reload",
        }
    }

    /// Resolve a subscription written as a string. `None` for a name nothing
    /// emits, which the caller turns into a load-time failure: a plugin that
    /// subscribes to `"pre_tool"` and silently never fires is worse than one
    /// that refuses to load.
    pub fn parse(raw: &str) -> Option<Event> {
        Event::ALL.into_iter().find(|event| event.name() == raw)
    }

    /// The `hooks.toml` event this one is the same instant as, where there is
    /// one. Nothing in the kernel reads it yet; it is what a later phase's
    /// shell-hook plugin uses to map one mechanism onto the other without a
    /// second table of string literals.
    pub fn hook_event(self) -> Option<crate::hooks::HookEvent> {
        match self {
            Event::SessionStart => Some(crate::hooks::HookEvent::SessionStart),
            Event::SessionEnd => Some(crate::hooks::HookEvent::SessionEnd),
            Event::UserPrompt => Some(crate::hooks::HookEvent::UserPromptSubmit),
            Event::TurnEnd => Some(crate::hooks::HookEvent::TurnEnd),
            Event::PreToolUse => Some(crate::hooks::HookEvent::PreToolUse),
            Event::PostToolUse => Some(crate::hooks::HookEvent::PostToolUse),
            _ => None,
        }
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What one handler decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Saw it, changed nothing.
    Continue,
    /// Replace the payload for every handler after this one, and for the
    /// caller.
    Rewrite(Value),
    /// Stop the chain and refuse the action, with a reason the caller shows
    /// the model or the user.
    Veto(String),
}

/// Default priority. Plugins that do not care sit here; a plugin that must run
/// before or after another picks a number on one side of it.
///
/// Zero rather than 50 because the number is signed and "before everything" is
/// a thing a plugin legitimately wants without knowing how many plugins exist.
pub const DEFAULT_PRIORITY: i32 = 0;

/// Handle to one subscription, and the only way to take it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HandlerId(u64);

impl std::fmt::Display for HandlerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handler#{}", self.0)
    }
}

/// The future a handler returns. Boxed because handlers come from three
/// different worlds — a Rust closure, an `async fn`, and a message round-trip
/// into a Lua VM task — and only a trait object can hold all three in one
/// ordered list.
pub type HandlerFuture = Pin<Box<dyn Future<Output = anyhow::Result<Verdict>> + Send + 'static>>;

/// Something that can be subscribed to an event.
pub trait EventHandler: Send + Sync + 'static {
    fn handle(&self, event: Event, payload: Value) -> HandlerFuture;
}

impl<F, Fut> EventHandler for F
where
    F: Fn(Event, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<Verdict>> + Send + 'static,
{
    fn handle(&self, event: Event, payload: Value) -> HandlerFuture {
        Box::pin(self(event, payload))
    }
}

/// A handler that misbehaved and was skipped.
///
/// Carries the plugin as well as the message because the point of recording it
/// is to be able to say *which* plugin to disable, and a stack of anonymous
/// "handler errored" lines cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub plugin: String,
    pub handler: HandlerId,
    pub message: String,
    /// True when the handler panicked rather than returning an error. Worth
    /// telling apart: an `Err` is a plugin reporting a condition, a panic is a
    /// plugin with a bug.
    pub panicked: bool,
}

/// Who vetoed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Veto {
    pub plugin: String,
    pub handler: HandlerId,
    pub reason: String,
}

/// What one [`EventBus::emit`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct Dispatch {
    /// The payload as it stands after every rewrite. Equal to the payload
    /// passed in when nothing rewrote it.
    pub payload: Value,
    /// Set when a handler refused. The chain stopped there.
    pub veto: Option<Veto>,
    /// Handlers that ran to completion, including ones that vetoed.
    pub ran: usize,
    /// Handlers that rewrote the payload.
    pub rewrites: usize,
    /// Handlers that errored or panicked and were skipped.
    pub failures: Vec<Failure>,
}

impl Dispatch {
    pub fn is_vetoed(&self) -> bool {
        self.veto.is_some()
    }

    /// The veto reason, for a caller that only needs to turn it into a message.
    pub fn veto_reason(&self) -> Option<&str> {
        self.veto.as_ref().map(|veto| veto.reason.as_str())
    }
}

/// One live subscription.
struct Subscription {
    id: HandlerId,
    plugin: PluginId,
    priority: i32,
    handler: Arc<dyn EventHandler>,
}

#[derive(Default)]
struct BusState {
    subscriptions: HashMap<Event, Vec<Subscription>>,
}

/// The bus. Cheap to clone — every clone is the same bus — because a `Ctx`
/// hands one to every plugin and a handler that wants to emit needs one of its
/// own.
#[derive(Clone, Default)]
pub struct EventBus {
    state: Arc<Mutex<BusState>>,
    next_id: Arc<AtomicU64>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach `handler` to `event` on behalf of `plugin`.
    ///
    /// The plugin is recorded here and not only in the plugin's ledger because
    /// a failure has to be able to name the culprit without a second lookup
    /// that could race with an unload.
    pub fn subscribe(
        &self,
        plugin: &PluginId,
        event: Event,
        priority: i32,
        handler: Arc<dyn EventHandler>,
    ) -> HandlerId {
        let id = HandlerId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let list = state.subscriptions.entry(event).or_default();
        let subscription = Subscription {
            id,
            plugin: plugin.clone(),
            priority,
            handler,
        };
        // Insertion sort by (priority, id) keeps the list ordered without
        // sorting on every emit, and the id tiebreak is what makes equal
        // priorities run in subscription order rather than in whatever order
        // a comparison sort happens to leave them.
        let at = list
            .iter()
            .position(|existing| existing.priority > priority)
            .unwrap_or(list.len());
        list.insert(at, subscription);
        id
    }

    /// Detach one subscription. Returns whether it was there, so a double
    /// unsubscribe is observable in a test rather than silently fine.
    pub fn unsubscribe(&self, id: HandlerId) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for list in state.subscriptions.values_mut() {
            if let Some(at) = list.iter().position(|sub| sub.id == id) {
                list.remove(at);
                return true;
            }
        }
        false
    }

    /// Detach everything `plugin` subscribed, and return how many went.
    ///
    /// The plugin's ledger already holds every [`HandlerId`] it took, so this
    /// is belt and braces — but it is the belt: an unload that trusted the
    /// ledger alone would leak any subscription made through a path that
    /// forgot to record one, and a leaked handler is a dead plugin still
    /// vetoing tool calls.
    pub fn unsubscribe_plugin(&self, plugin: &PluginId) -> usize {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut removed = 0;
        for list in state.subscriptions.values_mut() {
            let before = list.len();
            list.retain(|sub| &sub.plugin != plugin);
            removed += before - list.len();
        }
        state.subscriptions.retain(|_, list| !list.is_empty());
        removed
    }

    /// Total live subscriptions across every event. The residue check an
    /// unload test asserts on.
    pub fn len(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.subscriptions.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many handlers are attached to one event.
    pub fn subscriber_count(&self, event: Event) -> usize {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.subscriptions.get(&event).map_or(0, Vec::len)
    }

    /// Run every handler for `event` in order.
    ///
    /// The subscription list is copied out under the lock and the lock is
    /// dropped before the first handler runs. Holding it across the awaits
    /// would deadlock the first time a handler emitted — which a Lua plugin
    /// that reacts to `pre_tool_use` by emitting its own event does on its
    /// first call — and would serialise every concurrent turn behind the
    /// slowest subscriber besides.
    pub async fn emit(&self, event: Event, payload: Value) -> Dispatch {
        let handlers: Vec<(HandlerId, PluginId, Arc<dyn EventHandler>)> = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state
                .subscriptions
                .get(&event)
                .map(|list| {
                    list.iter()
                        .map(|sub| (sub.id, sub.plugin.clone(), Arc::clone(&sub.handler)))
                        .collect()
                })
                .unwrap_or_default()
        };

        let mut dispatch = Dispatch {
            payload,
            veto: None,
            ran: 0,
            rewrites: 0,
            failures: Vec::new(),
        };

        for (id, plugin, handler) in handlers {
            let payload = dispatch.payload.clone();
            // The handler is *called* inside the guarded future, not outside
            // it. A handler that panics while building its future — an unwrap
            // on the arguments, a slice index — is as broken as one that
            // panics at an await point, and `catch_unwind` around the future
            // alone would catch only the second.
            let outcome = AssertUnwindSafe(async move { handler.handle(event, payload).await })
                .catch_unwind()
                .await;
            match outcome {
                Ok(Ok(Verdict::Continue)) => dispatch.ran += 1,
                Ok(Ok(Verdict::Rewrite(next))) => {
                    dispatch.ran += 1;
                    dispatch.rewrites += 1;
                    dispatch.payload = next;
                }
                Ok(Ok(Verdict::Veto(reason))) => {
                    dispatch.ran += 1;
                    tracing::debug!(
                        event = event.name(),
                        plugin = %plugin,
                        reason = %reason,
                        "plugin vetoed"
                    );
                    dispatch.veto = Some(Veto {
                        plugin: plugin.to_string(),
                        handler: id,
                        reason,
                    });
                    break;
                }
                Ok(Err(err)) => {
                    tracing::warn!(
                        event = event.name(),
                        plugin = %plugin,
                        error = %err,
                        "plugin event handler failed; skipping it"
                    );
                    dispatch.failures.push(Failure {
                        plugin: plugin.to_string(),
                        handler: id,
                        message: err.to_string(),
                        panicked: false,
                    });
                }
                Err(panic) => {
                    let message = panic_message(&panic);
                    tracing::error!(
                        event = event.name(),
                        plugin = %plugin,
                        panic = %message,
                        "plugin event handler panicked; skipping it"
                    );
                    dispatch.failures.push(Failure {
                        plugin: plugin.to_string(),
                        handler: id,
                        message,
                        panicked: true,
                    });
                }
            }
        }

        dispatch
    }
}

/// Best effort at the text a panic carried. `Box<dyn Any>` is almost always a
/// `&str` or a `String`; anything else is reported as such rather than as an
/// empty message, because "a plugin panicked with nothing to say" is still
/// worth a line naming the plugin.
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = panic.downcast_ref::<&'static str>() {
        (*text).to_string()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "panicked with a non-string payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use serde_json::json;

    use super::*;

    fn plugin(name: &str) -> PluginId {
        PluginId::new(name)
    }

    /// A handler built from a plain closure, which is the shape every test
    /// here wants and the blanket impl above exists to allow.
    fn handler<F>(f: F) -> Arc<dyn EventHandler>
    where
        F: Fn(Event, Value) -> anyhow::Result<Verdict> + Send + Sync + 'static,
    {
        Arc::new(move |event, payload| {
            let verdict = f(event, payload);
            async move { verdict }
        })
    }

    #[test]
    fn every_event_round_trips_through_its_name() {
        for event in Event::ALL {
            assert_eq!(Event::parse(event.name()), Some(event), "{event}");
            assert_eq!(event.to_string(), event.name());
        }
        assert_eq!(Event::parse("pre_tool"), None);
    }

    #[test]
    fn the_six_events_hooks_already_has_map_onto_it() {
        // The mapping is the promise that a shell hook and a plugin name the
        // same instant. If a HookEvent spelling ever drifts from an Event
        // spelling this is what notices.
        for event in Event::ALL {
            if let Some(hook) = event.hook_event() {
                let expected = match event {
                    Event::UserPrompt => "user_prompt_submit",
                    other => other.name(),
                };
                assert_eq!(hook.name(), expected, "{event}");
            }
        }
        assert_eq!(Event::Compaction.hook_event(), None);
    }

    #[tokio::test]
    async fn an_emit_with_no_subscribers_returns_the_payload_unchanged() {
        let bus = EventBus::new();
        assert!(bus.is_empty());
        let dispatch = bus.emit(Event::TurnStart, json!({"n": 1})).await;
        assert_eq!(dispatch.payload, json!({"n": 1}));
        assert_eq!(dispatch.ran, 0);
        assert!(!dispatch.is_vetoed());
        assert!(dispatch.failures.is_empty());
    }

    #[tokio::test]
    async fn handlers_run_in_priority_order_then_subscription_order() {
        let bus = EventBus::new();
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let record = |order: &Arc<Mutex<Vec<&'static str>>>, tag: &'static str| {
            let order = Arc::clone(order);
            handler(move |_, _| {
                order.lock().unwrap().push(tag);
                Ok(Verdict::Continue)
            })
        };

        bus.subscribe(
            &plugin("late"),
            Event::TurnStart,
            10,
            record(&order, "late"),
        );
        bus.subscribe(
            &plugin("early"),
            Event::TurnStart,
            -10,
            record(&order, "early"),
        );
        // Two at the default priority: the earlier subscription runs first.
        bus.subscribe(
            &plugin("mid-a"),
            Event::TurnStart,
            DEFAULT_PRIORITY,
            record(&order, "mid-a"),
        );
        bus.subscribe(
            &plugin("mid-b"),
            Event::TurnStart,
            DEFAULT_PRIORITY,
            record(&order, "mid-b"),
        );

        let dispatch = bus.emit(Event::TurnStart, Value::Null).await;
        assert_eq!(dispatch.ran, 4);
        assert_eq!(
            *order.lock().unwrap(),
            ["early", "mid-a", "mid-b", "late"],
            "priority ascending, ties in subscription order"
        );
    }

    #[tokio::test]
    async fn a_rewrite_is_what_the_next_handler_sees() {
        let bus = EventBus::new();
        bus.subscribe(
            &plugin("first"),
            Event::UserPrompt,
            0,
            handler(|_, payload| {
                assert_eq!(payload, json!({"text": "hi"}));
                Ok(Verdict::Rewrite(json!({"text": "hi there"})))
            }),
        );
        bus.subscribe(
            &plugin("second"),
            Event::UserPrompt,
            1,
            handler(|_, payload| {
                // The point of the test: the second handler is handed the
                // first one's output, not the caller's input.
                assert_eq!(payload, json!({"text": "hi there"}));
                Ok(Verdict::Rewrite(json!({"text": "hi there!"})))
            }),
        );

        let dispatch = bus.emit(Event::UserPrompt, json!({"text": "hi"})).await;
        assert_eq!(dispatch.payload, json!({"text": "hi there!"}));
        assert_eq!(dispatch.rewrites, 2);
        assert_eq!(dispatch.ran, 2);
    }

    #[tokio::test]
    async fn a_veto_stops_the_chain_and_names_the_plugin() {
        let bus = EventBus::new();
        let after = Arc::new(AtomicUsize::new(0));
        bus.subscribe(
            &plugin("guard"),
            Event::PreToolUse,
            0,
            handler(|_, _| Ok(Verdict::Veto("rm -rf is not happening".into()))),
        );
        let counter = Arc::clone(&after);
        bus.subscribe(
            &plugin("downstream"),
            Event::PreToolUse,
            1,
            handler(move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Verdict::Continue)
            }),
        );

        let dispatch = bus
            .emit(Event::PreToolUse, json!({"tool": "execute"}))
            .await;
        assert!(dispatch.is_vetoed());
        assert_eq!(dispatch.veto_reason(), Some("rm -rf is not happening"));
        assert_eq!(dispatch.veto.as_ref().unwrap().plugin, "guard");
        assert_eq!(dispatch.ran, 1);
        assert_eq!(
            after.load(Ordering::SeqCst),
            0,
            "nothing after a veto should run"
        );
    }

    #[tokio::test]
    async fn a_handler_that_errors_is_skipped_and_the_rest_still_run() {
        let bus = EventBus::new();
        bus.subscribe(
            &plugin("broken"),
            Event::TurnEnd,
            0,
            handler(|_, _| Err(anyhow::anyhow!("the config file went missing"))),
        );
        bus.subscribe(
            &plugin("healthy"),
            Event::TurnEnd,
            1,
            handler(|_, _| Ok(Verdict::Rewrite(json!("still ran")))),
        );

        let dispatch = bus.emit(Event::TurnEnd, Value::Null).await;
        assert_eq!(dispatch.payload, json!("still ran"));
        assert_eq!(
            dispatch.ran, 1,
            "the broken one did not count as having run"
        );
        assert_eq!(dispatch.failures.len(), 1);
        let failure = &dispatch.failures[0];
        assert_eq!(failure.plugin, "broken");
        assert!(!failure.panicked);
        assert!(failure.message.contains("config file"));
    }

    #[tokio::test]
    async fn a_handler_that_panics_is_skipped_and_the_rest_still_run() {
        let bus = EventBus::new();
        bus.subscribe(
            &plugin("exploding"),
            Event::SessionStart,
            0,
            handler(|_, _| panic!("index out of bounds in a plugin")),
        );
        bus.subscribe(
            &plugin("healthy"),
            Event::SessionStart,
            1,
            handler(|_, _| Ok(Verdict::Continue)),
        );

        // The default hook would print a backtrace per panic and drown the
        // suite's output; the panic still unwinds into catch_unwind.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let dispatch = bus.emit(Event::SessionStart, Value::Null).await;
        std::panic::set_hook(previous);

        assert_eq!(dispatch.ran, 1);
        assert_eq!(dispatch.failures.len(), 1);
        assert!(dispatch.failures[0].panicked);
        assert!(dispatch.failures[0].message.contains("index out of bounds"));
    }

    #[tokio::test]
    async fn a_panic_at_the_await_point_is_caught_too() {
        // The construction of the future is fine and the panic happens when it
        // is polled, which is the shape a dead Lua VM produces.
        let bus = EventBus::new();
        bus.subscribe(
            &plugin("late-panic"),
            Event::Checkpoint,
            0,
            Arc::new(|_event, _payload| async move {
                tokio::task::yield_now().await;
                panic!("the VM went away");
            }),
        );
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let dispatch = bus.emit(Event::Checkpoint, Value::Null).await;
        std::panic::set_hook(previous);
        assert_eq!(dispatch.failures.len(), 1);
        assert!(dispatch.failures[0].panicked);
        assert!(dispatch.failures[0].message.contains("VM went away"));
    }

    #[tokio::test]
    async fn a_handler_only_hears_the_event_it_subscribed_to() {
        let bus = EventBus::new();
        bus.subscribe(
            &plugin("p"),
            Event::SessionEnd,
            0,
            handler(|event, _| {
                assert_eq!(event, Event::SessionEnd);
                Ok(Verdict::Continue)
            }),
        );
        assert_eq!(bus.subscriber_count(Event::SessionEnd), 1);
        assert_eq!(bus.subscriber_count(Event::SessionStart), 0);
        let dispatch = bus.emit(Event::SessionStart, Value::Null).await;
        assert_eq!(dispatch.ran, 0);
    }

    #[tokio::test]
    async fn unsubscribing_detaches_exactly_one_handler() {
        let bus = EventBus::new();
        let id = bus.subscribe(
            &plugin("p"),
            Event::TurnStart,
            0,
            handler(|_, _| Ok(Verdict::Continue)),
        );
        bus.subscribe(
            &plugin("p"),
            Event::TurnStart,
            0,
            handler(|_, _| Ok(Verdict::Continue)),
        );
        assert_eq!(bus.len(), 2);
        assert!(bus.unsubscribe(id));
        assert_eq!(bus.len(), 1);
        assert!(!bus.unsubscribe(id), "a second unsubscribe finds nothing");
        assert_eq!(bus.emit(Event::TurnStart, Value::Null).await.ran, 1);
    }

    #[tokio::test]
    async fn unsubscribing_a_plugin_takes_every_event_it_was_on() {
        let bus = EventBus::new();
        let doomed = plugin("doomed");
        let keeper = plugin("keeper");
        for event in [Event::TurnStart, Event::TurnEnd, Event::Compaction] {
            bus.subscribe(&doomed, event, 0, handler(|_, _| Ok(Verdict::Continue)));
        }
        bus.subscribe(
            &keeper,
            Event::TurnStart,
            0,
            handler(|_, _| Ok(Verdict::Continue)),
        );

        assert_eq!(bus.unsubscribe_plugin(&doomed), 3);
        assert_eq!(bus.len(), 1);
        assert_eq!(bus.subscriber_count(Event::TurnEnd), 0);
        assert_eq!(bus.subscriber_count(Event::TurnStart), 1);
        assert_eq!(
            bus.unsubscribe_plugin(&doomed),
            0,
            "a second sweep finds nothing"
        );
    }

    #[tokio::test]
    async fn a_handler_can_emit_without_deadlocking_the_bus() {
        // The subscription list is copied out before the first handler runs;
        // without that this test hangs rather than fails.
        let bus = EventBus::new();
        let inner = bus.clone();
        bus.subscribe(
            &plugin("chatty"),
            Event::TurnStart,
            0,
            Arc::new(move |_event, _payload| {
                let inner = inner.clone();
                async move {
                    inner.emit(Event::TurnEnd, json!("from a handler")).await;
                    Ok(Verdict::Continue)
                }
            }),
        );
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = Arc::clone(&seen);
        bus.subscribe(
            &plugin("listener"),
            Event::TurnEnd,
            0,
            handler(move |_, payload| {
                sink.lock().unwrap().push(payload);
                Ok(Verdict::Continue)
            }),
        );

        let dispatch = bus.emit(Event::TurnStart, Value::Null).await;
        assert_eq!(dispatch.ran, 1);
        assert_eq!(*seen.lock().unwrap(), [json!("from a handler")]);
    }

    #[tokio::test]
    async fn a_subscription_added_during_an_emit_does_not_run_in_that_emit() {
        // Falls out of copying the list, and is the behaviour to want: a
        // plugin that subscribes from inside a handler must not be able to
        // make its own subscription fire for the event that is already in
        // flight, which would be an unbounded loop written by accident.
        let bus = EventBus::new();
        let inner = bus.clone();
        let ran_late = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ran_late);
        bus.subscribe(
            &plugin("subscriber"),
            Event::ConfigReload,
            0,
            handler(move |_, _| {
                let counter = Arc::clone(&counter);
                inner.subscribe(
                    &plugin("late"),
                    Event::ConfigReload,
                    99,
                    handler(move |_, _| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(Verdict::Continue)
                    }),
                );
                Ok(Verdict::Continue)
            }),
        );

        bus.emit(Event::ConfigReload, Value::Null).await;
        assert_eq!(ran_late.load(Ordering::SeqCst), 0);
        bus.emit(Event::ConfigReload, Value::Null).await;
        assert_eq!(ran_late.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_handler_id_prints_something_a_log_line_can_use() {
        let bus = EventBus::new();
        let id = bus.subscribe(
            &plugin("p"),
            Event::TurnStart,
            0,
            handler(|_, _| Ok(Verdict::Continue)),
        );
        assert!(id.to_string().starts_with("handler#"));
    }
}
