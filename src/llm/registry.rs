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
//   - Onboarding's menu, the TUI's add-provider picker and the settings
//     sheet's presets each name backends in their rows. They are no longer
//     hand-written *lists*: every one of them is filtered by `kinds()`, so a
//     row is offered only where a plugin registered the kind behind it. What
//     is left is the label and the sentence beside it, which answer "which of
//     these should you pick" rather than "what is this backend called" — a
//     question no descriptor is asked.
//   - `tools/image.rs` picks an image endpoint per backend. That belongs on a
//     capability the descriptor does not have yet; see the module docs there.
//   - `plugins/web.rs` and `tools/image.rs` reach xAI's search and image APIs,
//     which are not chat and so are not behind a `kind` at all.
//
// Every provider is now a plugin, so each of these constants names a kind that
// a build may not answer to. That is a supported state and not a bug: every
// use is already guarded by a registry lookup that returns `None` when the
// plugin is absent, and `ProviderKind::ANTHROPIC`'s doc below has the full
// argument for why holding the *string* is allowed where holding the type is
// not.
impl ProviderKind {
    /// Local llama.cpp `llama-server`. The default local backend, and the one
    /// `Config::active` synthesizes when nothing is configured — so on a build
    /// without `provider-llamacpp` that synthesized entry resolves to nothing
    /// and says so, which is why the feature is on by default.
    pub const LLAMACPP: Self = Self::known("llamacpp");
    /// Local Ollama server (native `/api/chat`).
    pub const OLLAMA: Self = Self::known("ollama");
    /// Any OpenAI-compatible Chat Completions endpoint. Also how vLLM, LM
    /// Studio, DeepSeek and every `compat.rs` preset are reached.
    pub const OPENAI: Self = Self::known("openai");
    /// Anthropic Messages API. Registered by [`crate::plugins::anthropic`],
    /// so on a build without `provider-anthropic` this constant names a kind
    /// nothing answers to — which is a supported state, not a bug, and is now
    /// true of every constant in this block. A `kind` is a string a user
    /// writes in a file; core may hold the string (to offer it in a menu, to
    /// compare against one that was typed) as long as it never names the type
    /// behind it or constructs one. Every use is guarded by a registry lookup
    /// that returns `None` when the plugin is absent.
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

/// The on-disk strings a stock build's backends default to.
///
/// Same boundary as the [`ProviderKind`] constants above, and the same
/// argument. A default base URL, the env var a key is looked up in and the
/// model tag written into a fresh `config.toml` are all *text a user would
/// otherwise type*, and core is allowed to hold text: to prefill a form with
/// it, to offer it in a menu, to compare against something somebody typed. It
/// is naming the *type* behind a kind, or constructing one, that core may not
/// do, and nothing here does either.
///
/// This module exists because the alternative was `#[cfg(feature = "...")]`
/// inside onboarding's numbered menu, the TUI provider picker and the settings
/// sheet's preset table — the hand-written-menu problem this file flagged from
/// the registry landing until those three menus were filtered by [`kinds`].
/// They are now, so a stripped build no longer offers a backend it does not
/// have; the strings here are what a row that *is* offered prefills its form
/// with, and they are still text rather than types.
///
/// Only backends core actually spells out are here. A provider plugin whose
/// defaults nothing outside it reads keeps them to itself; llama.cpp, Ollama
/// and the OpenAI kind have no entries for exactly that reason.
pub mod defaults {
    /// OpenRouter's Chat Completions base URL.
    pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
    /// OpenRouter's Auto Router, which picks a model per prompt.
    pub const OPENROUTER_MODEL: &str = "openrouter/auto";
    /// Env var holding an OpenRouter API key.
    pub const OPENROUTER_KEY_ENV: &str = "OPENROUTER_API_KEY";

