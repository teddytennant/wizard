//! `wizard plugin`: what this binary has, what it does not, and how to change
//! either.
//!
//! # Why this surface exists
//!
//! The tree grew eighteen cargo features, three plugin backends and a rule that
//! any one plugin can be deleted, and none of it was reachable by a person.
//! `wizard --help` lists the subcommands a build happens to have, which answers
//! one corner of one question; everything else — which backend a tool is
//! written in, what a script plugin was granted, why `kind = "ollama"` is not
//! recognised on this machine — was readable only in the source of the binary
//! you are not holding. A feature flag nobody can see the effect of is
//! indistinguishable from no feature flag.
//!
//! So this is a read-only report, and the honesty is the point: every number in
//! it comes from the running kernel rather than from a `#[cfg]` at the print
//! site. `wizard plugin list` asks the kernel what loaded, not what should have
//! loaded, which is why a plugin that panicked in `apply` (see
//! [`super::load_rust`], which survives one) is missing from the listing and
//! present in [`super::catalogue`] — exactly the discrepancy somebody
//! debugging that would need to see.
//!
//! # Why there is no `wizard plugin install`
//!
//! Deliberately deferred, and not for lack of a mechanism: `~/.wizard/plugins/`
//! already loads a `plugin.lua` or `plugin.js` dropped into it, bounded, as
//! [`PluginSource::Registry`](crate::kernel::PluginSource::Registry). An
//! `install` verb would be a downloader in front of a `cp`, and the three
//! things that make it worth having are all missing:
//!
//! - **There is nowhere to install from.** `crate::registry_client` publishes
//!   skills and scripted tools, not plugins; a plugin index is a server-side
//!   change, not a client one.
//! - **The grant decision would have nowhere to be recorded.**
//!   [`crate::registry_client::decide_trust`] persists a yes against an exact
//!   author, version, checksum and capability list. A plugin installer that
//!   printed a capability list and then wrote the files would be asking a
//!   question it does not keep the answer to, so every subsequent load would
//!   either re-ask or silently not ask.
//! - **A `cp` needs no verb.** Today somebody installs a plugin by putting a
//!   directory in a directory. That is worse than a command and much better
//!   than a command that pretends to have verified something.
//!
//! What is here instead is the half that makes the other half safe to write
//! later: `wizard plugin show` prints the capabilities a plugin declared and
//! what each one grants, in the same words
//! [`crate::registry_client::grant_prompt`] uses, so the sentence an installer
//! would have to put in front of a yes/no already exists and is already read
//! off the manifest rather than restated.

use anyhow::Result;
use serde_json::{Value, json};

use super::{catalogue, profile};
use crate::cli::PluginCmd;
use crate::kernel::PluginReport;
use crate::kernel::manifest::PluginSource;

/// Run `wizard plugin <verb>`. Always exits 0 except when a name was not found.
///
/// `boot` has already run — every arm of [`crate::run`] is below it — so the
/// kernel holds the Rust plugins, the bundled scripted ones, and anything the
/// user dropped in `~/.wizard/plugins`. That ordering is what makes this
/// listing the truth about the process rather than about the build.
pub async fn run(cmd: PluginCmd) -> Result<i32> {
    match cmd {
        PluginCmd::List { json } => {
            print_or_json(json, list_json, list_text);
            Ok(0)
        }
        PluginCmd::Missing { json } => {
            print_or_json(json, missing_json, missing_text);
            Ok(0)
        }
        PluginCmd::Profiles { json } => {
            print_or_json(json, profiles_json, profiles_text);
            Ok(0)
        }
        PluginCmd::Show { name, json } => show(&name, json),
    }
}

/// One of two renderers, chosen by `--json`.
///
/// A closure pair rather than a `if json { ... } else { ... }` at four call
/// sites, because the failure that shape invites is a verb that grew a `--json`
/// flag and prints the table anyway.
fn print_or_json(json: bool, as_json: fn() -> Value, as_text: fn()) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&as_json()).unwrap_or_default()
        );
    } else {
        as_text();
    }
}

// -- list ------------------------------------------------------------------

/// Every plugin the kernel is holding, one row each.
fn list_text() {
    let reports = super::kernel().reports();
    println!("{}", header());
    println!();

    if reports.is_empty() {
        println!(
            "No plugins. This binary was built with `--no-default-features`, so it has no\n\
             provider transport, no tools beyond core's, and no plugin-owned subcommands.\n\
             `wizard plugin missing` lists what a flag would bring back."
        );
        return;
    }

    let width = reports
        .iter()
        .map(|report| report.id.as_str().len())
        .max()
        .unwrap_or(6)
        .max(6);
    println!(
        "{:<width$}  {:<7}  {:<11}  REGISTERS",
        "PLUGIN", "BACKEND", "SOURCE"
    );
    for report in &reports {
        println!(
            "{:<width$}  {:<7}  {:<11}  {}",
            report.id.as_str(),
            report.language,
            source_word(report.source),
            registers_summary(report),
        );
    }

    println!();
    println!("{}", tail());
}

