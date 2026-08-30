//! Named plugin sets: five answers to "what kind of machine is this", each one
//! a cargo feature list.
//!
//! A profile is not a checkbox list. `--features` already is one, and it is the
//! wrong shape for the question somebody actually has when they are installing:
//! nobody knows whether they want `graph` before they have used the explorer,
//! and asking them to decide eighteen times is how a feature list stays
//! decorative. So there are five names, they are ordered by size, and each one
//! is a sentence about a person rather than a set.
//!
//! # Why these five
//!
//! Each profile has to be a *different answer*, not a different amount. The
//! two that shrink the build shrink it by dropping the thing that machine
//! genuinely cannot use:
//!
//! - [`MINIMAL`] is one API key and git. A CI container and a borrowed second
//!   machine both want the smallest thing that can still answer a prompt, and
//!   the smallest thing that can answer a prompt is a provider — a build with
//!   none is `--no-default-features`, which is a floor rather than an install.
//!   `provider-openai` is in it because that one flag reaches OpenAI,
//!   OpenRouter, vLLM, LM Studio, DeepSeek and every `compat.rs` preset;
//!   `provider-anthropic` is in it because the other half of the world has that
//!   key and not this one.
//! - [`PI`] is the same size and the opposite key: no cloud provider at all,
//!   two local ones, and no JavaScript backend. A Raspberry Pi runs the model
//!   itself, so what it needs is `llama-server` and Ollama and nothing that
//!   assumes an account.
//! - [`SERVER`] is the stock build minus `mesh` and `graph`. The mesh is mDNS
//!   discovery and a QUIC listener, which is a thing a box in a datacenter
//!   should not be doing by default, and it is also the single feature that
//!   measurably shrinks the binary (`quinn`, `rustls` and `mdns-sd` leave with
//!   it). Everything a headless box *is* reached by — ACP, the gateway, fleet,
//!   every provider — stays.
//! - [`DEFAULT`] is what `cargo install --path .` builds and what every
//!   published release binary is. It is in the list so that
//!   `wizard plugin profiles` can say "you have this one" rather than leaving
//!   the stock build unnamed.
//! - [`FULL`] is [`DEFAULT`] plus the window.
//!
//! There is deliberately no profile per feature and no `custom` row.
//! `--features a,b,c` is already the custom profile and giving it a name would
//! only add a spelling.
//!
//! # The two places this table exists
//!
//! Here, and in `install.sh`'s `profile_features`. It cannot be one place:
//! `install.sh` is fetched and piped to bash by people who have no checkout, so
//! it cannot read a file from the repository, and this module cannot be
//! consulted before the binary it is compiled into exists.
//! [`the_installer_agrees_about_every_profile`] is what keeps the two copies
//! honest — it sources `install.sh` and diffs the answers.
//!
//! [`the_installer_agrees_about_every_profile`]: tests::the_installer_agrees_about_every_profile

use super::catalogue::{self, CATALOGUE};

/// How a profile's feature list is built out of the catalogue.
///
/// Three shapes rather than one written-out list per profile, because two of
/// the five are defined *relative to the default build* and a written-out copy
/// of `default` would go stale the first time a feature was added to it. The
/// list in Cargo.toml stays the source of truth; this is arithmetic on it.
#[derive(Debug, Clone, Copy)]
enum Shape {
    /// The stock build: no cargo flags at all.
    Default,
    /// The stock build plus these.
    DefaultPlus(&'static [&'static str]),
    /// The stock build minus these.
    DefaultMinus(&'static [&'static str]),
    /// Built up from nothing: `--no-default-features --features <these>`.
    Only(&'static [&'static str]),
}

/// A named plugin set.
#[derive(Debug, Clone, Copy)]
pub struct Profile {
    /// What `WIZARD_PROFILE` and `wizard plugin profiles` call it.
    pub name: &'static str,
    /// Who it is for, in one line. The question this whole file answers, so it
    /// is phrased as a person and a machine rather than as a feature count.
    pub audience: &'static str,
    shape: Shape,
}

/// One API key and git. The smallest build that can still answer a prompt.
pub const MINIMAL: Profile = Profile {
    name: "minimal",
    audience: "CI containers and second machines: one API key, git, nothing else",
    shape: Shape::Only(&["provider-anthropic", "provider-openai", "tool-git"]),
};

/// A local model and nothing that needs an account.
pub const PI: Profile = Profile {
    name: "pi",
    audience: "Raspberry Pi and small ARM: a local model, no cloud provider, no JS backend",
    shape: Shape::Only(&["provider-llamacpp", "provider-ollama", "tool-git"]),
};

/// The stock build without local peer discovery.
pub const SERVER: Profile = Profile {
    name: "server",
    audience: "headless boxes: every provider and every remote surface, no P2P mesh",
    shape: Shape::DefaultMinus(&["graph", "mesh"]),
};

/// What `cargo install --path .` builds.
pub const DEFAULT: Profile = Profile {
    name: "default",
    audience: "everyone else: every backend and every tool, no window",
    shape: Shape::Default,
};

