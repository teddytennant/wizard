//! `provide` / `inject`: how one plugin reaches another without naming it at
//! compile time.
//!
//! The whole design rests on one line of `docs/plugins.md`: "`ctx:inject`
//! returning `nil` is the composability rule". A plugin that wants the web tool
//! asks for it and degrades when it is absent, rather than failing to load —
//! which is what makes a `pi` profile a subset of the same tree rather than a
//! build matrix. So the only shape [`ServiceRegistry::inject`] may have is one
//! that returns `Option`, and the only failure mode it may have is `None`.
//!
//! # Two kinds of service, because two kinds of plugin
//!
//! A Rust plugin provides an `Arc<dyn SomeTrait>` and the plugin that injects
//! it downcasts back. A Lua plugin cannot hold either end of that, so it
//! provides a table, which the kernel keeps as JSON. [`Service`] is the
//! two-armed enum rather than two registries because the name space is one
//! name space: `ctx:provide("todo", ...)` from Lua must collide with
//! `ctx.provide("todo", ...)` from Rust, or the composability rule quietly
//! becomes "whichever language you happen to be in".
//!
//! Lua injecting a native service gets `nil`. That is the honest answer — it
//! cannot call the thing — and it is the same answer it gets for a service
//! nobody provided, which means a Lua plugin's degrade path already covers it.
//!
//! # Withdrawal, and why `inject` is not the last word
//!
//! Unloading a plugin has to withdraw its services "from anyone who injected
//! them". [`ServiceRegistry::inject`] hands out an `Arc`, and an `Arc` the
//! holder already has cannot be taken back, so a holder that stashed one keeps
//! a working object pointing at a dead plugin. [`ServiceRef`] is the way to ask
//! for a service that stays honest: it holds the *name* and resolves on every
//! call, so the instant the provider unloads it starts answering `None`.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;

use super::lifecycle::PluginId;

/// Something one plugin exposed for others to take.
#[derive(Clone)]
pub enum Service {
    /// A Rust object, retrieved by downcasting to the concrete type or trait
    /// object the provider published. Invisible to Lua.
    Native(Arc<dyn Any + Send + Sync>),
    /// Plain data, which is what a Lua plugin can provide and what either
    /// language can read.
    Data(Value),
}

impl Service {
    /// Wrap a concrete value. The injector has to name the same type to get it
    /// back, which is the whole of the type discipline here: there is no
    /// registry of type names, so a mismatch is a `None` and not a panic.
    pub fn native<T: Any + Send + Sync>(value: T) -> Self {
        Service::Native(Arc::new(value))
    }

    /// Wrap an already-shared object, for a provider that keeps its own handle
    /// on the thing it published.
    pub fn shared<T: Any + Send + Sync>(value: Arc<T>) -> Self {
        Service::Native(value)
    }

    pub fn data(value: Value) -> Self {
        Service::Data(value)
    }

    /// Recover the concrete type. `None` when this is data, or when the caller
    /// guessed a different type than the provider published.
    pub fn downcast<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        match self {
            Service::Native(any) => Arc::clone(any).downcast::<T>().ok(),
            Service::Data(_) => None,
        }
    }

    /// The JSON behind a data service. `None` for a native one, which is what
    /// Lua sees.
    pub fn as_data(&self) -> Option<&Value> {
        match self {
            Service::Data(value) => Some(value),
            Service::Native(_) => None,
        }
    }

    pub fn is_native(&self) -> bool {
        matches!(self, Service::Native(_))
    }
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `dyn Any` has no Debug and the type id is not a name a reader can
            // use, so this says what it is and stops.
            Service::Native(_) => f.write_str("Service::Native(..)"),
            Service::Data(value) => write!(f, "Service::Data({value})"),
        }
    }
}

/// One entry, and who owns it.
#[derive(Clone)]
struct Entry {
    owner: PluginId,
    service: Service,
}

/// The name-keyed table every plugin provides into and injects out of.
///
/// Cheap to clone; every clone is the same registry.
#[derive(Clone, Default)]
pub struct ServiceRegistry {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish `service` under `name` on behalf of `owner`.
    ///
    /// A name already taken is *replaced* and the previous entry returned,
    /// rather than refused. Two reasons: reload has to be able to put a
    /// plugin's own service back without a withdraw/provide window where
    /// injectors see `None`, and an override is a legitimate thing for a
    /// higher-priority plugin to do. The caller decides whether a replacement
    /// is news; the ledger of the *previous* owner still holds the name, and
    /// [`ServiceRegistry::withdraw_owned`] is what keeps that from taking the
    /// new owner's entry down with it.
    pub fn provide(
        &self,
        owner: &PluginId,
        name: impl Into<String>,
        service: Service,
    ) -> Option<Service> {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.insert(
            name.into(),
            Entry {
                owner: owner.clone(),
                service,
            },
        )
        .map(|entry| entry.service)
    }