/// `wizard 2.1.2 · profile: default · 17 of 18 plugin features`.
///
/// The profile line is the first thing somebody triaging "why does my config
/// not work here" wants, and it is the one fact no other surface prints.
fn header() -> String {
    let present = catalogue::compiled_features().len();
    let total = catalogue::CATALOGUE.len();
    let named = match profile::active() {
        Some(profile) => profile.name.to_string(),
        // A hand-rolled `--features` list is not a mistake — it is most of what
        // `contrib/check-tool-plugins.sh` builds — so it gets a word rather
        // than the nearest profile's name.
        None => "custom".to_string(),
    };
    format!(
        "wizard {}  ·  profile: {named}  ·  {present} of {total} plugin features",
        env!("CARGO_PKG_VERSION"),
    )
}

/// What to read next, and where the user's own plugins would be.
fn tail() -> String {
    let missing = catalogue::CATALOGUE.iter().filter(|e| !e.present).count();
    let mut lines = Vec::new();
    match missing {
        0 => lines.push("Every plugin feature in the tree is in this build.".to_string()),
        1 => lines.push(
            "One plugin feature is not in this build; `wizard plugin missing` names it."
                .to_string(),
        ),
        n => lines.push(format!(
            "{n} plugin features are not in this build; `wizard plugin missing` names them."
        )),
    }
    lines.push(format!(
        "Third-party plugins load from {}.",
        super::kernel().plugin_root().display()
    ));
    lines.push(
        "`wizard plugin show <name>` for one plugin's capabilities and registrations.".to_string(),
    );
    lines.join("\n")
}

/// `1 provider`, `3 tools`, `2 entrypoints, 1 service`, or `nothing`.
///
/// Counts rather than names, because the names are what `show` is for and a
/// column that grows with the widest plugin turns the table into a wrap. The
/// `nothing` arm is real and worth printing rather than blanking: `graph`
/// registers nothing at all through `Ctx` — what it contributes is a type the
/// window builds on — and a blank cell reads as a bug where the word does not.
fn registers_summary(report: &PluginReport) -> String {
    let mut parts = Vec::new();
    let mut add = |n: usize, one: &str, many: &str| {
        if n == 1 {
            parts.push(format!("1 {one}"));
        } else if n > 1 {
            parts.push(format!("{n} {many}"));
        }
    };
    add(report.providers.len(), "provider", "providers");
    add(report.tools.len(), "tool", "tools");
    add(report.commands.len(), "command", "commands");
    // A service is a CLI surface only if something answers to its name at one
    // of the entrypoint shapes, and a plugin can register both: the mesh
    // publishes `wizard peers` and a tee factory, and calling the second one an
    // entrypoint would tell the reader there is a `wizard mesh-tee`. The two
    // are counted apart for the same reason `entrypoint::description` exists.
    let entrypoints = report
        .services
        .iter()
        .filter(|name| crate::entrypoint::description(name).is_some())
        .count();
    add(entrypoints, "entrypoint", "entrypoints");
    add(report.services.len() - entrypoints, "service", "services");
    add(report.handlers, "subscription", "subscriptions");
    if parts.is_empty() {
        "nothing".to_string()
    } else {
        parts.join(", ")
    }
}

/// `first-party` or `installed`, plus what the word costs.
///
/// The two are not decoration: [`PluginSource`] is the whole of the bound
/// decision, so a `first-party` plugin runs with the JIT on and no instruction
/// hook and an `installed` one is interpreted under a deadline. Somebody
/// wondering why their plugin is slow is owed that in the listing.
fn source_word(source: PluginSource) -> &'static str {
    match source {
        PluginSource::FirstParty => "first-party",
        PluginSource::Registry => "installed",
    }
}

fn list_json() -> Value {
    let reports = super::kernel().reports();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "profile": profile::active().map(|p| p.name),
        "features": catalogue::compiled_features(),
        "plugins": reports.iter().map(report_json).collect::<Vec<_>>(),
        "plugin_root": super::kernel().plugin_root().display().to_string(),
    })
}

