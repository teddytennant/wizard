//! One row per cargo feature: what plugin it brings, what that plugin
//! registers, and whether this binary has it.
//!
//! [`super::compiled_in`] and [`super::bundled`] are the two tables that name
//! every plugin *that is here*. Neither can answer the question a user
//! actually asks, which is about the ones that are not: `--no-default-features`
//! produces a wizard with no provider at all, and the only thing that build
//! knows about `anthropic` is a `kind` string it does not recognise. Something
//! has to hold the sentence "there is a plugin called anthropic, this is what
//! it does, and `--features provider-anthropic` is how you get it".
//!
//! # Why core is allowed to hold this
//!
//! `docs/plugins.md`'s first rule is that no core module may
//! `use crate::<plugin>`, and this file does not: every row is four string
//! literals and a `cfg!`. That is exactly the split
//! [`crate::entrypoint::absent`] and [`crate::llm::registry::unknown`] already
//! make — core may hold the *name* a user types and the prose explaining how
//! to get the thing behind it, as long as it never names the type or
//! constructs one. The difference here is only that the sentences are in one
//! table instead of scattered across the call sites that need them.
//!
//! It lives in `src/plugins/` rather than somewhere in core proper so that the
//! three tables sit in one directory and a plugin added to one is missing from
//! the others at a glance. [`every_compiled_in_plugin_has_a_catalogue_row`]
//! is what turns "at a glance" into a test failure.
//!
//! [`every_compiled_in_plugin_has_a_catalogue_row`]: tests::every_compiled_in_plugin_has_a_catalogue_row

/// Which engine runs the plugin a feature brings.
///
/// The kernel cannot tell these apart once a plugin is loaded — that is the
/// claim `docs/plugins.md` opens with — but a person deciding whether to edit
/// one very much can, because it is the difference between needing a Rust
/// toolchain and needing a text editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Rust,
    Lua,
    Js,
}

impl Backend {
    /// The word `wizard plugin` prints. Lowercase because it appears in a
    /// column beside plugin names, not at the start of a sentence.
    pub fn name(self) -> &'static str {
        match self {
            Backend::Rust => "rust",
            Backend::Lua => "lua",
            Backend::Js => "js",
        }
    }
}

/// One cargo feature, and the plugin behind it.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    /// The cargo feature name, which is what `--features` takes.
    pub feature: &'static str,
    /// The plugin's manifest name, which is what the kernel keys on and what
    /// `wizard plugin show` takes.
    ///
    /// [`None`] for `plugin-js`, which is the one feature in the tree that is
    /// a *backend* rather than a plugin: it registers nothing and its whole
    /// contribution is that a `plugin.js` can be loaded at all.
    pub plugin: Option<&'static str>,
    /// [`None`] for the same row, and for the same reason.
    pub backend: Option<Backend>,
    /// One line, present tense, for somebody deciding whether they want it.
    pub summary: &'static str,
    /// Whether the feature is in Cargo.toml's `default` list. Only `native`
    /// answers `false`, and the reason is in Cargo.toml.
    pub default_on: bool,
    /// Whether *this* binary was built with it.
    pub present: bool,
}

