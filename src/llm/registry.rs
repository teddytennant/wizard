//! The provider registry: what `kind = "..."` in `config.toml` actually means.
//!
//! `ProviderKind` used to be a nine-variant enum, matched exhaustively in
//! twenty-three files, with one `match` in `config.rs` that imported all nine
//! concrete provider types and constructed them. That shape has a hard
//! consequence: a provider can only exist if core names it, so no provider can
//! ever be a plugin. `docs/plugins.md` requires the opposite — "no core module
//! names a plugin" — and a closed enum is the single largest thing standing in
//! the way.
//!
//! So the enum became a string id plus a lookup. A kind is now
//! [`ProviderKind`], a newtype over the exact string that was already on disk,
//! and everything the old `match` arms answered — the display name, the
//! default key env var, whether a key is needed at all, how to build the
//! client, how to get the backend ready — is a field on a
//! [`ProviderDescriptor`] that the provider registers for itself.
//!
//! # Why the id is the on-disk string and nothing else
//!
//! `kind = "xaioauth"` is in every `config.toml` in the wild. The registry key
//! is that string, byte for byte, so the serialized form is unchanged and a
//! config written by an older build loads into a newer one and back out
//! identically. There is no id-to-string table anywhere, because a table is a
//! place the two can drift.
//!
//! # Why an unknown kind no longer fails to parse
//!
//! The old enum rejected `kind = "banana"` at deserialization. A registry
//! cannot, and should not: a provider that lives in a plugin is absent from a
//! profile that leaves the plugin out, and the config naming it must still
//! load so the user can `/provider use` something else. That is the same
//! degrade-when-missing rule `Ctx::inject` follows. Parsing therefore accepts
//! any non-empty id and the error moves to the point of use — [`unknown`],
//! raised by `ProviderConfig::build` and by the two places that validate a
//! kind typed by a human. The message got better in the move: it lists the
//! kinds actually installed rather than a hand-maintained literal.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use anyhow::Result;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::provider::LlmProvider;
use crate::config::ProviderConfig;

/// Which backend a [`ProviderConfig`] talks to, as the stable string that is
/// written to `config.toml`.
///
/// `Cow` rather than `String` so the shipped kinds are compile-time constants
/// with no allocation, and rather than `&'static str` so a plugin can register
/// an id it computed. It is deliberately *not* `Copy`, which the old enum was:
/// nothing here can be, and the compiler pointing at every site that assumed
/// otherwise is how the migration was made total.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderKind(Cow<'static, str>);

impl ProviderKind {
    /// A kind whose id is known at compile time.
    pub const fn known(id: &'static str) -> Self {
        Self(Cow::Borrowed(id))
    }

    /// A kind whose id was computed — read from a config file, typed at a
    /// prompt, or supplied by a plugin.
    pub fn new(id: impl Into<String>) -> Self {
        Self(Cow::Owned(id.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/* ---------------------------------------------------------------------- */
/* The ids the shipped providers register under                           */
/* ---------------------------------------------------------------------- */

// Every constant below is a place core still knows the name of one specific
// backend, and therefore a place this migration is not finished. They are
// gathered here rather than left as literals at each site so that
// `grep -r 'ProviderKind::'` is an exact to-do list rather than a search
// through 198k lines. What remains, after the descriptor absorbed the display
// names, the key policy and the readiness hooks:
//
//   - `Config::active` synthesizes a llama.cpp provider when nothing is
//     configured, so core has to know a local default exists.
//   - `WIZARD_GGUF_PATH` writes through to the active provider only when that
//     provider is llama.cpp.
//   - Onboarding and the TUI's provider picker offer a hand-written menu of
//     backends. A picker built from `installed()` is the obvious next step and
//     is not this change.
//   - `tools/image.rs` picks an image endpoint per backend. That belongs on a
//     capability the descriptor does not have yet; see the module docs there.
impl ProviderKind {
    /// Local llama.cpp `llama-server`. The default local backend.
    pub const LLAMACPP: Self = Self::known("llamacpp");
    /// Local Ollama server (native `/api/chat`).
    pub const OLLAMA: Self = Self::known("ollama");
    /// Any OpenAI-compatible Chat Completions endpoint.
    pub const OPENAI: Self = Self::known("openai");
    /// Anthropic Messages API.
    pub const ANTHROPIC: Self = Self::known("anthropic");
    /// OpenRouter, with a plain API key.
    pub const OPENROUTER: Self = Self::known("openrouter");
    /// xAI (Grok), with a plain API key.
    pub const XAI: Self = Self::known("xai");
    /// xAI via account sign-in (`wizard --login xai`).
    pub const XAI_OAUTH: Self = Self::known("xaioauth");
    /// ChatGPT subscription via account sign-in (`wizard --login chatgpt`).
    pub const CHATGPT_OAUTH: Self = Self::known("chatgptoauth");
    /// Cloudflare Workers AI.
    pub const CLOUDFLARE: Self = Self::known("cloudflare");
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Quoted, so a kind nested in a `{:?}` of `ProviderConfig` still reads
        // as the string it is on disk rather than as a bare word that looks
        // like the old variant name.
        f.debug_tuple("ProviderKind").field(&self.0).finish()
    }
}

impl Serialize for ProviderKind {
    /// A bare string, which is exactly what `#[serde(rename_all =
    /// "lowercase")]` on the old enum produced. Changing this changes every
    /// `config.toml` on disk.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProviderKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KindVisitor;

        impl Visitor<'_> for KindVisitor {
            type Value = ProviderKind;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a provider kind, e.g. \"llamacpp\" or \"anthropic\"")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                // An empty id is the one string that can never name anything,
                // and the only case the old enum rejected that is still worth
                // rejecting here: `kind = ""` is a broken file, not an absent
                // plugin, so it keeps failing where it always failed.
                if value.is_empty() {
                    return Err(E::custom("provider kind is empty"));
                }
                Ok(ProviderKind::new(value))
            }
        }