    /// Take a service, or `None` if nobody provided it.
    ///
    /// There is no erroring variant on purpose. See the module docs: a plugin
    /// that treats a missing service as a failure has made itself a hard
    /// dependency, and hard dependencies are what the profile system exists to
    /// not have.
    pub fn inject(&self, name: &str) -> Option<Service> {
        let map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(name).map(|entry| entry.service.clone())
    }

    /// [`ServiceRegistry::inject`] plus the downcast, which is the call every
    /// Rust injector actually wants.
    pub fn inject_as<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.inject(name)?.downcast::<T>()
    }

    /// A handle that re-resolves on every use, so it goes `None` the moment
    /// the provider unloads. See the module docs.
    pub fn reference(&self, name: impl Into<String>) -> ServiceRef {
        ServiceRef {
            registry: self.clone(),
            name: name.into(),
        }
    }

    /// Who provided `name`, if anyone.
    pub fn owner(&self, name: &str) -> Option<PluginId> {
        let map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(name).map(|entry| entry.owner.clone())
    }

    /// Remove one name whatever owns it.
    pub fn withdraw(&self, name: &str) -> Option<Service> {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.remove(name).map(|entry| entry.service)
    }

    /// Remove `name` only if `owner` still owns it.
    ///
    /// This is the one an unload uses. Withdrawing by name alone would let a
    /// plugin whose service was overridden by a later one take the *override*
    /// down on its way out, which reads as "unloading A broke B" and is
    /// exactly the kind of reload residue this kernel exists to not have.
    pub fn withdraw_owned(&self, owner: &PluginId, name: &str) -> Option<Service> {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match map.get(name) {
            Some(entry) if &entry.owner == owner => map.remove(name).map(|entry| entry.service),
            _ => None,
        }
    }

    /// Withdraw everything `owner` provided, and say how many went. The sweep
    /// half of the belt-and-braces the bus has: the ledger names them, this
    /// catches any the ledger missed.
    pub fn withdraw_all(&self, owner: &PluginId) -> usize {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let before = map.len();
        map.retain(|_, entry| &entry.owner != owner);
        before - map.len()
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, name: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(name)
    }

    /// Every provided name, sorted. For `/plugin` listings and for tests that
    /// assert on residue.
    pub fn names(&self) -> Vec<String> {
        let map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut names: Vec<String> = map.keys().cloned().collect();
        names.sort();
        names
    }
}

/// A lazily-resolved service, for an injector that outlives its provider.
#[derive(Clone)]
pub struct ServiceRef {
    registry: ServiceRegistry,
    name: String,
}

impl ServiceRef {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The service as it stands right now, or `None` once the provider is gone.
    pub fn get(&self) -> Option<Service> {
        self.registry.inject(&self.name)
    }

    pub fn get_as<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.registry.inject_as::<T>(&self.name)
    }

    /// Whether the service is available right now.
    pub fn is_present(&self) -> bool {
        self.registry.contains(&self.name)
    }
}