/// Every plugin feature in the tree, alphabetically by feature name.
///
/// Alphabetical rather than in load order because this table is read by a
/// person looking for one row, while [`super::compiled_in`] is read by the
/// kernel in the order the first claim on a tool name wins. Two tables with
/// two jobs, sorted two ways on purpose.
pub const CATALOGUE: &[Entry] = &[
    Entry {
        feature: "acp",
        plugin: Some("acp"),
        backend: Some(Backend::Rust),
        summary: "`wizard acp`: serve an editor (Zed, Neovim, Emacs) over stdio",
        default_on: true,
        present: cfg!(feature = "acp"),
    },
    Entry {
        feature: "fleet",
        plugin: Some("fleet"),
        backend: Some(Backend::Rust),
        summary: "`wizard fleet`: split a mission across parallel headless workers, one git \
                  worktree each",
        default_on: true,
        present: cfg!(feature = "fleet"),
    },
    Entry {
        feature: "gateway",
        plugin: Some("gateway"),
        backend: Some(Backend::Rust),
        summary: "`wizard --gateway` and `wizard gateway <verb>`: one agent turn per inbound \
                  chat message",
        default_on: true,
        present: cfg!(feature = "gateway"),
    },
    Entry {
        feature: "graph",
        plugin: Some("graph"),
        backend: Some(Backend::Rust),
        summary: "the mesh explorer's data model; needs `mesh`, and the window is its only \
                  consumer",
        default_on: true,
        present: cfg!(feature = "graph"),
    },
    Entry {
        feature: "mesh",
        plugin: Some("mesh"),
        backend: Some(Backend::Rust),
        summary: "the P2P mesh: peer identity, the QUIC transport, the trust ledger and \
                  `wizard peers`",
        default_on: true,
        present: cfg!(feature = "mesh"),
    },
    Entry {
        feature: "native",
        plugin: Some("native"),
        backend: Some(Backend::Rust),
        summary: "`wizard gui`: the iced window. Off by default and shipped as its own release \
                  asset",
        default_on: false,
        present: cfg!(feature = "native"),
    },
    Entry {
        feature: "plugin-js",
        plugin: None,
        backend: None,
        summary: "the JavaScript plugin backend, one QuickJS VM per plugin. Without it a \
                  `plugin.js` does not load",
        default_on: true,
        present: cfg!(feature = "plugin-js"),
    },
    Entry {
        feature: "provider-anthropic",
        plugin: Some("anthropic"),
        backend: Some(Backend::Rust),
        summary: "Anthropic Messages API (kind = \"anthropic\")",
        default_on: true,
        present: cfg!(feature = "provider-anthropic"),
    },
    Entry {
        feature: "provider-chatgpt",
        plugin: Some("chatgpt"),
        backend: Some(Backend::Rust),
        summary: "ChatGPT by account sign-in (kind = \"chatgptoauth\")",
        default_on: true,
        present: cfg!(feature = "provider-chatgpt"),
    },
    Entry {
        feature: "provider-cloudflare",
        plugin: Some("cloudflare"),
        backend: Some(Backend::Rust),
        summary: "Cloudflare Workers AI (kind = \"cloudflare\")",
        default_on: true,
        present: cfg!(feature = "provider-cloudflare"),
    },
    Entry {
        feature: "provider-llamacpp",
        plugin: Some("llamacpp"),
        backend: Some(Backend::Rust),
        summary: "local llama.cpp, and the `llama-server` lifecycle under it (kind = \
                  \"llamacpp\")",
        default_on: true,
        present: cfg!(feature = "provider-llamacpp"),
    },
    Entry {
        feature: "provider-ollama",
        plugin: Some("ollama"),
        backend: Some(Backend::Rust),
        summary: "local Ollama, native /api/chat (kind = \"ollama\")",
        default_on: true,
        present: cfg!(feature = "provider-ollama"),
    },
    Entry {
        feature: "provider-openai",
        plugin: Some("openai"),
        backend: Some(Backend::Rust),
        summary: "the OpenAI-compatible family: OpenAI, OpenRouter, vLLM, LM Studio, DeepSeek \
                  (kind = \"openai\", \"openrouter\")",
        default_on: true,
        present: cfg!(feature = "provider-openai"),
    },
    Entry {
        feature: "provider-xai",
        plugin: Some("xai"),
        backend: Some(Backend::Rust),
        summary: "xAI Grok, by API key and by account sign-in (kind = \"xai\", \"xaioauth\")",
        default_on: true,
        present: cfg!(feature = "provider-xai"),
    },
    Entry {
        feature: "tool-git",
        plugin: Some("git"),
        backend: Some(Backend::Lua),
        summary: "the `git_status` and `git_diff` tools",
        default_on: true,
        present: cfg!(feature = "tool-git"),
    },
    Entry {
        feature: "tool-json",
        plugin: Some("json"),
        backend: Some(Backend::Js),
        summary: "the `json_query` tool: one value out of a JSON document without spending the \
                  context window on the rest. Needs `plugin-js`",
        default_on: true,
        present: cfg!(feature = "tool-json"),
    },
    Entry {
        feature: "tool-publish",
        plugin: Some("publish"),
        backend: Some(Backend::Lua),
        summary: "the `publish` tool and the `/publish` command: fork Wizard to your GitHub and \
                  hand back an installer",
        default_on: true,
        present: cfg!(feature = "tool-publish"),
    },
    Entry {
        feature: "tool-web",
        plugin: Some("web"),
        backend: Some(Backend::Rust),
        summary: "the `web_fetch`, `web_search` and `x_search` tools",
        default_on: true,
        present: cfg!(feature = "tool-web"),
    },
];

/// The row for one cargo feature.
pub fn feature(name: &str) -> Option<&'static Entry> {
    CATALOGUE.iter().find(|entry| entry.feature == name)
}

/// The row for one plugin, by the name its manifest carries.
///
/// This is the lookup `wizard plugin show <name>` falls back to when the
/// kernel has never heard of the name: on a build without `mesh`, `mesh` is
/// not a typo, it is a plugin somebody left out, and those two deserve
/// different answers.
pub fn plugin(name: &str) -> Option<&'static Entry> {
    CATALOGUE.iter().find(|entry| entry.plugin == Some(name))
}