        deserializer.deserialize_str(KindVisitor)
    }
}

/* ---------------------------------------------------------------------- */
/* Descriptors                                                            */
/* ---------------------------------------------------------------------- */

/// How a backend proves who is asking.
///
/// One field replacing four separate exhaustive matches that had each drifted
/// into their own shape: `usage::self_hosted` (is this billed?),
/// `app::session::is_local_kind` (does this run here?), `gui::settings`'s
/// `default_key_env` and `key_source` (where does the key come from?), and
/// onboarding's closing "next steps" advice. All four were asking this one
/// question, and all four had to be edited in lockstep for a new backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credentials {
    /// Runs on this machine. No key, no account, and the tokens cost
    /// electricity rather than money.
    Local,
    /// A bearer key, resolved from the configured env var, then
    /// `default_env`, then `credentials.toml` — see
    /// [`ProviderConfig::api_key`].
    ApiKey {
        /// The env var consulted when the config names none. `None` for
        /// backends where there is no conventional variable to guess.
        default_env: Option<String>,
    },
    /// An account sign-in, with tokens in a file under `~/.wizard`.
    Account {
        /// The argument to `wizard --login`, which is what the user is told
        /// to run when the token store is empty.
        login: String,
    },
}

impl Credentials {
    /// True for a backend that runs on this machine, so its tokens are not
    /// billed and `/cost` reports them as free.
    pub fn is_local(&self) -> bool {
        matches!(self, Credentials::Local)
    }

    /// The env var this backend falls back to when the config names none.
    pub fn default_env(&self) -> Option<&str> {
        match self {
            Credentials::ApiKey { default_env } => default_env.as_deref(),
            Credentials::Local | Credentials::Account { .. } => None,
        }
    }
}

/// Construct the client for one configured provider.
pub type BuildFn = Arc<dyn Fn(&ProviderConfig) -> Result<Arc<dyn LlmProvider>> + Send + Sync>;

/// Get the backend ready to answer, reporting progress as it goes.
///
/// Owned arguments rather than references because the future has to be
/// `'static` to be boxed, and this runs once per startup — the clone is
/// nothing against the process spawn it is about to wait on. The second
/// argument is the *effective* model, which is not always
/// `ProviderConfig::model`: an agent built with a `/model` override has to
/// pull the tag it will actually ask for.
pub type PrepareFn = Arc<
    dyn Fn(ProviderConfig, String) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

/// Everything core needs to know about one backend without naming its type.
///
/// A provider registers one of these — from `llm::builtin` today, from
/// `Ctx::provider` once providers are plugins — and every site that used to
/// match on a `ProviderKind` variant reads a field off it instead.
#[derive(Clone)]
pub struct ProviderDescriptor {
    kind: ProviderKind,
    display_name: String,
    credentials: Credentials,
    local_server: bool,
    build: BuildFn,
    prepare: Option<PrepareFn>,
}

impl ProviderDescriptor {
    /// The four things every backend must declare. Everything else is opt-in.
    pub fn new<F>(
        kind: ProviderKind,
        display_name: impl Into<String>,
        credentials: Credentials,
        build: F,
    ) -> Self
    where
        F: Fn(&ProviderConfig) -> Result<Arc<dyn LlmProvider>> + Send + Sync + 'static,
    {
        Self {
            kind,
            display_name: display_name.into(),
            credentials,
            local_server: false,
            build: Arc::new(build),
            prepare: None,
        }
    }

