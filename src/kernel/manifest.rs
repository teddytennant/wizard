//! What a plugin declares about itself, and what that declaration buys it.
//!
//! A manifest is the only thing the kernel knows about a plugin before it runs
//! a line of its code, so it is where the trust decision has to be made. The
//! two questions it answers are deliberately separate, for the same reason
//! [`crate::tools::lua::Stdlib`] and [`crate::tools::lua::Bounds`] are separate
//! there: *what may this reach* is about whose code it is, and *what may this
//! take* is about whether anybody read it.
//!
//! [`Capability`] answers the first. It is a superset of
//! [`crate::registry_client::Capability`], which today gates registry-installed
//! scripted tools with two names — `filesystem` and `process`. Those two keep
//! their exact meaning here; `network`, `model`, `ui` and `agent` are new, and
//! the two worth arguing about are `network` and `model`, because they are the
//! ones that leak data and spend the user's money.
//!
//! [`PluginSource`] answers the second, and the answer is the one
//! `docs/plugins.md` records from the async spike: a first-party plugin in a
//! profile runs unbounded and keeps the JIT, a registry plugin runs bounded and
//! loses it, because `jit.off()` is what makes the instruction hook fire at all.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::tools::lua::Stdlib;

/// One grant a plugin can hold.
///
/// The set is closed on purpose. A capability is a promise to the user about
/// what a plugin cannot do, and a capability that means "and some other things"
/// is not a promise. Adding a name here is a decision about the grant prompt,
/// not a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    /// `io.open`, `os.remove`, `os.rename`, and `wizard.fs.*` unconfined.
    ///
    /// Without it the host file helpers stay pinned to the project directory
    /// (see [`crate::tools::lua::Stdlib::Sandboxed`]), which is the same
    /// confinement a registry-installed scripted tool has had since the
    /// sandbox landed.
    Filesystem,
    /// `os.execute`, `io.popen`, `os.getenv`, `wizard.process.*`.
    ///
    /// `os.getenv` rides along with the shell rather than getting a name of
    /// its own because this process's environment holds every provider API
    /// key: a plugin that can read it can spend the user's money without ever
    /// touching `model`.
    Process,
    /// `wizard.http.*`, subject to the same `[web]` allowlist the native web
    /// tools already apply.
    Network,
    /// `wizard.model.*` — inference billed to the user's account, attributed
    /// to this plugin.
    Model,
    /// `wizard.ui.*` — write to the transcript, open a picker.
    Ui,
    /// `wizard.agent.spawn` — start subagents, which is `model` plus a loop
    /// and therefore never granted implicitly by it.
    Agent,
}

impl Capability {
    /// Every capability, in declaration order. Used by the grant prompt and by
    /// tests that must not silently miss a new name.
    pub const ALL: [Capability; 6] = [
        Capability::Filesystem,
        Capability::Process,
        Capability::Network,
        Capability::Model,
        Capability::Ui,
        Capability::Agent,
    ];

    /// The name used in `manifest.toml` and in the grant prompt.
    pub fn name(self) -> &'static str {
        match self {
            Capability::Filesystem => "filesystem",
            Capability::Process => "process",
            Capability::Network => "network",
            Capability::Model => "model",
            Capability::Ui => "ui",
            Capability::Agent => "agent",
        }
    }

    /// Parse a manifest spelling. Unknown names are rejected rather than
    /// ignored: a typo'd capability that silently grants nothing produces a
    /// plugin that fails deep inside a host call instead of at load.
    pub fn parse(raw: &str) -> Option<Capability> {
        Capability::ALL
            .into_iter()
            .find(|cap| cap.name() == raw.trim())
    }

    /// One line naming what the grant actually is, phrased for a human who is
    /// deciding whether to give it. Mirrors
    /// [`crate::registry_client::Capability::describe`] for the two names the
    /// two enums share, so the prompt reads the same wherever it comes from.
    pub fn describe(self) -> &'static str {
        match self {
            Capability::Filesystem => "read and write any file you can (io.open, os.remove)",
            Capability::Process => {
                "run commands with your privileges (os.execute, io.popen, os.getenv)"
            }
            Capability::Network => "make network requests (wizard.http)",
            Capability::Model => "spend tokens on your account (wizard.model)",
            Capability::Ui => "write to your transcript and open pickers (wizard.ui)",
            Capability::Agent => "start subagents that do all of the above (wizard.agent.spawn)",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The grants one plugin holds, as a set.
///
/// A `BTreeSet` rather than a bitflag so `Debug` output and the grant prompt
/// are ordered and stable — a prompt whose lines move between runs is a prompt
/// people stop reading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    /// The default a plugin gets when its manifest declares nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// Every capability. Only for first-party plugins compiled into the binary
    /// and for tests; nothing reachable from the registry may call it.
    pub fn all() -> Self {
        Self(Capability::ALL.into_iter().collect())
    }

    pub fn contains(&self, cap: Capability) -> bool {
        self.0.contains(&cap)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.0.iter().copied()
    }

    pub fn insert(&mut self, cap: Capability) -> bool {
        self.0.insert(cap)
    }

    /// Which standard library a plugin holding this set runs under.
    ///
    /// The existing profile is binary — [`Stdlib::Sandboxed`] leaves `os` and
    /// `io` out of the state entirely — so either of the two capabilities that
    /// name functions *inside* those tables has to open both. That is coarser
    /// than the table in `docs/plugins.md` reads, and the gap is closed one
    /// level up rather than here: `filesystem` alone still gets `os.execute`,
    /// `io.popen` and `os.getenv` blanked, and `process` alone still gets
    /// `io.open`, `os.remove` and `os.rename` blanked (see
    /// [`crate::kernel::lua::host::narrow_stdlib`]). Splitting `StdLib` itself
    /// would mean a second allowlist to keep in sync with
    /// [`crate::tools::lua::sandboxed_libs`], which is the one set whose
    /// accidental widening is a supply-chain hole.
    pub fn stdlib(&self) -> Stdlib {
        if self.contains(Capability::Filesystem) || self.contains(Capability::Process) {
            Stdlib::Full
        } else {
            Stdlib::Sandboxed
        }
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl fmt::Display for CapabilitySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("none");
        }
        let names: Vec<&str> = self.iter().map(Capability::name).collect();
        f.write_str(&names.join(", "))
    }
}