/// Feature names this binary was built with, in catalogue order.
///
/// The only place the `cfg!` set is turned back into strings, which is what
/// [`super::profile::active`] compares against and what `wizard plugin` prints.
pub fn compiled_features() -> Vec<&'static str> {
    CATALOGUE
        .iter()
        .filter(|entry| entry.present)
        .map(|entry| entry.feature)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every plugin the kernel loaded is in the table, and its row says it is
    /// present.
    ///
    /// The failure this catches is a new plugin whose feature nobody added
    /// here: it would load fine, work fine, and be invisible to
    /// `wizard plugin`, which is the one surface whose entire job is to be
    /// complete. Asserted against the *process* kernel rather than against
    /// [`super::super::compiled_in`] so it covers the scripted plugins too —
    /// they are a second table and would otherwise need a second test that
    /// somebody would forget in the same way.
    #[tokio::test]
    async fn every_compiled_in_plugin_has_a_catalogue_row() {
        super::super::bundled::ensure().await;
        for id in super::super::kernel().loaded() {
            let entry = plugin(id.as_str())
                .unwrap_or_else(|| panic!("plugin '{id}' is loaded but has no catalogue row"));
            assert!(
                entry.present,
                "plugin '{id}' is loaded but its row says the feature is off"
            );
        }
    }

    /// And the other direction: a row that claims to be present names a plugin
    /// the kernel actually holds.
    ///
    /// `plugin-js` is the exception and it is the reason the field is an
    /// [`Option`] — it is a backend, so there is nothing for the kernel to
    /// hold. `graph` registers nothing through `Ctx` but is still a loaded
    /// plugin, so it is not an exception here.
    #[tokio::test]
    async fn a_row_that_says_present_names_a_plugin_the_kernel_loaded() {
        super::super::bundled::ensure().await;
        let loaded = super::super::kernel().loaded();
        for entry in CATALOGUE.iter().filter(|entry| entry.present) {
            let Some(name) = entry.plugin else { continue };
            assert!(
                loaded.iter().any(|id| id.as_str() == name),
                "catalogue says '{}' is present but the kernel has no plugin '{name}'",
                entry.feature
            );
        }
    }

    /// The table and Cargo.toml agree about which features exist and which are
    /// on by default.
    ///
    /// Read off the manifest at test time rather than restated, the same trick
    /// `contrib/check-tool-plugins.sh` uses: a feature added to `default` is
    /// covered the day it lands rather than the day somebody remembers this
    /// file. Only plugin features are compared — `dep:`-only rows are not in
    /// `[features]` at all, and the `default` list is the one place both
    /// spellings meet.
    #[test]
    fn the_catalogue_matches_cargo_tomls_default_list() {
        let manifest =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        let default: Vec<String> = manifest
            .lines()
            .skip_while(|line| !line.starts_with("default = ["))
            .skip(1)
            .take_while(|line| !line.starts_with(']'))
            .map(|line| line.trim().trim_matches(',').trim_matches('"').to_string())
            .filter(|line| !line.is_empty())
            .collect();
        assert!(
            default.len() > 10,
            "could not read `default` out of Cargo.toml"
        );

        for name in &default {
            let entry = feature(name)
                .unwrap_or_else(|| panic!("`{name}` is in `default` but not in the catalogue"));
            assert!(
                entry.default_on,
                "`{name}` is in `default` but the row says otherwise"
            );
        }
        for entry in CATALOGUE.iter().filter(|entry| entry.default_on) {
            assert!(
                default.iter().any(|f| f == entry.feature),
                "the catalogue says `{}` is on by default and Cargo.toml does not",
                entry.feature
            );
        }
    }

    /// No duplicate feature or plugin names, and every row says something.
    ///
    /// A duplicate would make `feature`/`plugin` return whichever came first
    /// and hide the other row forever, which is the kind of bug a table this
    /// shape invites.
    #[test]
    fn the_catalogue_is_a_set_and_every_row_is_filled_in() {
        let mut features: Vec<&str> = CATALOGUE.iter().map(|entry| entry.feature).collect();
        let count = features.len();
        features.sort_unstable();
        features.dedup();
        assert_eq!(features.len(), count, "a feature appears twice");

        let mut plugins: Vec<&str> = CATALOGUE.iter().filter_map(|entry| entry.plugin).collect();
        let named = plugins.len();
        plugins.sort_unstable();
        plugins.dedup();
        assert_eq!(plugins.len(), named, "a plugin name appears twice");

        for entry in CATALOGUE {
            assert!(!entry.summary.trim().is_empty(), "{}", entry.feature);
            assert!(!entry.summary.ends_with('.'), "{}", entry.summary);
            assert_eq!(
                entry.plugin.is_some(),
                entry.backend.is_some(),
                "{} names a plugin without a backend, or the reverse",
                entry.feature
            );
        }
    }

    /// Alphabetical, because a listing that reorders itself between releases
    /// is one people stop scanning.
    #[test]
    fn the_catalogue_is_sorted_by_feature_name() {
        let features: Vec<&str> = CATALOGUE.iter().map(|entry| entry.feature).collect();
        let mut sorted = features.clone();
        sorted.sort_unstable();
        assert_eq!(features, sorted);
    }
}