    /// Declare a readiness step run once, before the first health probe.
    ///
    /// This is what lets the agent's startup path stop knowing that llama.cpp
    /// needs a server spawned and Ollama needs a model pulled. Both were
    /// open-coded, identically, in two places.
    pub fn with_prepare<F, Fut>(mut self, prepare: F) -> Self
    where
        F: Fn(ProviderConfig, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.prepare = Some(Arc::new(move |config, model| {
            Box::pin(prepare(config, model))
        }));
        self
    }

    /// Declare that `/server` manages this backend's process.
    ///
    /// Narrower than [`Credentials::Local`] and deliberately a separate flag:
    /// Ollama also runs on this machine, but Wizard neither spawns nor stops
    /// it, so `/server status` against an Ollama provider has to keep saying
    /// the command does not apply.
    pub fn with_local_server(mut self) -> Self {
        self.local_server = true;
        self
    }

    pub fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    /// The name a human reads: "xAI", "llama.cpp", "OpenAI-compatible".
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    /// Whether `/server` manages this backend's process.
    pub fn manages_local_server(&self) -> bool {
        self.local_server
    }

    /// Build the client for `config`.
    pub fn build(&self, config: &ProviderConfig) -> Result<Arc<dyn LlmProvider>> {
        (self.build)(config)
    }

    /// Run the readiness step, if this backend has one.
    pub async fn prepare(&self, config: &ProviderConfig, model: &str) -> Result<()> {
        match &self.prepare {
            Some(prepare) => prepare(config.clone(), model.to_string()).await,
            None => Ok(()),
        }
    }
}

impl fmt::Debug for ProviderDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderDescriptor")
            .field("kind", &self.kind)
            .field("display_name", &self.display_name)
            .field("credentials", &self.credentials)
            .field("local_server", &self.local_server)
            .finish_non_exhaustive()
    }
}

/* ---------------------------------------------------------------------- */
/* The registry                                                           */
/* ---------------------------------------------------------------------- */

/// A set of descriptors keyed by kind.
///
/// `BTreeMap` because the only thing ever done with the whole set is to list
/// it for a human — an error message naming the installed kinds, a picker —
/// and a list that reorders between runs reads as a bug.
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    by_kind: BTreeMap<ProviderKind, ProviderDescriptor>,
}

/// Refusing a kind somebody else already holds.
///
/// The same rule the kernel applies to tool and command names, for the same
/// reason: a kind is named by a user in a config file, and two plugins quietly
/// answering to one name is a bug report filed against the wrong one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindTaken {
    pub kind: ProviderKind,
}

impl fmt::Display for KindTaken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "provider kind '{}' is already registered", self.kind)
    }
}

impl std::error::Error for KindTaken {}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a descriptor, or refuse because its kind is taken.
    pub fn insert(&mut self, descriptor: ProviderDescriptor) -> Result<(), KindTaken> {
        let kind = descriptor.kind.clone();
        if self.by_kind.contains_key(&kind) {
            return Err(KindTaken { kind });
        }
        self.by_kind.insert(kind, descriptor);
        Ok(())
    }

    pub fn get(&self, kind: &ProviderKind) -> Option<&ProviderDescriptor> {
        self.by_kind.get(kind)
    }

    pub fn remove(&mut self, kind: &ProviderKind) -> Option<ProviderDescriptor> {
        self.by_kind.remove(kind)
    }

    /// Every installed kind, sorted.
    pub fn kinds(&self) -> Vec<ProviderKind> {
        self.by_kind.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.by_kind.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_kind.is_empty()
    }
}

/// The process-wide registry every `kind = "..."` resolves against.
///
/// A global rather than a value threaded through the tree because
/// `ProviderConfig::build` is called from twenty places, several of them deep
/// inside code that has no business holding a registry handle, and because
/// there is exactly one set of installed providers per process by
/// construction. The kernel does not own it for the same reason: a `Kernel` is
/// instantiable more than once — every kernel test makes its own — while the
/// set of kinds a config file can name is a property of the process.
/// `Ctx::provider` writes here and into the kernel's own slot in one step, so
/// an unload withdraws the kind from both.
static INSTALLED: LazyLock<RwLock<ProviderRegistry>> =
    LazyLock::new(|| RwLock::new(super::builtin::registry()));