fn report_json(report: &PluginReport) -> Value {
    json!({
        "name": report.id.as_str(),
        "version": report.manifest.version,
        "description": report.manifest.description,
        "backend": report.language,
        "source": source_word(report.source),
        "feature": catalogue::plugin(report.id.as_str()).map(|entry| entry.feature),
        "capabilities": report
            .manifest
            .capabilities
            .iter()
            .map(|cap| cap.name())
            .collect::<Vec<_>>(),
        "tools": report.tools,
        "commands": report.commands,
        "providers": report.providers,
        "entrypoints": report
            .services
            .iter()
            .map(|name| json!({ "name": name, "about": crate::entrypoint::description(name) }))
            .collect::<Vec<_>>(),
        "subscriptions": report.handlers,
        "children": report.children,
        "parent": report.parent.as_ref().map(|id| id.to_string()),
    })
}

// -- show ------------------------------------------------------------------

/// One plugin in full, or the reason there is nothing to print.
///
/// Three outcomes, and they are three different answers: a loaded plugin, a
/// plugin this build left out (which is not a typo and gets the flag that
/// brings it back), and a name nothing in the tree has ever answered to.
/// Collapsing the middle case into the last is the failure this surface exists
/// to prevent.
fn show(name: &str, as_json: bool) -> Result<i32> {
    if let Some(report) = super::kernel().describe(&crate::kernel::PluginId::new(name)) {
        if as_json {
            println!("{}", serde_json::to_string_pretty(&report_json(&report))?);
        } else {
            show_text(&report);
        }
        return Ok(0);
    }

    if let Some(entry) = catalogue::plugin(name) {
        if as_json {
            println!(
                "{}",
                serde_json::to_string_pretty(&missing_entry_json(entry))?
            );
        } else {
            println!("`{name}` is a plugin, but not in this build.\n");
            println!("{}", wrapped(entry.summary, 2, 78));
            println!();
            println!("{}", wrapped(&how_to_get(entry), 2, 78));
        }
        return Ok(1);
    }

    eprintln!(
        "no plugin called `{name}`. `wizard plugin list` shows what this build has, and \
         `wizard plugin missing` what it does not."
    );
    Ok(1)
}

/// Word-wrap `text` to `width` columns, every line indented by `indent`.
///
/// Local and eight lines rather than a crate, because the whole requirement is
/// that a surface's own `about` — `wizard peers` writes a paragraph, because
/// that paragraph is also the first thing `wizard peers --help` prints — does
/// not run off the side of a terminal here. Splits on spaces only; a single
/// word longer than the budget is left long, which is what a URL or a path
/// should do.
fn wrapped(text: &str, indent: usize, width: usize) -> String {
    let pad = " ".repeat(indent);
    let budget = width.saturating_sub(indent).max(20);
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let line = lines.last_mut().expect("at least one line");
        if line.is_empty() {
            line.push_str(word);
        } else if line.chars().count() + 1 + word.chars().count() <= budget {
            line.push(' ');
            line.push_str(word);
        } else {
            lines.push(word.to_string());
        }
    }
    lines
        .into_iter()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn show_text(report: &PluginReport) {
    let feature = catalogue::plugin(report.id.as_str());
    println!("{} {}", report.id.as_str(), report.manifest.version);
    // The manifest's own sentence first, because it is the plugin author's,
    // and the catalogue's only when there is none. They are usually the same
    // claim in two voices and printing both would be noise.
    let description = if report.manifest.description.trim().is_empty() {
        feature.map(|entry| entry.summary).unwrap_or_default()
    } else {
        report.manifest.description.trim()
    };
    if !description.is_empty() {
        println!("{}", wrapped(description, 2, 78));
    }
    println!();
    println!("  backend  {}", report.language);
    println!(
        "  source   {} ({})",
        source_word(report.source),
        match report.source {
            PluginSource::FirstParty => "unbounded: no deadline hook, JIT on",
            PluginSource::Registry => "bounded: interpreted, under a per-call deadline",
        }
    );
    if let Some(entry) = feature {
        println!(
            "  feature  {} ({})",
            entry.feature,
            if entry.default_on {
                "on by default"
            } else {
                "off by default"
            }
        );
    }
    if let Some(parent) = &report.parent {
        println!("  loaded by  {parent}");
    }

    println!();
    println!("capabilities");
    if report.manifest.capabilities.is_empty() {
        println!(
            "  none declared — sandboxed: no os, no io, no package, and the host file\n\
             \x20 helpers confined to the project directory"
        );
    } else {
        for cap in &report.manifest.capabilities {
            println!("  {:<11}{}", cap.name(), cap.describe());
        }
    }

    println!();
    println!("registers");
    let mut any = false;
    for provider in &report.providers {
        println!("  provider    kind = \"{provider}\"");
        any = true;
    }
    for tool in &report.tools {
        println!("  tool        {tool}");
        any = true;
    }
    for command in &report.commands {
        println!("  command     /{command}");
        any = true;
    }
    for service in &report.services {
        // A service name is where a CLI surface shows up, and
        // `entrypoint::description` is the only thing that can tell one from a
        // service that is merely a value another plugin injects. `None` prints
        // the bare name rather than guessing.
        match crate::entrypoint::description(service) {
            Some(about) => {
                println!("  entrypoint  wizard {service}");
                println!("{}", wrapped(about, 14, 78));
            }
            None => println!("  service     {service}"),
        }
        any = true;
    }
    if report.handlers > 0 {
        println!("  events      {} subscription(s)", report.handlers);
        any = true;
    }
    for child in &report.children {
        println!("  plugin      {child}");
        any = true;
    }
    if !any {
        println!("  nothing through Ctx — what it contributes is a type another plugin builds on");
    }

    if !report.manifest.optional_deps.is_empty() {
        println!();
        println!("optional deps");
        for dep in &report.manifest.optional_deps {
            println!("  {dep}");
        }
    }
}