/// Where a plugin came from, which is the whole of the bound decision.
///
/// Not derived from the capability set: a plugin that declares nothing is not
/// thereby trustworthy, and a first-party plugin that declares everything is
/// still first-party. See `docs/plugins.md` — "a bound costs the JIT", so this
/// is also a performance decision and it has to be made deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PluginSource {
    /// Shipped with Wizard, in a profile the user chose. Unbounded: no
    /// instruction hook, no memory ceiling, JIT on.
    FirstParty,
    /// Installed from the registry or dropped in by hand. Bounded in time and
    /// memory for its whole lifetime, and interpreted rather than compiled.
    #[default]
    Registry,
}

/// A plugin's `manifest.toml`, and the in-memory declaration a Rust plugin
/// returns from [`crate::kernel::Plugin::manifest`].
///
/// Deserialized with `deny_unknown_fields` so a key that is a typo of a real
/// one fails at load instead of being quietly dropped — the failure mode this
/// exists to prevent is a `capabilties = ["network"]` that reads as a plugin
/// declaring nothing and then dying halfway through its first fetch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// Install name, and the key every registration is recorded against.
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Grants, by the spellings in [`Capability::name`].
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Services this plugin will `inject` but can run without. Advisory: the
    /// kernel does not order loads by it, because `inject` returning `nil` is
    /// the composability rule and a plugin that cannot survive a missing
    /// dependency should declare it required by failing in `apply`.
    #[serde(default)]
    pub optional_deps: Vec<String>,
    /// Profiles this plugin belongs to (`full`, `server`, `minimal`, `pi`).
    #[serde(default)]
    pub profiles: Vec<String>,
}

impl PluginManifest {
    /// A manifest with nothing but a name, for a Rust plugin that declares its
    /// grants in code and for tests.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: "0.0.0".to_string(),
            description: String::new(),
            capabilities: Vec::new(),
            optional_deps: Vec::new(),
            profiles: Vec::new(),
        }
    }

    /// Builder sugar, used by the in-tree plugins and by tests.
    pub fn with_capabilities(mut self, caps: impl IntoIterator<Item = Capability>) -> Self {
        self.capabilities = caps.into_iter().collect();
        self
    }

    pub fn capability_set(&self) -> CapabilitySet {
        self.capabilities.iter().copied().collect()
    }

    /// Parse and validate one `manifest.toml`.
    pub fn parse(raw: &str) -> Result<Self, ManifestError> {
        let manifest: PluginManifest =
            toml::from_str(raw).map_err(|err| ManifestError::Syntax(err.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Reject names that cannot be used as a key.
    ///
    /// The charset is narrow because a plugin name ends up in a tool name, a
    /// slash command, a config table key and a directory path, and the
    /// intersection of what those four accept is smaller than any one of them.
    /// Rejecting at load beats discovering it when `/plugin unload` cannot
    /// find the thing it just loaded.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.name.is_empty() {
            return Err(ManifestError::Name("a plugin name may not be empty".into()));
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ManifestError::Name(format!(
                "'{}' is not a usable plugin name: only ASCII letters, digits, '_' and '-'",
                self.name
            )));
        }
        if self.version.is_empty() {
            return Err(ManifestError::Version(self.name.clone()));
        }
        Ok(())
    }
}