fn read() -> RwLockReadGuard<'static, ProviderRegistry> {
    INSTALLED
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write() -> RwLockWriteGuard<'static, ProviderRegistry> {
    INSTALLED
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The descriptor for `kind`, or `None` when nothing is registered under it.
///
/// Cloned out rather than handed back behind the lock: a descriptor holds
/// `Arc`s, so the clone is cheap, and a caller that held a guard across
/// `build` would be holding a read lock across a `llama-server` spawn.
pub fn installed(kind: &ProviderKind) -> Option<ProviderDescriptor> {
    read().get(kind).cloned()
}

/// Every installed kind, sorted.
pub fn kinds() -> Vec<ProviderKind> {
    read().kinds()
}

/// Register a descriptor process-wide, or refuse because its kind is taken.
pub fn install(descriptor: ProviderDescriptor) -> Result<(), KindTaken> {
    write().insert(descriptor)
}

/// Withdraw a kind, e.g. because the plugin that registered it unloaded.
pub fn uninstall(kind: &ProviderKind) -> bool {
    write().remove(kind).is_some()
}

/// The error every site raises for a kind nothing is registered under.
///
/// One function so the message is identical whether the kind came from a
/// config file, a `/provider add`, or a settings form, and so the list of
/// valid kinds is generated from what is installed rather than typed out and
/// left to rot.
pub fn unknown(kind: &ProviderKind) -> anyhow::Error {
    let known = kinds()
        .iter()
        .map(ProviderKind::to_string)
        .collect::<Vec<_>>()
        .join("|");
    anyhow::anyhow!("unknown provider kind '{kind}' ({known})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(id: &'static str) -> ProviderDescriptor {
        ProviderDescriptor::new(ProviderKind::known(id), id, Credentials::Local, |_| {
            Err(anyhow::anyhow!("test descriptor builds nothing"))
        })
    }

    #[test]
    fn a_kind_serializes_as_the_bare_string_it_is_on_disk() {
        #[derive(Serialize, Deserialize)]
        struct Probe {
            kind: ProviderKind,
        }

        for id in [
            "llamacpp",
            "ollama",
            "openai",
            "anthropic",
            "openrouter",
            "xai",
            "xaioauth",
            "chatgptoauth",
            "cloudflare",
        ] {
            let raw = format!("kind = \"{id}\"\n");
            let probe: Probe = toml::from_str(&raw).expect("parse");
            assert_eq!(probe.kind.as_str(), id);
            assert_eq!(toml::to_string(&probe).expect("serialize"), raw);
        }
    }

    /// The deliberate departure from the old enum, pinned so it is a decision
    /// rather than an accident: a kind nothing has registered parses, and the
    /// complaint arrives at the point of use.
    #[test]
    fn an_unregistered_kind_parses_and_fails_later() {
        #[derive(Deserialize)]
        struct Probe {
            kind: ProviderKind,
        }

        let probe: Probe = toml::from_str("kind = \"a-plugin-that-is-not-loaded\"").expect("parse");
        assert!(installed(&probe.kind).is_none());
        let message = unknown(&probe.kind).to_string();
        assert!(message.contains("a-plugin-that-is-not-loaded"), "{message}");
        // The valid list is generated, so it names the builtins.
        assert!(message.contains("anthropic"), "{message}");
    }

    #[test]
    fn an_empty_kind_is_still_refused_at_parse() {
        #[derive(Deserialize)]
        struct Probe {
            #[allow(dead_code)]
            kind: ProviderKind,
        }

        assert!(toml::from_str::<Probe>("kind = \"\"").is_err());
    }

    #[test]
    fn a_registry_refuses_a_kind_it_already_holds() {
        let mut registry = ProviderRegistry::new();
        registry.insert(descriptor("a")).expect("first");
        registry.insert(descriptor("b")).expect("second");
        let taken = registry.insert(descriptor("a")).expect_err("duplicate");
        assert_eq!(taken.kind, ProviderKind::known("a"));
        assert_eq!(
            registry.kinds(),
            vec![ProviderKind::known("a"), ProviderKind::known("b")]
        );
        assert!(registry.remove(&ProviderKind::known("a")).is_some());
        assert!(registry.get(&ProviderKind::known("a")).is_none());
    }

    #[test]
    fn credentials_answer_the_four_questions_that_used_to_be_four_matches() {
        assert!(Credentials::Local.is_local());
        assert_eq!(Credentials::Local.default_env(), None);

        let keyed = Credentials::ApiKey {
            default_env: Some("SOME_KEY".to_string()),
        };
        assert!(!keyed.is_local());
        assert_eq!(keyed.default_env(), Some("SOME_KEY"));

        let account = Credentials::Account {
            login: "xai".to_string(),
        };
        assert!(!account.is_local());
        assert_eq!(account.default_env(), None);
    }
}