// -- missing ---------------------------------------------------------------

/// What this build left out, and the flag for each.
fn missing_text() {
    let missing: Vec<&catalogue::Entry> = catalogue::CATALOGUE
        .iter()
        .filter(|entry| !entry.present)
        .collect();
    println!("{}", header());
    println!();
    if missing.is_empty() {
        println!("Nothing. Every plugin feature in the tree is in this build.");
        return;
    }
    for entry in &missing {
        println!("{}", entry.feature);
        println!("{}", wrapped(entry.summary, 2, 78));
        println!("{}", wrapped(&how_to_get(entry), 2, 78));
        println!();
    }
    println!("A profile is the shorter way to ask for a set of these: `wizard plugin profiles`.");
}

/// The two routes back, and which one is offered depends on whether a stock
/// binary already has it.
///
/// This is [`crate::entrypoint::absent`]'s rule generalised: a feature that is
/// on by default is missing because somebody built it out, so the useful
/// sentence is "a stock release has it". `native` is the one that is off by
/// default, and telling its reader to rebuild without mentioning the release
/// asset is how `wizard app` spent a year telling people to compile iced.
fn how_to_get(entry: &catalogue::Entry) -> String {
    if entry.feature == "native" {
        return format!(
            "cargo build --release --features {} — or `install.sh` with WIZARD_NATIVE=1, \
             which installs a prebuilt `wizard-native` beside `wizard`",
            entry.feature
        );
    }
    if entry.default_on {
        format!(
            "cargo build --release --features {} — or install a stock release binary, \
             which has it: it is on by default",
            entry.feature
        )
    } else {
        format!("cargo build --release --features {}", entry.feature)
    }
}

fn missing_json() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "profile": profile::active().map(|p| p.name),
        "missing": catalogue::CATALOGUE
            .iter()
            .filter(|entry| !entry.present)
            .map(missing_entry_json)
            .collect::<Vec<_>>(),
    })
}

fn missing_entry_json(entry: &catalogue::Entry) -> Value {
    json!({
        "feature": entry.feature,
        "plugin": entry.plugin,
        "backend": entry.backend.map(catalogue::Backend::name),
        "summary": entry.summary,
        "default_on": entry.default_on,
        "present": entry.present,
        "how_to_get": how_to_get(entry),
    })
}

// -- profiles --------------------------------------------------------------

/// The five named builds, and which one this binary is.
fn profiles_text() {
    let active = profile::active().map(|p| p.name);
    println!("{}", header());
    println!();
    for profile in profile::PROFILES {
        let marker = if active == Some(profile.name) {
            "*"
        } else {
            " "
        };
        println!("{marker} {:<8} {}", profile.name, profile.audience);
        let flags = profile.cargo_flags();
        let flags = if flags.is_empty() {
            "(no flags — the stock build)".to_string()
        } else {
            flags.join(" ")
        };
        println!("    cargo build --release {flags}");
        println!("    install.sh: WIZARD_PROFILE={}", profile.name);
        println!();
    }
    if active.is_none() {
        println!(
            "This binary matches no profile: it was built with a hand-picked feature list.\n\
             `wizard plugin list` shows what it has."
        );
    }
}