impl std::fmt::Debug for ServiceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceRef")
            .field("name", &self.name)
            .field("present", &self.is_present())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn plugin(name: &str) -> PluginId {
        PluginId::new(name)
    }

    /// A trait object service, which is the shape a real Rust plugin provides.
    trait Greeter: Send + Sync {
        fn greet(&self) -> String;
    }

    struct Hello;
    impl Greeter for Hello {
        fn greet(&self) -> String {
            "hello".to_string()
        }
    }

    #[test]
    fn injecting_something_nobody_provided_is_none_and_not_an_error() {
        let registry = ServiceRegistry::new();
        assert!(registry.is_empty());
        assert!(registry.inject("web").is_none());
        assert!(registry.inject_as::<Hello>("web").is_none());
        assert!(registry.owner("web").is_none());
    }

    #[test]
    fn a_native_service_comes_back_through_a_downcast() {
        let registry = ServiceRegistry::new();
        let boxed: Arc<dyn Greeter> = Arc::new(Hello);
        registry.provide(&plugin("greeter"), "greeter", Service::native(boxed));

        let recovered = registry
            .inject_as::<Arc<dyn Greeter>>("greeter")
            .expect("the provider's own type comes back");
        assert_eq!(recovered.greet(), "hello");
        assert_eq!(registry.owner("greeter"), Some(plugin("greeter")));
    }

    #[test]
    fn a_downcast_to_the_wrong_type_is_none_rather_than_a_panic() {
        let registry = ServiceRegistry::new();
        registry.provide(&plugin("p"), "n", Service::native(7_u32));
        assert!(registry.inject_as::<String>("n").is_none());
        assert_eq!(registry.inject_as::<u32>("n").as_deref(), Some(&7));
    }

    #[test]
    fn a_data_service_is_readable_and_a_native_one_is_not() {
        let registry = ServiceRegistry::new();
        registry.provide(&plugin("todo"), "todo", Service::data(json!({"open": 3})));
        registry.provide(&plugin("greeter"), "greeter", Service::native(Hello));

        let data = registry.inject("todo").expect("provided");
        assert_eq!(data.as_data(), Some(&json!({"open": 3})));
        assert!(!data.is_native());
        assert!(data.downcast::<Hello>().is_none());

        // What Lua sees when it injects a native service: nothing usable, the
        // same nothing it sees for an absent one.
        let native = registry.inject("greeter").expect("provided");
        assert!(native.is_native());
        assert!(native.as_data().is_none());
    }

    #[test]
    fn providing_a_taken_name_replaces_it_and_hands_back_the_old_one() {
        let registry = ServiceRegistry::new();
        registry.provide(&plugin("a"), "shared", Service::data(json!(1)));
        let previous = registry
            .provide(&plugin("b"), "shared", Service::data(json!(2)))
            .expect("the first entry comes back");
        assert_eq!(previous.as_data(), Some(&json!(1)));
        assert_eq!(registry.owner("shared"), Some(plugin("b")));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn withdrawing_by_owner_leaves_an_override_alone() {
        let registry = ServiceRegistry::new();
        let first = plugin("a");
        let second = plugin("b");
        registry.provide(&first, "shared", Service::data(json!(1)));
        registry.provide(&second, "shared", Service::data(json!(2)));

        // `a` unloads. Its ledger still says it provided "shared", but `b`
        // owns that name now and must keep it.
        assert!(registry.withdraw_owned(&first, "shared").is_none());
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.inject("shared").unwrap().as_data(),
            Some(&json!(2))
        );

        assert!(registry.withdraw_owned(&second, "shared").is_some());
        assert!(registry.is_empty());
    }

    #[test]
    fn withdrawing_all_of_an_owner_leaves_every_other_owner_alone() {
        let registry = ServiceRegistry::new();
        let doomed = plugin("doomed");
        let keeper = plugin("keeper");
        registry.provide(&doomed, "one", Service::data(json!(1)));
        registry.provide(&doomed, "two", Service::data(json!(2)));
        registry.provide(&keeper, "three", Service::data(json!(3)));

        assert_eq!(registry.withdraw_all(&doomed), 2);
        assert_eq!(registry.names(), ["three"]);
        assert_eq!(
            registry.withdraw_all(&doomed),
            0,
            "a second sweep is a no-op"
        );
    }

    #[test]
    fn withdrawing_by_name_ignores_the_owner() {
        let registry = ServiceRegistry::new();
        registry.provide(&plugin("a"), "n", Service::data(json!(1)));
        assert!(registry.withdraw("n").is_some());
        assert!(registry.withdraw("n").is_none());
    }

    #[test]
    fn a_reference_goes_dark_when_its_provider_unloads() {
        let registry = ServiceRegistry::new();
        let owner = plugin("web");
        let reference = registry.reference("web");
        // Taken before anything provided it: the degrade path is the default.
        assert!(!reference.is_present());
        assert!(reference.get().is_none());

        registry.provide(&owner, "web", Service::native(Hello));
        assert!(reference.is_present());
        assert_eq!(
            reference.get_as::<Hello>().expect("present").greet(),
            "hello"
        );
        assert_eq!(reference.name(), "web");
        assert!(format!("{reference:?}").contains("present: true"));

        registry.withdraw_all(&owner);
        assert!(!reference.is_present());
        assert!(reference.get().is_none());
        assert!(reference.get_as::<Hello>().is_none());
    }

    #[test]
    fn a_shared_arc_can_be_provided_without_being_rewrapped() {
        let registry = ServiceRegistry::new();
        let held = Arc::new(Hello);
        registry.provide(&plugin("p"), "hello", Service::shared(Arc::clone(&held)));
        assert_eq!(
            registry
                .inject_as::<Hello>("hello")
                .expect("present")
                .greet(),
            held.greet()
        );
    }

    #[test]
    fn debug_says_what_a_service_is_without_pretending_to_name_a_type() {
        assert_eq!(
            format!("{:?}", Service::native(Hello)),
            "Service::Native(..)"
        );
        assert_eq!(format!("{:?}", Service::data(json!(1))), "Service::Data(1)");
    }

    #[test]
    fn names_are_sorted_so_a_listing_does_not_move_between_runs() {
        let registry = ServiceRegistry::new();
        let owner = plugin("p");
        for name in ["zeta", "alpha", "mu"] {
            registry.provide(&owner, name, Service::data(Value::Null));
        }
        assert_eq!(registry.names(), ["alpha", "mu", "zeta"]);
        assert!(registry.contains("mu"));
        assert!(!registry.contains("nu"));
    }
}