/// Why a manifest was refused. Distinct variants because the three failures
/// have different fixes and a single "bad manifest" string sends the reader to
/// re-read the whole file.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("could not parse manifest.toml: {0}")]
    Syntax(String),
    #[error("{0}")]
    Name(String),
    #[error("plugin '{0}' has no version")]
    Version(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_round_trips_through_its_name() {
        for cap in Capability::ALL {
            assert_eq!(Capability::parse(cap.name()), Some(cap), "{cap}");
            assert!(!cap.describe().is_empty(), "{cap} has no description");
        }
    }

    #[test]
    fn an_unknown_capability_name_is_refused_rather_than_ignored() {
        assert_eq!(Capability::parse("filesytem"), None);
        assert_eq!(Capability::parse(""), None);
        // Surrounding whitespace in a hand-edited TOML list is a typo, not a
        // different capability.
        assert_eq!(Capability::parse("  network  "), Some(Capability::Network));
    }

    #[test]
    fn declaring_nothing_means_the_sandboxed_stdlib() {
        assert_eq!(CapabilitySet::none().stdlib(), Stdlib::Sandboxed);
        // Neither of the three capabilities that live entirely in the host
        // table opens the standard library.
        for cap in [Capability::Network, Capability::Model, Capability::Ui] {
            let set: CapabilitySet = [cap].into_iter().collect();
            assert_eq!(set.stdlib(), Stdlib::Sandboxed, "{cap}");
        }
    }

    #[test]
    fn filesystem_or_process_opens_the_full_stdlib() {
        for cap in [Capability::Filesystem, Capability::Process] {
            let set: CapabilitySet = [cap].into_iter().collect();
            assert_eq!(set.stdlib(), Stdlib::Full, "{cap}");
        }
        assert_eq!(CapabilitySet::all().stdlib(), Stdlib::Full);
    }

    #[test]
    fn a_capability_set_reports_what_it_holds() {
        let mut set = CapabilitySet::none();
        assert!(set.is_empty());
        assert_eq!(set.to_string(), "none");
        assert!(set.insert(Capability::Network));
        assert!(!set.insert(Capability::Network));
        assert!(set.insert(Capability::Filesystem));
        assert_eq!(set.len(), 2);
        assert!(set.contains(Capability::Network));
        assert!(!set.contains(Capability::Agent));
        // Ordered by declaration order, not insertion order.
        assert_eq!(set.to_string(), "filesystem, network");
        assert_eq!(CapabilitySet::all().len(), Capability::ALL.len());
    }

    #[test]
    fn a_manifest_parses_the_shape_the_spec_documents() {
        let manifest = PluginManifest::parse(
            r#"
            name = "web"
            version = "1.0.0"
            description = "Fetch and search the web"
            capabilities = ["network"]
            optional_deps = ["credentials"]
            profiles = ["full", "server"]
            "#,
        )
        .expect("the documented manifest parses");
        assert_eq!(manifest.name, "web");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.optional_deps, ["credentials"]);
        assert_eq!(manifest.profiles, ["full", "server"]);
        assert!(manifest.capability_set().contains(Capability::Network));
        assert_eq!(manifest.capability_set().stdlib(), Stdlib::Sandboxed);
    }

    #[test]
    fn a_misspelled_key_fails_the_manifest_instead_of_being_dropped() {
        let err = PluginManifest::parse(
            r#"
            name = "web"
            version = "1.0.0"
            capabilties = ["network"]
            "#,
        )
        .expect_err("an unknown key is a typo of a real one");
        assert!(matches!(err, ManifestError::Syntax(_)), "{err}");
    }

    #[test]
    fn a_misspelled_capability_fails_the_manifest() {
        let err = PluginManifest::parse(
            r#"
            name = "web"
            version = "1.0.0"
            capabilities = ["netwrok"]
            "#,
        )
        .expect_err("an unknown capability is refused");
        assert!(matches!(err, ManifestError::Syntax(_)), "{err}");
    }

    #[test]
    fn a_name_that_cannot_be_a_key_is_refused_at_load() {
        for bad in ["", "web tool", "web/tool", "web.tool", "wéb"] {
            let manifest = PluginManifest {
                name: bad.to_string(),
                ..PluginManifest::new("placeholder")
            };
            let err = manifest.validate().expect_err("'{bad}' should be refused");
            assert!(matches!(err, ManifestError::Name(_)), "{bad}: {err}");
        }
        for good in ["web", "web-tool", "web_tool", "web2"] {
            PluginManifest::new(good)
                .validate()
                .unwrap_or_else(|err| panic!("'{good}' should be accepted: {err}"));
        }
    }

    #[test]
    fn a_manifest_without_a_version_is_refused() {
        let manifest = PluginManifest {
            version: String::new(),
            ..PluginManifest::new("web")
        };
        let err = manifest.validate().expect_err("no version");
        assert!(matches!(err, ManifestError::Version(_)), "{err}");
        assert!(err.to_string().contains("web"));
    }

    #[test]
    fn the_builder_sets_capabilities() {
        let manifest =
            PluginManifest::new("todo").with_capabilities([Capability::Ui, Capability::Model]);
        let set = manifest.capability_set();
        assert!(set.contains(Capability::Ui));
        assert!(set.contains(Capability::Model));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn registry_is_the_default_source_because_the_safe_answer_has_to_be_the_lazy_one() {
        assert_eq!(PluginSource::default(), PluginSource::Registry);
    }
}