/// Everything, window included.
pub const FULL: Profile = Profile {
    name: "full",
    audience: "one binary with the GUI in it, for a desktop you build yourself",
    shape: Shape::DefaultPlus(&["native"]),
};

/// Every profile, smallest first.
///
/// The order is the order `wizard plugin profiles` prints, and it is by size
/// because that is the axis somebody scanning the list is choosing along.
pub const PROFILES: &[Profile] = &[MINIMAL, PI, SERVER, DEFAULT, FULL];

impl Profile {
    /// The feature list this profile resolves to, in catalogue order.
    ///
    /// Catalogue order rather than the order the shape names them, so two
    /// profiles that hold the same set print the same string and the installer
    /// cross-check compares sorted lists on both sides.
    pub fn features(&self) -> Vec<&'static str> {
        match self.shape {
            Shape::Default => default_features(),
            Shape::DefaultPlus(extra) => {
                let mut features = default_features();
                for name in extra {
                    if !features.contains(name) {
                        features.push(name);
                    }
                }
                features.sort_unstable();
                features
            }
            Shape::DefaultMinus(dropped) => default_features()
                .into_iter()
                // A feature that another kept feature enables is not removed by
                // dropping it from the list — `--features graph` turns `mesh`
                // back on — which is why `server` drops both and why this is a
                // filter over the whole list rather than a `retain` on one name.
                .filter(|name| !dropped.contains(name))
                .collect(),
            Shape::Only(only) => CATALOGUE
                .iter()
                .map(|entry| entry.feature)
                .filter(|name| only.contains(name))
                .collect(),
        }
    }

    /// The cargo flags that build it, ready to splice into a command line.
    ///
    /// Empty for [`DEFAULT`], and that emptiness is load-bearing: a stock
    /// `cargo install --path .` and a stock `install.sh` run have to behave
    /// exactly as they did before profiles existed, which they do by being
    /// handed no flags at all rather than by being handed the default list
    /// spelled out. The two are not the same command — `--no-default-features
    /// --features <every default>` is one `--features` resolution away from the
    /// stock build and would be a difference nobody could see until it bit.
    pub fn cargo_flags(&self) -> Vec<String> {
        match self.shape {
            Shape::Default => Vec::new(),
            Shape::DefaultPlus(extra) => {
                vec!["--features".to_string(), extra.join(",")]
            }
            Shape::DefaultMinus(_) | Shape::Only(_) => vec![
                "--no-default-features".to_string(),
                "--features".to_string(),
                self.features().join(","),
            ],
        }
    }

    /// Whether this binary was built with exactly this profile's features.
    fn matches_this_build(&self) -> bool {
        self.features() == catalogue::compiled_features()
    }
}

/// Cargo's `default` list, in catalogue order.
///
/// Derived from the catalogue rather than restated, so the one place a feature
/// is declared on-by-default is the `default_on` field, and that field is
/// checked against Cargo.toml itself by
/// `catalogue::tests::the_catalogue_matches_cargo_tomls_default_list`.
fn default_features() -> Vec<&'static str> {
    CATALOGUE
        .iter()
        .filter(|entry| entry.default_on)
        .map(|entry| entry.feature)
        .collect()
}

/// The profile whose feature set this binary was built with, if it is one of
/// them.
///
/// [`None`] for any hand-rolled `--features` list, which is most of what
/// `contrib/check-tool-plugins.sh` produces and is not an error — a build that
/// matches no profile is a build somebody composed on purpose, and saying
/// "custom" is a more honest answer than picking the nearest name.
pub fn active() -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| profile.matches_this_build())
}