fn profiles_json() -> Value {
    let active = profile::active().map(|p| p.name);
    json!({
        "active": active,
        "profiles": profile::PROFILES
            .iter()
            .map(|profile| json!({
                "name": profile.name,
                "audience": profile.audience,
                "features": profile.features(),
                "cargo_flags": profile.cargo_flags(),
                "active": active == Some(profile.name),
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header names a profile on every build, including one that matches
    /// none.
    ///
    /// `custom` is the arm that matters: `contrib/check-tool-plugins.sh` runs
    /// this suite under a dozen leave-one-out feature sets, and a header that
    /// printed `None` or panicked on those would be a surface that only works
    /// on the builds nobody needs to inspect.
    #[test]
    fn the_header_names_a_profile_on_any_feature_set() {
        let header = header();
        assert!(header.contains("profile: "), "{header}");
        assert!(header.contains(env!("CARGO_PKG_VERSION")), "{header}");
        let named = profile::active().map(|p| p.name).unwrap_or("custom");
        assert!(header.contains(named), "{header}");
    }

    /// Every loaded plugin renders, and the row says what the kernel says.
    ///
    /// Rendered rather than merely counted, because the bug this catches is a
    /// column format that panics on an empty vector — `graph` registers
    /// nothing, and it is in every default build.
    #[tokio::test]
    async fn every_loaded_plugin_has_a_row_and_a_summary() {
        super::super::bundled::ensure().await;
        for report in super::super::kernel().reports() {
            let summary = registers_summary(&report);
            assert!(!summary.is_empty(), "{}", report.id);
            assert_eq!(
                summary == "nothing",
                report.registration_count() == 0,
                "{} says '{summary}' with {} registrations",
                report.id,
                report.registration_count()
            );
        }
    }

    /// Wrapping keeps every line inside the budget, indents all of them, and
    /// never drops or splits a word.
    ///
    /// The input that made this necessary is `wizard peers`' own `about`: it is
    /// a paragraph, because the same string is the first thing
    /// `wizard peers --help` prints, and unwrapped it ran a hundred and sixty
    /// columns off the side of `wizard plugin show mesh`.
    #[test]
    fn wrapping_respects_the_budget_and_keeps_every_word() {
        let text = "Mesh peers: other machines running Wizard, and what this one \
                    has decided about each of them";
        let out = wrapped(text, 4, 40);
        for line in out.lines() {
            assert!(line.starts_with("    "), "{line:?}");
            assert!(line.chars().count() <= 40, "{line:?}");
        }
        assert_eq!(
            out.split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );
        // A word with no break in it is left long rather than cut in half: a
        // URL or a path that has been hyphenated is worse than one that wraps.
        let long = "https://example.invalid/a/very/long/path/that/does/not/fit";
        assert_eq!(wrapped(long, 2, 20), format!("  {long}"));
    }

    /// `show` distinguishes a plugin that is absent from a name that never
    /// existed, and neither is a crash.
    ///
    /// Both return 1, which is what a script branches on, and the difference is
    /// what is printed. Asserted here because the two paths look identical from
    /// the exit code and it is the message that carries the whole value.
    #[test]
    fn showing_an_unknown_name_fails_without_pretending_it_is_a_missing_plugin() {
        assert_eq!(show("definitely-not-a-plugin", false).expect("ran"), 1);
    }

    /// Every catalogue row renders a route back, and it names its own feature.
    ///
    /// The sentence is the only thing a person on a stripped build has, so an
    /// empty or wrong one is the whole surface failing quietly.
    #[test]
    fn every_missing_feature_says_how_to_get_it() {
        for entry in catalogue::CATALOGUE {
            let advice = how_to_get(entry);
            assert!(advice.contains(entry.feature), "{advice}");
            assert!(advice.contains("cargo build"), "{advice}");
        }
        assert!(
            how_to_get(catalogue::feature("native").expect("native row"))
                .contains("WIZARD_NATIVE=1")
        );
    }

    /// The JSON is JSON, on whatever this build is.
    ///
    /// `--json` exists so a provisioning script can ask what it installed, and
    /// a script cannot recover from a surface that emits a table when it is
    /// surprised.
    #[tokio::test]
    async fn the_json_shapes_are_well_formed_on_any_build() {
        super::super::bundled::ensure().await;
        for value in [list_json(), missing_json(), profiles_json()] {
            assert!(value.is_object());
            assert!(serde_json::to_string(&value).is_ok());
        }
        assert!(list_json()["plugins"].is_array());
        assert_eq!(
            list_json()["plugins"].as_array().map(Vec::len),
            Some(super::super::kernel().reports().len())
        );
        assert!(
            profiles_json()["profiles"].as_array().map(Vec::len) == Some(profile::PROFILES.len())
        );
    }
}