    /// Workers AI's OpenAI-compatible base URL, with the account id left as
    /// [`CLOUDFLARE_ACCOUNT_ID_PLACEHOLDER`] for onboarding to fill in.
    pub const CLOUDFLARE_BASE_URL_TEMPLATE: &str =
        "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1";
    /// The token in [`CLOUDFLARE_BASE_URL_TEMPLATE`] a configured account id
    /// replaces. Written out rather than formatted so the unsubstituted
    /// template can be shown in a form and detected when it comes back
    /// unedited.
    pub const CLOUDFLARE_ACCOUNT_ID_PLACEHOLDER: &str = "{account_id}";
    /// Default Workers AI model: GLM 5.2 (Z.ai), the most capable text model
    /// in the catalog.
    pub const CLOUDFLARE_MODEL: &str = "@cf/zai-org/glm-5.2";
    /// Env var holding a Workers AI API token.
    pub const CLOUDFLARE_KEY_ENV: &str = "CLOUDFLARE_API_TOKEN";

    /// The Workers AI base URL for one account.
    pub fn cloudflare_base_url(account_id: &str) -> String {
        CLOUDFLARE_BASE_URL_TEMPLATE.replace(CLOUDFLARE_ACCOUNT_ID_PLACEHOLDER, account_id.trim())
    }
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
/// A provider registers one of these through `Ctx::provider`, and every site
/// that used to match on a `ProviderKind` variant reads a field off it
/// instead. There is no other way in: the table of concrete provider types
/// that used to seed this registry (`llm::builtin`) is gone, and what a build
/// answers to is exactly the set of plugins it was compiled with.
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
/// Starts **empty**, which is the whole of the migration in one line. It used
/// to be seeded from `llm::builtin::registry()`, a table naming eight concrete
/// provider types and constructing a descriptor from each; every one of them
/// is now a plugin that calls [`install`] for itself at kernel boot. A build
/// with no provider features linked therefore has no kinds, and says so.
static INSTALLED: LazyLock<RwLock<ProviderRegistry>> =
    LazyLock::new(|| RwLock::new(ProviderRegistry::new()));

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

/// Bring the compiled-in plugins up, so a provider one of them registers is
/// visible here.
///
/// Every read below goes through this and no write does, which is the whole
/// of the re-entrancy rule: `ensure` loads plugins, a loading plugin calls
/// [`install`], and if `install` ensured too the process would be waiting on
/// its own `OnceLock`.
///
/// It is on the read side rather than at startup because `ProviderConfig::build`
/// is reachable from twenty places, several of which run before any `main` —
/// unit tests, `wizard doctor`, the settings sheet's probe. "Eagerly at
/// startup" and "on the first lookup" are the same thing when the first lookup
/// is the first thing startup does, and only the second one is true in a test
/// binary. See [`crate::plugins`].
fn ensure() {
    crate::plugins::ensure_providers();
}

/// The descriptor for `kind`, or `None` when nothing is registered under it.
///
/// Cloned out rather than handed back behind the lock: a descriptor holds
/// `Arc`s, so the clone is cheap, and a caller that held a guard across
/// `build` would be holding a read lock across a `llama-server` spawn.
pub fn installed(kind: &ProviderKind) -> Option<ProviderDescriptor> {
    ensure();
    read().get(kind).cloned()
}

/// Every installed kind, sorted.
pub fn kinds() -> Vec<ProviderKind> {
    ensure();
    read().kinds()
}

/// Register a descriptor process-wide, or refuse because its kind is taken.
///
/// Ensures first, so the refusal is decided against the full set: a plugin
/// loaded into some other kernel — a test's, a `/plugin load` — must not be
/// able to take `anthropic` merely because nothing had looked a kind up yet in
/// this process. [`crate::plugins::ensure_providers`] short-circuits when the
/// caller *is* the plugin boot, which is the only way this could re-enter.
pub fn install(descriptor: ProviderDescriptor) -> Result<(), KindTaken> {
    ensure();
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
        // The valid list is generated from what is installed rather than
        // typed out, so on a build with no provider plugins at all it is
        // empty and this assertion has nothing to check. That is the point of
        // generating it.
        #[cfg(feature = "provider-openai")]
        assert!(message.contains("openai"), "{message}");
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