/// The profile with this name.
pub fn by_name(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| profile.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every feature every profile names is a feature that exists.
    ///
    /// A typo here would produce a `cargo build --features privder-openai` that
    /// fails at the install, on somebody else's machine, several minutes into a
    /// compile.
    #[test]
    fn every_profile_names_real_features() {
        for profile in PROFILES {
            for name in profile.features() {
                assert!(
                    catalogue::feature(name).is_some(),
                    "profile `{}` names `{name}`, which is not a cargo feature",
                    profile.name
                );
            }
        }
    }

    /// The stock build is `default`, and it is built with no flags.
    ///
    /// The whole opt-in promise in one assertion: whatever else profiles do,
    /// asking for the default one has to produce the command that was already
    /// being run.
    #[test]
    fn the_default_profile_is_the_stock_build_and_passes_no_flags() {
        assert!(DEFAULT.cargo_flags().is_empty());
        assert_eq!(DEFAULT.features(), default_features());
        assert!(!DEFAULT.features().contains(&"native"));
    }

    /// `server` is the default build minus the mesh and the explorer over it,
    /// and keeps everything a headless box is reached by.
    #[test]
    fn server_drops_the_mesh_and_keeps_every_remote_surface() {
        let features = SERVER.features();
        assert!(!features.contains(&"mesh"));
        assert!(!features.contains(&"graph"));
        for kept in ["acp", "gateway", "fleet", "provider-anthropic", "tool-web"] {
            assert!(features.contains(&kept), "server dropped `{kept}`");
        }
    }

    /// Each profile is a different set, and they are listed smallest first.
    ///
    /// Two profiles resolving to the same features would be two names for one
    /// build, which is the "a profile per feature" failure arriving from the
    /// other direction.
    #[test]
    fn the_profiles_are_distinct_and_ordered_by_size() {
        let sets: Vec<Vec<&str>> = PROFILES.iter().map(Profile::features).collect();
        for (i, left) in sets.iter().enumerate() {
            for right in sets.iter().skip(i + 1) {
                assert_ne!(left, right, "two profiles resolve to the same features");
            }
        }
        for pair in sets.windows(2) {
            assert!(
                pair[0].len() <= pair[1].len(),
                "PROFILES is not ordered by size: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    /// Every profile is named, described, and findable by its own name.
    #[test]
    fn every_profile_is_findable_and_says_who_it_is_for() {
        for profile in PROFILES {
            assert_eq!(by_name(profile.name).map(|p| p.name), Some(profile.name));
            assert!(!profile.audience.trim().is_empty(), "{}", profile.name);
            assert!(!profile.audience.ends_with('.'), "{}", profile.audience);
        }
        assert!(by_name("no-such-profile").is_none());
    }

    /// A stock `cargo test` is the `default` profile, and says so.
    ///
    /// Skipped on any other feature set, because leaving a plugin out is the
    /// whole point of the flags and `contrib/check-tool-plugins.sh` runs this
    /// suite under a dozen sets that match no profile at all. What the guard
    /// asserts is the pairing: with every default feature on and `native` off,
    /// `active()` must find `default` and not `None`.
    #[test]
    fn a_stock_build_reports_the_default_profile() {
        if catalogue::compiled_features() != default_features() {
            return;
        }
        assert_eq!(active().map(|p| p.name), Some("default"));
    }

    /// Every plugin's manifest agrees with this table about which profiles it
    /// is in.
    ///
    /// `PluginManifest::profiles` predates this module: it was written from the
    /// design sketch in `docs/plugins.md`, no code read it, and by the time the
    /// profiles were real it was wrong in five places — every provider claimed
    /// `server` and none claimed `minimal`, the mesh claimed a profile that now
    /// drops it, and `json` claimed two that cannot load a `plugin.js` at all.
    /// A declaration nothing checks is documentation that decays, so this is
    /// the check.
    ///
    /// The manifest stays the place an author writes it down, because that is
    /// where somebody reading one plugin looks; this table stays the place the
    /// build is computed from, because a profile has to resolve on a build that
    /// left the plugin out. The two meet here.
    ///
    /// Only loaded plugins are compared, which is all that can be: a manifest
    /// is a value a compiled-in plugin returns, so a build without the feature
    /// has nothing to disagree with. `contrib/check-tool-plugins.sh` is what
    /// makes that coverage total — between its legs and the default build,
    /// every plugin is loaded in some run of this test.
    #[tokio::test]
    async fn every_manifest_declares_the_profiles_this_table_puts_it_in() {
        super::super::bundled::ensure().await;
        for report in super::super::kernel().reports() {
            let Some(entry) = catalogue::plugin(report.id.as_str()) else {
                continue;
            };
            let expected: Vec<&str> = PROFILES
                .iter()
                .filter(|profile| profile.features().contains(&entry.feature))
                .map(|profile| profile.name)
                .collect();
            assert_eq!(
                report.manifest.profiles, expected,
                "plugin '{}' declares {:?} and this table puts it in {expected:?}",
                report.id, report.manifest.profiles,
            );
        }
    }

    /// `install.sh` resolves every profile to the same feature list this module
    /// does.
    ///
    /// The two copies of the table are the price of an installer that is piped
    /// from a URL, and this is the only thing standing between them and a
    /// silent drift where `WIZARD_PROFILE=server` builds something the binary
    /// then reports as custom. Sourcing the script with `WIZARD_SELFTEST=1`
    /// defines its functions without installing anything, which is the same
    /// door `crate::update`'s installer tests go through.
    #[test]
    fn the_installer_agrees_about_every_profile() {
        let installer = concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh");
        if !std::path::Path::new(installer).is_file() {
            // A packaged crate has no installer to check. Nothing to prove.
            return;
        }
        for profile in PROFILES {
            let out = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!(
                    "set -eu; WIZARD_SELFTEST=1 . '{installer}'; profile_features '{}'",
                    profile.name
                ))
                .output()
                .expect("run bash against install.sh");
            assert!(
                out.status.success(),
                "install.sh could not resolve profile `{}`: {}",
                profile.name,
                String::from_utf8_lossy(&out.stderr)
            );
            let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let mut theirs: Vec<&str> = printed.split(',').filter(|s| !s.is_empty()).collect();
            theirs.sort_unstable();
            let mut ours = profile.features();
            ours.sort_unstable();
            assert_eq!(
                theirs, ours,
                "install.sh and this table disagree about profile `{}`",
                profile.name
            );
        }
    }
}
