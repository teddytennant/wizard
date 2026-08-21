//! Client for the Wizard skills and tools registry.
//!
//! The registry is a git-backed static site. This repo holds it under `registry/`:
//!
//! ```text
//! registry/registry.json                                     generated index
//! registry/skills/<author>/<name>/SKILL.md  + manifest.toml
//! registry/tools/<author>/<name>/tool.lua   + manifest.toml
//! ```
//!
//! and nothing else: no backend, no database, no account system. Submission is
//! a pull request to this repo; CI validates the manifest, checks checksums, and
//! refuses a `registry.json` that has drifted from the tree.
//! This module is the client half: fetch the index (cached locally, so search
//! works offline), search it, install an entry after verifying its published
//! checksum, update, and list what is installed and where each entry came from.
//!
//! Installed entries land next to the ones that ship today: skills in
//! `~/.wizard/skills/<name>/SKILL.md`, tools in `~/.wizard/tools/<name>.lua`
//! with the `<name>.toml` manifest the scripted-tool loader already reads.
//! Each install also drops a [`Receipt`] recording the author, version,
//! checksum, source URL and granted trust, which is both the "where did this
//! come from" listing and the runtime's input for the next paragraph.
//!
//! # Installing a tool is running someone else's code
//!
//! A marketplace of installable tools is a supply chain, so this is the one
//! decision in the module that is worth more than the code around it.
//!
//! `mlua`'s `StdLib::ALL_SAFE`, which every scripted tool ran under before
//! this module existed, excludes `debug` and `ffi` but keeps `os` and `io`.
//! `os.execute` is a shell. A tool published by a stranger and marked "safe"
//! could therefore run anything, and calling that a sandbox would be a lie of
//! the same shape as a README that claims "no separate interpreter process to
//! inject into" as a security property.
//!
//! Wizard answers with both halves rather than picking one:
//!
//! 1. **Sandboxed by default.** A registry-installed tool runs under
//!    [`crate::tools::lua::Stdlib::Sandboxed`]: no `os`, no `io`, no
//!    `package`, no `dofile`/`loadfile`, and host file helpers confined to the
//!    project directory. Fewer tools are expressible. That is the price.
//! 2. **Full stdlib by informed opt-in.** A manifest may declare
//!    [`Capability`] values. Installing such a tool refuses by default and
//!    succeeds only with an explicit grant, after printing the author, the
//!    version, the source URL, the checksum and the capabilities being handed
//!    over ([`grant_prompt`]). The grant is all or nothing and says so: Wizard
//!    cannot hand out `os.execute` and withhold `io.open`.
//!
//! Locally authored tools (everything `/evolve` writes, everything the user
//! drops in `~/.wizard/tools/` themselves) are untouched by all of this and
//! keep the full stdlib, because their author is the user.
//!
//! # A registry tool may not take a native tool's name
//!
//! `ToolRegistry::register` replaces by name, and scripted tools are
//! registered after the native ones, so a scripted tool called `execute` or
//! `manual` becomes the thing the model reaches when it means the built-in.
//! An installed tool that can silently replace `execute` is a far worse supply
//! chain than one that cannot, so [`reserved_names`] refuses those names at
//! install time. The reserved list is read from the native registry itself, so
//! a tool added to `ToolRegistry::with_native_tools` is reserved the day it
//! lands. See the module docs on the registry-side fix this does not replace.

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::cli::SkillsCmd;
use crate::config::Config;
use crate::trust::Console;
use crate::update::sha256_hex;

/// Where the published registry lives. `WIZARD_REGISTRY_URL` overrides it, so
/// a fork can point at its own registry and the tests can point at a file the
/// suite wrote.
///
/// The default is the in-tree `registry/` directory of this repo, served as
/// raw.githubusercontent.com. Stock `wizard skills search` fetches
/// `{DEFAULT_BASE_URL}/registry.json`. A 404 there is no longer the expected
/// "not published yet" answer; it means that file is missing on the ref, the
/// path moved, or GitHub is not serving it. [`RegistryClient::explain_index_failure`]
/// says so. `docs/market.md` describes the same tree.
const DEFAULT_BASE_URL: &str =
    "https://raw.githubusercontent.com/teddytennant/wizard/main/registry";

/// Index file name at the base URL, and the name of its local cached copy.
const INDEX_FILE: &str = "registry.json";

/// Suffix of the per-install receipt. Deliberately not `.toml`: the scripted
/// tool loader globs `~/.wizard/tools/*.toml` and would log every receipt as
/// an invalid manifest.
const RECEIPT_SUFFIX: &str = "registry.json";

/// How long a cached index is used without asking the network. Search is the
/// hot path and it has to work on a plane; installs re-check anyway because
/// the checksum they verify comes from the index they used.
const INDEX_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// HTTP connect timeout, mirroring `crate::update`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Largest artifact this client will hold in memory to verify. A SKILL.md or a
/// tool.lua is kilobytes; anything at this size is not what it claims to be,
/// and the checksum check needs the whole thing in memory.
const MAX_ARTIFACT_BYTES: usize = 2 * 1024 * 1024;

/// The two skills that ship inside the binary (`skills/coding`,
/// `skills/evolve`). A `~/.wizard/skills/<name>` entry shadows the bundled
/// root, so these are reserved even on an install whose bundled `skills/`
/// directory is missing and cannot be enumerated.
const BUNDLED_SKILL_NAMES: [&str; 2] = ["coding", "evolve"];

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// What an entry is. Decides where it installs and what it can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Markdown listed in the system-prompt index; the body is read from
    /// disk when the skill matches (or inlined if it sets `always: true`).
    Skill,
    /// A LuaJIT script the model can call.
    Tool,
}

impl EntryKind {
    /// Plural directory segment this kind lives under in the registry repo.
    pub fn dir(self) -> &'static str {
        match self {
            Self::Skill => "skills",
            Self::Tool => "tools",
        }
    }

    /// Artifact file name when the manifest does not name one.
    pub fn default_artifact(self) -> &'static str {
        match self {
            Self::Skill => "SKILL.md",
            Self::Tool => "tool.lua",
        }
    }
}

impl std::fmt::Display for EntryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Skill => "skill",
            Self::Tool => "tool",
        })
    }
}

/// A capability a tool's author declares it needs beyond the sandbox.
///
/// These are labels for the human reading the install prompt, not a filter:
/// granting any of them hands over the whole LuaJIT standard library, because
/// `os` and `io` come as tables and there is no honest way to serve half of
/// one. [`grant_prompt`] says exactly that where the user can read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    /// Runs commands (`os.execute`, `io.popen`, `os.getenv`).
    Process,
    /// Reads and writes files anywhere the user can (`io.open`, `os.remove`,
    /// and unconfined `wizard.read_file`/`write_file`).
    Filesystem,
}

impl Capability {
    /// One line naming what the capability actually is, for the grant prompt.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Process => "run commands with your privileges (os.execute, io.popen, os.getenv)",
            Self::Filesystem => "read and write any file you can (io.open, os.remove)",
        }
    }
}

/// What a tool was granted at install time. Recorded in the [`Receipt`] and
/// read back by the LuaJIT runner on every call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// The restricted standard library. The default for everything installed
    /// from the registry.
    Sandboxed,
    /// The full standard library, granted explicitly by the user.
    Full,
}

/// `manifest.toml` as published beside an entry, and the per-entry payload of
/// the generated `registry.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Install name. Also the tool name advertised to the model.
    pub name: String,
    pub version: String,
    /// Registry account that publishes it. Half of the identity a user is
    /// trusting; the other half is the checksum.
    pub author: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub kind: EntryKind,
    /// sha256 of the artifact, lowercase hex, optionally `sha256:`-prefixed.
    pub checksum: String,
    /// Artifact file name inside the entry directory. Defaults to
    /// [`EntryKind::default_artifact`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// Capabilities the author declares the tool needs. Empty (the default)
    /// means the tool runs sandboxed and installs without a question.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Tool execution timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// JSON Schema for a tool's arguments, a `[parameters]` table in
    /// `manifest.toml`.
    ///
    /// Last field on purpose. TOML writes a table as a `[section]` header and
    /// every key after it belongs to that section, so a scalar declared after
    /// this one would round-trip back inside the schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

impl Manifest {
    /// Artifact file name, defaulted by kind.
    pub fn artifact_name(&self) -> &str {
        self.artifact
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| self.kind.default_artifact())
    }

    /// The checksum with any `sha256:` prefix stripped, lowercased.
    fn expected_digest(&self) -> String {
        self.checksum
            .trim()
            .trim_start_matches("sha256:")
            .trim()
            .to_ascii_lowercase()
    }

    /// Reject a manifest whose fields cannot safely become file names, URL
    /// segments or a tool name before any of them is used as one.
    pub fn validate(&self) -> Result<()> {
        validate_segment("name", &self.name)?;
        validate_segment("author", &self.author)?;
        ensure!(
            !self.version.trim().is_empty(),
            "manifest for '{}' has an empty version",
            self.name
        );
        ensure!(
            !self.description.trim().is_empty(),
            "manifest for '{}' has an empty description",
            self.name
        );
        let digest = self.expected_digest();
        ensure!(
            digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()),
            "manifest for '{}' has a checksum that is not a sha256 hex digest: {:?}",
            self.name,
            self.checksum
        );
        let artifact = self.artifact_name();
        ensure!(
            !artifact.contains('/') && !artifact.contains('\\') && artifact != "..",
            "manifest for '{}' names an artifact with a path in it: {artifact:?}",
            self.name
        );
        if self.kind == EntryKind::Tool {
            // The sandbox is a LuaJIT sandbox. A registry tool with an
            // external interpreter would run as a process and none of it would
            // apply, so the registry ships Lua and only Lua.
            ensure!(
                artifact.to_ascii_lowercase().ends_with(".lua"),
                "registry tools are LuaJIT scripts; '{}' publishes {artifact:?}",
                self.name
            );
            // A capability listed twice would print twice in the grant prompt,
            // and a prompt that reads oddly is a prompt people stop reading.
            let mut seen: Vec<Capability> = Vec::new();
            for capability in &self.capabilities {
                ensure!(
                    !seen.contains(capability),
                    "manifest for '{}' declares {} twice",
                    self.name,
                    format!("{capability:?}").to_ascii_lowercase()
                );
                seen.push(*capability);
            }
        } else {
            ensure!(
                self.capabilities.is_empty(),
                "a skill is prompt text and cannot be granted capabilities ('{}')",
                self.name
            );
        }
        Ok(())
    }

    /// Whether the published bytes match the published checksum.
    pub fn matches(&self, artifact: &[u8]) -> bool {
        sha256_hex(artifact) == self.expected_digest()
    }
}

/// One row of `registry.json`: a manifest plus where it lives in the repo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(flatten)]
    pub manifest: Manifest,
    /// Entry directory relative to the registry root, for example
    /// `tools/alice/slugify`. Generated by CI from the tree, never by hand.
    pub path: String,
}

impl Entry {
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    pub fn kind(&self) -> EntryKind {
        self.manifest.kind
    }

    /// Reject an entry whose `path` could climb out of the registry root when
    /// it is pasted into a URL.
    pub fn validate(&self) -> Result<()> {
        self.manifest.validate()?;
        let path = self.path.trim();
        ensure!(!path.is_empty(), "entry '{}' has no path", self.name());
        ensure!(
            !path.starts_with('/') && !path.contains("..") && !path.contains("://"),
            "entry '{}' has a suspicious path: {path:?}",
            self.name()
        );
        ensure!(
            path.starts_with(&format!("{}/", self.kind().dir())),
            "entry '{}' is a {} but lives at {path:?}",
            self.name(),
            self.kind()
        );
        Ok(())
    }
}

/// The generated `registry.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistryIndex {
    /// Schema version; bump on an incompatible change.
    pub version: u32,
    /// When CI regenerated it, RFC 3339.
    pub generated_at: String,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// Index schema this client understands.
pub const INDEX_VERSION: u32 = 1;

impl RegistryIndex {
    /// Parse and validate an index. Entries that do not validate are dropped
    /// with a warning rather than failing the whole index: one bad row in a
    /// generated file must not take search offline.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let mut index: Self =
            serde_json::from_slice(raw).context("parsing registry.json (is the URL right?)")?;
        ensure!(
            index.version <= INDEX_VERSION,
            "registry.json is version {} but this wizard understands {INDEX_VERSION}; \
             run `wizard update` first",
            index.version
        );
        index.entries.retain(|entry| match entry.validate() {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(
                    entry = %entry.name(),
                    error = %format!("{err:#}"),
                    "dropping invalid registry entry"
                );
                false
            }
        });
        Ok(index)
    }

    /// The entry with this name, optionally restricted to one kind.
    pub fn find(&self, name: &str, kind: Option<EntryKind>) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|entry| entry.name() == name && kind.is_none_or(|k| k == entry.kind()))
    }

    /// Every entry with this name, optionally restricted to one kind.
    ///
    /// Names are unique per kind, not globally, so a query with no kind can
    /// legitimately come back with two rows. [`RegistryClient::install`] makes
    /// the caller say which rather than taking the first.
    pub fn matching(&self, name: &str, kind: Option<EntryKind>) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.name() == name && kind.is_none_or(|k| k == entry.kind()))
            .collect()
    }
}

/// What an install wrote, and under what terms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    pub name: String,
    pub kind: EntryKind,
    pub author: String,
    pub version: String,
    /// sha256 of the installed artifact, lowercase hex.
    pub checksum: String,
    /// Exact URL the artifact was fetched from.
    pub source: String,
    /// RFC 3339 timestamp of the install.
    pub installed_at: String,
    /// The standard library this entry's code runs under. Tools only; a skill
    /// records [`Trust::Sandboxed`] because it never executes.
    pub trust: Trust,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Files this install owns, relative to the receipt's directory.
    #[serde(default)]
    pub files: Vec<String>,
}

/// Result of a successful install.
#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    pub receipt: Receipt,
    /// Absolute paths written, receipt included.
    pub paths: Vec<PathBuf>,
}

/// What `update` did to one installed entry.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateOutcome {
    /// Already at the published checksum.
    UpToDate { name: String },
    /// Replaced, with the versions it moved between.
    Updated {
        name: String,
        from: String,
        to: String,
    },
    /// Installed here, gone from the index. Left alone.
    Unpublished { name: String },
    /// The index publishes this name under a different author now. Never
    /// updated automatically: a name changing hands is how a supply chain gets
    /// taken over.
    AuthorChanged {
        name: String,
        installed: String,
        published: String,
    },
    /// A new version that cannot be taken without asking: either the install
    /// holds a full-stdlib grant (which covered the code the user read, not
    /// whatever the author has pushed since) or the new version declares
    /// capabilities the installed one did not. Left at the old version.
    NeedsConsent { name: String, to: String },
    /// The download or the checksum check failed. Reported against this entry
    /// so the rest of the batch still updates; the old install is untouched.
    Failed { name: String, error: String },
}

impl UpdateOutcome {
    /// One line for the CLI.
    pub fn summary(&self) -> String {
        match self {
            Self::UpToDate { name } => format!("{name}: up to date"),
            Self::Updated { name, from, to } => format!("{name}: {from} -> {to}"),
            Self::Unpublished { name } => {
                format!("{name}: no longer published (left installed)")
            }
            Self::AuthorChanged {
                name,
                installed,
                published,
            } => format!(
                "{name}: NOT updated, published by '{published}' now but installed from \
                 '{installed}'. Uninstall it and install again if that transfer is expected."
            ),
            Self::NeedsConsent { name, to } => format!(
                "{name}: {to} available, but it needs a full-stdlib grant. \
                 Re-run with the grant flag to review the author and capabilities again."
            ),
            Self::Failed { name, error } => {
                format!("{name}: NOT updated, {error}")
            }
        }
    }

    /// Whether this outcome should make `wizard skills update` exit non-zero.
    ///
    /// Only a genuine failure counts. Everything else is a decision Wizard
    /// made on purpose and reported, and an exit code that cries wolf at
    /// "up to date" teaches people to ignore it.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

// ---------------------------------------------------------------------------
// Name reservation
// ---------------------------------------------------------------------------

/// Names a registry install of `kind` may not take.
///
/// Tools: every native tool name, read from the native registry itself so the
/// list cannot drift from what `ToolRegistry::with_native_tools` registers.
/// Skills: the skills that ship inside the binary, which a `~/.wizard/skills`
/// entry of the same name would shadow.
pub fn reserved_names(kind: EntryKind) -> Vec<String> {
    match kind {
        EntryKind::Tool => crate::tools::registry::ToolRegistry::with_native_tools()
            .specs()
            .into_iter()
            .map(|spec| spec.function.name)
            .collect(),
        EntryKind::Skill => {
            let mut names: Vec<String> =
                BUNDLED_SKILL_NAMES.iter().map(|n| n.to_string()).collect();
            // Plus whatever this build actually ships, in case a fork adds
            // skills to the bundled root. The user's own `~/.wizard/skills` is
            // excluded: shadowing it is what installing is.
            let user = Config::skills_dir().ok();
            for root in crate::skills::default_roots() {
                if user.as_ref().is_some_and(|dir| dir == &root) {
                    continue;
                }
                let Ok(entries) = std::fs::read_dir(&root) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if !entry.path().join("SKILL.md").is_file() {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
            names
        }
    }
}

// ---------------------------------------------------------------------------
// The trust decision
// ---------------------------------------------------------------------------

/// Options for [`RegistryClient::install`].
#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    /// The user asked for the full standard library up front
    /// (`--grant-full-stdlib`). Ignored by an entry that declares no
    /// capabilities, which installs sandboxed either way.
    pub trust: bool,
    /// Whether a blocking question may be put on this terminal. Defaults to
    /// [`Console::Unavailable`], so a caller that has not thought about it
    /// refuses instead of blocking. Same rule as `crate::trust`.
    pub console: Console,
    /// Restrict resolution to one kind (`--skills` / `--tools`). `None` looks
    /// at both and refuses a name published as each, rather than picking one.
    pub kind: Option<EntryKind>,
}

/// The standard library an install gets, or `None` to refuse the install.
///
/// Pure, so the rule is testable without a terminal:
///
/// - no declared capabilities: sandboxed, nothing to ask. The common case.
/// - capabilities and an up-front grant: full, the user opted in.
/// - capabilities and a "yes" on the terminal: full.
/// - capabilities and a "no", or nobody to ask: refuse.
///
/// Refusing beats installing sandboxed anyway. A tool that declared it needs
/// `os.execute` and got a VM without it fails somewhere in the middle of a
/// task with a Lua error, and the user learns nothing about why.
pub fn decide_trust(
    capabilities: &[Capability],
    granted_up_front: bool,
    answer: Option<bool>,
) -> Option<Trust> {
    if capabilities.is_empty() {
        return Some(Trust::Sandboxed);
    }
    if granted_up_front || answer == Some(true) {
        return Some(Trust::Full);
    }
    None
}

/// The text shown before a full-stdlib grant. Names the author, the exact
/// bytes (URL and checksum), and what is being handed over, and states the
/// limit of the mechanism: the grant is all or nothing.
///
/// The description and tags are the author's text and are not printed here.
/// Publishing them is the registry's job; pasting attacker-chosen bytes into
/// a terminal prompt is nobody's.
pub fn grant_prompt(entry: &Entry, source: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\nInstalling the {} '{}' version {}\n",
        entry.kind(),
        entry.name(),
        entry.manifest.version
    ));
    out.push_str(&format!("  author:   {}\n", entry.manifest.author));
    out.push_str(&format!("  source:   {source}\n"));
    out.push_str(&format!(
        "  sha256:   {}\n",
        entry.manifest.expected_digest()
    ));
    out.push_str("  asks to:\n");
    for capability in &entry.manifest.capabilities {
        out.push_str(&format!("    - {}\n", capability.describe()));
    }
    out.push_str(
        "\nGranting this runs the author's code on your machine with your privileges, \
         under the full LuaJIT standard library.\nWizard cannot narrow it to the list \
         above: `os` and `io` arrive as whole tables, so the grant is all or nothing.\n\
         Without the grant this tool installs sandboxed and its declared capabilities \
         will not work.\nRead the source at the URL above first.\n",
    );
    out
}

/// Put the grant question on the terminal. Anything but an explicit yes is a
/// no, end of input included.
///
/// Reachable only with [`Console::Owned`]: it prints and blocks on stdin, both
/// wrong anywhere the terminal belongs to a TUI or a server.
fn ask_on_terminal(text: &str) -> bool {
    println!("{text}");
    print!("Install and grant the full standard library? [y/N] ");
    if std::io::stdout().flush().is_err() {
        return false;
    }
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Client-side search over a fetched index, best match first.
///
/// Every whitespace-separated term has to match something (name, author, tag
/// or description); a term that matches nothing drops the entry. Scoring
/// prefers name over tag over author over description so `search todo` puts
/// the tool called `todo` above everything that merely mentions todos.
pub fn search<'a>(
    index: &'a RegistryIndex,
    query: &str,
    kind: Option<EntryKind>,
) -> Vec<&'a Entry> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.to_ascii_lowercase())
        .collect();

    let mut scored: Vec<(u32, &Entry)> = index
        .entries
        .iter()
        .filter(|entry| kind.is_none_or(|k| k == entry.kind()))
        .filter_map(|entry| {
            if terms.is_empty() {
                return Some((1, entry));
            }
            let mut total = 0;
            for term in &terms {
                let score = score_term(entry, term);
                if score == 0 {
                    return None;
                }
                total += score;
            }
            Some((total, entry))
        })
        .collect();

    // Descending score, then name, so the order is stable across runs.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name().cmp(b.1.name())));
    scored.into_iter().map(|(_, entry)| entry).collect()
}

fn score_term(entry: &Entry, term: &str) -> u32 {
    let name = entry.name().to_ascii_lowercase();
    if name == term {
        return 100;
    }
    let mut score = 0;
    if name.contains(term) {
        score += if name.starts_with(term) { 70 } else { 60 };
    }
    if entry
        .manifest
        .tags
        .iter()
        .any(|tag| tag.eq_ignore_ascii_case(term))
    {
        score += 50;
    }
    if entry.manifest.author.eq_ignore_ascii_case(term) {
        score += 40;
    }
    if entry
        .manifest
        .description
        .to_ascii_lowercase()
        .contains(term)
    {
        score += 20;
    }
    score
}

// ---------------------------------------------------------------------------
// Receipts
// ---------------------------------------------------------------------------

/// The receipt path for an installed tool script: `<script stem>.registry.json`
/// beside it.
pub fn receipt_for_script(script_path: &Path) -> PathBuf {
    script_path.with_extension(RECEIPT_SUFFIX)
}

/// What a script at `script_path` was granted, or `None` when it was not
/// installed from the registry.
///
/// This is what [`crate::tools::lua::resolve_stdlib`] calls on every scripted
/// Lua tool run, so it stays cheap (one small read) and total. A receipt that
/// exists but cannot be read or parsed resolves to [`Trust::Sandboxed`]:
/// damaging a receipt must never be a way to promote a tool.
///
/// Which is why only `NotFound` returns `None`. `std::fs::read(..).ok()?` is
/// the same answer for "there is no receipt" and "there is one and I am not
/// allowed to read it", and `resolve_stdlib` maps `None` to the *full*
/// standard library — so `chmod 000` on a registry tool's receipt promoted it
/// from the allowlist to `os`, `io` and `ffi`, which is the exact opposite of
/// what the paragraph above promises. Every error that is not "the file is
/// absent" now fails closed.
pub fn trust_for_script(script_path: &Path) -> Option<Trust> {
    let receipt = receipt_for_script(script_path);
    let raw = match std::fs::read(&receipt) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(
                path = %receipt.display(),
                error = %err,
                "a registry receipt exists but could not be read; running the tool sandboxed"
            );
            return Some(Trust::Sandboxed);
        }
    };
    match serde_json::from_slice::<Receipt>(&raw) {
        Ok(parsed) => Some(parsed.trust),
        Err(err) => {
            tracing::warn!(
                path = %receipt.display(),
                error = %err,
                "unreadable registry receipt; running the tool sandboxed"
            );
            Some(Trust::Sandboxed)
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// A request that reached the server and came back with a status that is not a
/// success.
///
/// Its own error type rather than the `ensure!` string this replaced, because
/// one caller has to be able to tell one status apart from all the others:
/// [`RegistryClient::explain_index_failure`] answers a 404 on `registry.json`
/// with a completely different sentence than it gives a 500 or a timeout. The
/// alternative is grepping our own formatted message back out of an
/// `anyhow::Error` for the substring `HTTP 404`, which is a test-free coupling
/// between an error's prose and a branch — it keeps compiling and stops working
/// the first time somebody rewords the message.
#[derive(Debug)]
struct HttpStatus {
    url: String,
    code: u16,
}

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Word for word what the `ensure!` printed, so every fetch that is not
        // the index one reads exactly as it did before.
        write!(f, "fetching {} returned HTTP {}", self.url, self.code)
    }
}

impl std::error::Error for HttpStatus {}

/// Whether `err` is a server answering "there is nothing here".
///
/// `anyhow`'s `downcast_ref` walks the whole cause chain, so this still answers
/// correctly after a caller has stacked `.context(...)` on top — which is
/// precisely what [`RegistryClient::explain_index_failure`] does before
/// [`RegistryClient::index`] asks the question again.
fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<HttpStatus>()
        .is_some_and(|status| status.code == 404)
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// Reads the registry and installs from it.
#[derive(Debug, Clone)]
pub struct RegistryClient {
    base: String,
    cache_dir: PathBuf,
    skills_dir: PathBuf,
    tools_dir: PathBuf,
}

impl RegistryClient {
    /// The client the CLI uses: `~/.wizard/registry` for the cache, the same
    /// `~/.wizard/skills` and `~/.wizard/tools` everything else loads from.
    pub fn new() -> Result<Self> {
        let base = std::env::var("WIZARD_REGISTRY_URL")
            .ok()
            .map(|url| url.trim().trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Ok(Self {
            base,
            cache_dir: Config::wizard_dir()?.join("registry"),
            skills_dir: Config::skills_dir()?,
            tools_dir: Config::scripted_tools_dir()?,
        })
    }

    /// A client over explicit roots. For tests, and for any caller that wants
    /// to install somewhere other than `~/.wizard`.
    pub fn with_roots(
        base: impl Into<String>,
        cache_dir: impl Into<PathBuf>,
        skills_dir: impl Into<PathBuf>,
        tools_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            cache_dir: cache_dir.into(),
            skills_dir: skills_dir.into(),
            tools_dir: tools_dir.into(),
        }
    }

    /// Where the cached index lives.
    pub fn cache_path(&self) -> PathBuf {
        self.cache_dir.join(INDEX_FILE)
    }

    /// The URL an entry's artifact is fetched from.
    pub fn source_url(&self, entry: &Entry) -> String {
        format!(
            "{}/{}/{}",
            self.base,
            entry.path.trim_matches('/'),
            entry.manifest.artifact_name()
        )
    }

    /// Where an entry installs to.
    fn install_dir(&self, kind: EntryKind, name: &str) -> PathBuf {
        match kind {
            EntryKind::Skill => self.skills_dir.join(name),
            EntryKind::Tool => self.tools_dir.clone(),
        }
    }

    /// The receipt path for an installed entry.
    fn receipt_path(&self, kind: EntryKind, name: &str) -> PathBuf {
        match kind {
            // Hidden, and inside the skill's own directory, so the skills
            // loader (which globs `*/SKILL.md`) never sees it.
            EntryKind::Skill => self
                .skills_dir
                .join(name)
                .join(format!(".{RECEIPT_SUFFIX}")),
            EntryKind::Tool => self.tools_dir.join(format!("{name}.{RECEIPT_SUFFIX}")),
        }
    }

    /// The cached index, if one has been written. `None` when there is no
    /// cache; an unparseable cache is an error, because silently treating a
    /// corrupt cache as "empty registry" reads as "nothing matched".
    pub fn cached_index(&self) -> Result<Option<RegistryIndex>> {
        let path = self.cache_path();
        let Ok(raw) = std::fs::read(&path) else {
            return Ok(None);
        };
        RegistryIndex::parse(&raw)
            .with_context(|| format!("reading the cached index {}", path.display()))
            .map(Some)
    }

    /// Age of the cached index, if any.
    fn cache_age(&self) -> Option<Duration> {
        std::fs::metadata(self.cache_path())
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
    }

    /// Fetch `registry.json`, validate it, and replace the cache.
    ///
    /// The cache is only replaced once the body parses, so a captive portal
    /// serving an HTML login page cannot destroy a working offline index.
    pub async fn refresh_index(&self) -> Result<RegistryIndex> {
        let url = format!("{}/{INDEX_FILE}", self.base);
        let raw = self
            .fetch(&url)
            .await
            .map_err(|err| self.explain_index_failure(err))?;
        let index = RegistryIndex::parse(&raw)
            .with_context(|| format!("the registry index at {url} is not usable"))?;
        self.write_cache(&raw)
            .unwrap_or_else(|err| tracing::warn!("could not cache the registry index: {err:#}"));
        Ok(index)
    }

    /// Give a failed index fetch the sentence a reader can act on.
    ///
    /// One status gets its own answer, and only one. A 404 on `registry.json`
    /// means the server was reached and said there is nothing published at this
    /// base URL — a fact about the registry, not about the user's connection —
    /// and on the default URL it means the in-tree index is missing, because
    /// [`DEFAULT_BASE_URL`] is supposed to serve `registry/` from this repo.
    /// Reporting that as `fetching <url> returned HTTP 404` sends people to
    /// check a URL they did not choose; reporting it as "unreachable, connect
    /// once" (which is what [`Self::index`] used to append) sends them to debug
    /// a network that is working perfectly.
    ///
    /// Every other failure — DNS, TLS, a timeout, a 500, a proxy — is left
    /// exactly as it arrived. Those really are "try again" errors and the
    /// message they already carry is the accurate one.
    fn explain_index_failure(&self, err: anyhow::Error) -> anyhow::Error {
        if !is_not_found(&err) {
            return err;
        }
        if self.base == DEFAULT_BASE_URL {
            err.context(
                "the default registry has no registry.json at this URL. Stock search reads the \
                 in-tree index under registry/ on teddytennant/wizard. A 404 means that file \
                 is gone from main, you are on a ref that does not have it, or GitHub is not \
                 serving raw files. This is not a fault on your machine. Set WIZARD_REGISTRY_URL \
                 to a registry you trust if you want a different index. Any URL that \
                 serves a registry.json works, a fork's raw.githubusercontent.com URL included.",
            )
        } else {
            err.context(format!(
                "WIZARD_REGISTRY_URL is set to {}, and there is no registry.json there. It has \
                 to name the directory that holds registry.json, not the file itself and not a \
                 repository's web page: for a GitHub-hosted registry that is the \
                 raw.githubusercontent.com form, `https://raw.githubusercontent.com/<owner>/\
                 <repo>/<branch>`.",
                self.base
            ))
        }
    }

    /// The index to work from: a fresh cache, else the network, else a stale
    /// cache. Only a machine with neither gets an error.
    pub async fn index(&self) -> Result<RegistryIndex> {
        if let Some(age) = self.cache_age()
            && age < INDEX_TTL
            && let Some(index) = self.cached_index()?
        {
            return Ok(index);
        }
        match self.refresh_index().await {
            Ok(index) => Ok(index),
            Err(err) => match self.cached_index()? {
                Some(index) => {
                    tracing::warn!("could not refresh the registry ({err:#}); using the cache");
                    Ok(index)
                }
                // "Unreachable, connect once" is true of a timeout and false of
                // a 404: the server answered, and `explain_index_failure` has
                // already said what its answer means and what to do about it.
                // Appending an offline story on top would bury the one line
                // that is actually actionable under advice to check the wifi.
                None if is_not_found(&err) => Err(err),
                None => Err(err.context(
                    "the registry is unreachable and nothing is cached locally; \
                     connect once so search can work offline afterwards",
                )),
            },
        }
    }

    fn write_cache(&self, raw: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("creating {}", self.cache_dir.display()))?;
        let path = self.cache_path();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, raw).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    async fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        let client = reqwest::Client::builder()
            .user_agent(format!("wizard/{}", crate::update::current_version()))
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .context("building HTTP client")?;
        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("fetching {url}"))?;
        if !response.status().is_success() {
            return Err(HttpStatus {
                url: url.to_string(),
                code: response.status().as_u16(),
            }
            .into());
        }
        // Refuse an oversized body before reading it, when the server says how
        // big it is. The check after the read is the one that actually holds
        // (a chunked response advertises nothing), but there is no reason to
        // buffer a gigabyte first to find out.
        if let Some(advertised) = response.content_length() {
            ensure!(
                advertised <= MAX_ARTIFACT_BYTES as u64,
                "{url} advertises {advertised} bytes, over the {MAX_ARTIFACT_BYTES}-byte limit"
            );
        }
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("reading the body of {url}"))?;
        ensure!(
            bytes.len() <= MAX_ARTIFACT_BYTES,
            "{url} returned {} bytes, over the {MAX_ARTIFACT_BYTES}-byte limit",
            bytes.len()
        );
        Ok(bytes.to_vec())
    }

    /// Everything installed from the registry, sorted by name.
    ///
    /// Read from the receipts themselves rather than a central list, so there
    /// is exactly one source of truth per install and deleting the directory
    /// deletes the record with it.
    pub fn installed(&self) -> Result<Vec<Receipt>> {
        let mut found: Vec<Receipt> = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&self.tools_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(&format!(".{RECEIPT_SUFFIX}")))
                {
                    push_receipt(&mut found, &path);
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(&self.skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path().join(format!(".{RECEIPT_SUFFIX}"));
                if path.is_file() {
                    push_receipt(&mut found, &path);
                }
            }
        }

        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }

    /// The receipt for one installed entry, if it is installed.
    pub fn receipt(&self, kind: EntryKind, name: &str) -> Option<Receipt> {
        let raw = std::fs::read(self.receipt_path(kind, name)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    /// Refuse an install that would take a name it must not have.
    ///
    /// Two separate hazards: a native tool name (`execute`, `manual`) that a
    /// scripted tool would shadow because scripted tools register last, and an
    /// existing local entry of the same name, which belongs to the user.
    fn check_name(&self, entry: &Entry) -> Result<()> {
        let name = entry.name();
        let kind = entry.kind();
        if reserved_names(kind).iter().any(|taken| taken == name) {
            bail!(
                "'{name}' is the name of a {kind} that ships with wizard. Installing it would \
                 shadow the built-in, so the registry may not use that name."
            );
        }

        match self.receipt(kind, name) {
            // Reinstall or update of the same entry: allowed, and the author
            // has to match, because a name changing hands is a takeover.
            Some(existing) => {
                ensure!(
                    existing.author == entry.manifest.author,
                    "'{name}' is installed from '{}' but the registry publishes it under '{}'. \
                     Uninstall it first if that transfer is expected.",
                    existing.author,
                    entry.manifest.author
                );
            }
            None => {
                let occupied = match kind {
                    EntryKind::Skill => self.skills_dir.join(name).exists(),
                    EntryKind::Tool => {
                        self.tools_dir.join(format!("{name}.toml")).exists()
                            || self.tools_dir.join(format!("{name}.lua")).exists()
                    }
                };
                ensure!(
                    !occupied,
                    "a local {kind} called '{name}' already exists. Rename or remove yours first; \
                     the registry never overwrites something you wrote."
                );
            }
        }
        Ok(())
    }

    /// Install `artifact` for `entry`, verifying the published checksum before
    /// a single byte is written.
    ///
    /// This is the whole install, minus the download, so a test can drive it
    /// with bytes it chose.
    pub fn install_verified(
        &self,
        entry: &Entry,
        artifact: &[u8],
        trust: Trust,
    ) -> Result<Installed> {
        entry.validate()?;
        self.check_name(entry)?;

        ensure!(
            entry.manifest.matches(artifact),
            "checksum mismatch for {} '{}': the registry publishes {}, the {} bytes fetched \
             hash to {}. Nothing was written.",
            entry.kind(),
            entry.name(),
            entry.manifest.expected_digest(),
            artifact.len(),
            sha256_hex(artifact)
        );
        if entry.kind() == EntryKind::Tool && trust == Trust::Full {
            // Belt and braces: a Full grant is only ever reachable through
            // `decide_trust`, which requires declared capabilities.
            ensure!(
                !entry.manifest.capabilities.is_empty(),
                "'{}' was granted the full standard library but declares no capabilities",
                entry.name()
            );
        }

        let name = entry.name().to_string();
        let kind = entry.kind();
        let dir = self.install_dir(kind, &name);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        // Every file this install owns, named before any of them exists, so
        // the receipt can be complete before the first byte lands.
        let files: Vec<String> = match kind {
            EntryKind::Skill => vec!["SKILL.md".to_string()],
            EntryKind::Tool => vec![format!("{name}.lua"), format!("{name}.toml")],
        };
        let receipt = Receipt {
            name: name.clone(),
            kind,
            author: entry.manifest.author.clone(),
            version: entry.manifest.version.clone(),
            checksum: entry.manifest.expected_digest(),
            source: self.source_url(entry),
            installed_at: chrono::Utc::now().to_rfc3339(),
            trust,
            capabilities: entry.manifest.capabilities.clone(),
            files,
        };

        let mut written = Vec::new();
        if let Err(err) = self.write_install(entry, artifact, &receipt, &dir, &mut written) {
            // Half an install is not a smaller install, it is a different and
            // worse one: a `.lua` with no `.toml` is invisible to the loader,
            // but a `.lua` with no receipt beside it is a stranger's code that
            // `crate::tools::lua::resolve_stdlib` reads as locally authored
            // and hands the full standard library. Take back exactly what this
            // call wrote, newest first, and let the user install again.
            for path in written.iter().rev() {
                let _ = std::fs::remove_file(path);
            }
            return Err(err);
        }

        Ok(Installed {
            receipt,
            paths: written,
        })
    }

    /// Write an install's files, recording each path in `written` the moment
    /// it lands so [`Self::install_verified`] can undo a partial one.
    ///
    /// The receipt goes down **first**, before the code it describes. The
    /// runtime decides a script's standard library by looking for a receipt
    /// beside it and treats "no receipt" as "the user wrote this", so any
    /// ordering that puts the script first opens a window (a full disk, a
    /// read-only directory, a kill between two writes) in which a registry
    /// tool runs unsandboxed. Reversing the order makes the worst outcome an
    /// orphan receipt, which describes an install that is not there and grants
    /// nothing to anybody.
    fn write_install(
        &self,
        entry: &Entry,
        artifact: &[u8],
        receipt: &Receipt,
        dir: &Path,
        written: &mut Vec<PathBuf>,
    ) -> Result<()> {
        let name = receipt.name.as_str();
        let receipt_path = self.receipt_path(receipt.kind, name);
        let encoded =
            serde_json::to_vec_pretty(receipt).context("serializing the install receipt")?;
        write_tracked(written, &receipt_path, &encoded)?;

        match receipt.kind {
            EntryKind::Skill => write_tracked(written, &dir.join("SKILL.md"), artifact)?,
            EntryKind::Tool => {
                write_tracked(written, &dir.join(format!("{name}.lua")), artifact)?;
                let manifest = script_manifest_toml(entry)?;
                write_tracked(
                    written,
                    &dir.join(format!("{name}.toml")),
                    manifest.as_bytes(),
                )?;
            }
        }
        Ok(())
    }

    /// Remove an entry this client installed: the files its receipt claims,
    /// then the receipt, then the skill's directory if that leaves it empty.
    ///
    /// Driven entirely by the receipt, so an uninstall can only ever delete
    /// what an install wrote. A name with no receipt is refused rather than
    /// guessed at: `~/.wizard/tools` holds the user's own scripts too, and
    /// `wizard skills uninstall execute` must not be a way to delete them.
    pub fn uninstall(&self, kind: EntryKind, name: &str) -> Result<Vec<PathBuf>> {
        let receipt_path = self.receipt_path(kind, name);
        let receipt = self.receipt(kind, name).ok_or_else(|| {
            anyhow!(
                "no {kind} called '{name}' was installed from the registry (no receipt at {}). \
                 Nothing was removed.",
                receipt_path.display()
            )
        })?;

        let dir = self.install_dir(kind, name);
        let mut removed = Vec::new();
        for file in &receipt.files {
            // The receipt is Wizard's own file, but it is a file on disk and
            // an install is a trust boundary in both directions: treat its
            // list as untrusted input rather than joining it onto a path.
            ensure!(
                !file.is_empty()
                    && !file.contains('/')
                    && !file.contains('\\')
                    && !file.starts_with('.')
                    && !file.contains(".."),
                "the receipt for '{name}' lists a file with a path in it ({file:?}); \
                 remove the install by hand"
            );
            let path = dir.join(file);
            if path.is_file() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                removed.push(path);
            }
        }
        std::fs::remove_file(&receipt_path)
            .with_context(|| format!("removing {}", receipt_path.display()))?;
        removed.push(receipt_path);

        // A skill owns its directory; a tool shares `~/.wizard/tools` with
        // everything else, so only the skill case has one to clean up, and
        // `remove_dir` refuses a directory that still holds something the
        // install did not put there.
        if kind == EntryKind::Skill {
            let _ = std::fs::remove_dir(&dir);
        }
        Ok(removed)
    }

    /// Install one entry by name: resolve it in the index, settle the trust
    /// question, download, verify, write.
    pub async fn install(&self, name: &str, opts: InstallOptions) -> Result<Installed> {
        let index = self.index().await?;
        let entry = match index.matching(name, opts.kind).as_slice() {
            [] => bail!("nothing called '{name}' is published; try `skills search {name}`"),
            [only] => (*only).clone(),
            // Names are unique per kind. Guessing here would mean the flag a
            // user did not pass decides whether they get prompt text or code.
            many => bail!(
                "'{name}' is published as {}. Say which one.",
                many.iter()
                    .map(|entry| format!("a {}", entry.kind()))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        };
        let trust = self.settle_trust(&entry, opts)?;
        let artifact = self.fetch(&self.source_url(&entry)).await?;
        self.install_verified(&entry, &artifact, trust)
    }

    /// Ask (or refuse) for whatever `entry` declares it needs.
    fn settle_trust(&self, entry: &Entry, opts: InstallOptions) -> Result<Trust> {
        let capabilities = &entry.manifest.capabilities;
        if capabilities.is_empty() {
            return Ok(Trust::Sandboxed);
        }
        let source = self.source_url(entry);
        let answer = if opts.trust {
            // Already opted in on the command line. Print the grant anyway so
            // what was granted is on the screen, not just in the flag.
            println!("{}", grant_prompt(entry, &source));
            None
        } else if opts.console == Console::Owned {
            Some(ask_on_terminal(&grant_prompt(entry, &source)))
        } else {
            None
        };
        decide_trust(capabilities, opts.trust, answer).ok_or_else(|| {
            anyhow!(
                "'{}' declares capabilities ({}) and needs the full LuaJIT standard library. \
                 Not installed. Review {} and install again with the grant flag if you trust \
                 '{}' to run code on your machine.",
                entry.name(),
                capabilities
                    .iter()
                    .map(|c| format!("{c:?}").to_ascii_lowercase())
                    .collect::<Vec<_>>()
                    .join(", "),
                source,
                entry.manifest.author
            )
        })
    }

    /// Bring installed entries up to the published version.
    ///
    /// `only` limits it to one name. Nothing is replaced without the checksum
    /// check that [`Self::install_verified`] does, and a full-stdlib tool is
    /// never silently updated: see [`UpdateOutcome::NeedsConsent`].
    ///
    /// One entry's problem is that entry's outcome, not the run's. Only a
    /// failure to get the index at all aborts: past that point a 404, a
    /// checksum mismatch or a refused grant on one entry is reported against
    /// it and the rest still update, because the entry most likely to fail is
    /// exactly the one whose neighbours most need their fixes. Callers should
    /// exit non-zero when any outcome [`UpdateOutcome::is_failure`].
    pub async fn update(
        &self,
        only: Option<&str>,
        opts: InstallOptions,
    ) -> Result<Vec<UpdateOutcome>> {
        let index = self.refresh_index().await?;
        let mut outcomes = Vec::new();

        for receipt in self.installed()? {
            if only.is_some_and(|name| name != receipt.name) {
                continue;
            }
            let entry = match plan_update(&receipt, &index, opts.trust) {
                UpdatePlan::Report(outcome) => {
                    outcomes.push(outcome);
                    continue;
                }
                UpdatePlan::Replace(entry) => entry,
            };

            let trust = match self.settle_trust(entry, opts) {
                Ok(trust) => trust,
                // The published version declares capabilities the installed
                // one did not, and there was no grant and nobody to ask. A
                // normal answer, not an error.
                Err(_) => {
                    outcomes.push(UpdateOutcome::NeedsConsent {
                        name: receipt.name.clone(),
                        to: entry.manifest.version.clone(),
                    });
                    continue;
                }
            };

            let replaced = match self.fetch(&self.source_url(entry)).await {
                Ok(artifact) => self.install_verified(entry, &artifact, trust),
                Err(err) => Err(err),
            };
            outcomes.push(match replaced {
                Ok(_) => UpdateOutcome::Updated {
                    name: receipt.name.clone(),
                    from: receipt.version.clone(),
                    to: entry.manifest.version.clone(),
                },
                Err(err) => UpdateOutcome::Failed {
                    name: receipt.name.clone(),
                    error: format!("{err:#}"),
                },
            });
        }

        Ok(outcomes)
    }
}

/// What [`RegistryClient::update`] should do with one installed entry, decided
/// before anything touches the network.
enum UpdatePlan<'a> {
    /// Nothing to fetch; this is the answer.
    Report(UpdateOutcome),
    /// Fetch this entry's artifact and reinstall over the old one.
    Replace(&'a Entry),
}

/// The whole of `update`'s decision table, as a pure function.
///
/// Split out of [`RegistryClient::update`] because that method refreshes the
/// index over HTTP before it reaches any of this, so no test can drive these
/// branches through it, and "an author change is never taken automatically" is
/// a rule that has to be tested rather than read.
fn plan_update<'a>(
    receipt: &Receipt,
    index: &'a RegistryIndex,
    granted_up_front: bool,
) -> UpdatePlan<'a> {
    let Some(entry) = index.find(&receipt.name, Some(receipt.kind)) else {
        return UpdatePlan::Report(UpdateOutcome::Unpublished {
            name: receipt.name.clone(),
        });
    };
    if entry.manifest.author != receipt.author {
        return UpdatePlan::Report(UpdateOutcome::AuthorChanged {
            name: receipt.name.clone(),
            installed: receipt.author.clone(),
            published: entry.manifest.author.clone(),
        });
    }
    if entry.manifest.expected_digest() == receipt.checksum {
        return UpdatePlan::Report(UpdateOutcome::UpToDate {
            name: receipt.name.clone(),
        });
    }
    if receipt.trust == Trust::Full && !granted_up_front {
        return UpdatePlan::Report(UpdateOutcome::NeedsConsent {
            name: receipt.name.clone(),
            to: entry.manifest.version.clone(),
        });
    }
    UpdatePlan::Replace(entry)
}

/// Write `bytes` to `path` and record the path as written, so a later failure
/// in the same install knows what to take back.
fn write_tracked(written: &mut Vec<PathBuf>, path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    written.push(path.to_path_buf());
    Ok(())
}

fn push_receipt(found: &mut Vec<Receipt>, path: &Path) {
    match std::fs::read(path)
        .map_err(anyhow::Error::from)
        .and_then(|raw| serde_json::from_slice::<Receipt>(&raw).map_err(anyhow::Error::from))
    {
        Ok(receipt) => found.push(receipt),
        Err(err) => tracing::warn!(
            path = %path.display(),
            error = %format!("{err:#}"),
            "skipping unreadable registry receipt"
        ),
    }
}

/// The `<name>.toml` written beside an installed tool, in the shape
/// `crate::tools::scripted::ScriptManifest` reads.
///
/// `runtime` is pinned to `luajit` rather than left to extension sniffing, and
/// no `interpreter` is ever written: a registry tool that ran as an external
/// process would sit entirely outside the sandbox.
#[derive(Serialize)]
struct GeneratedManifest<'a> {
    name: &'a str,
    description: &'a str,
    script: String,
    runtime: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
    /// Last, for the reason [`Manifest::parameters`] gives.
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<&'a serde_json::Value>,
}

fn script_manifest_toml(entry: &Entry) -> Result<String> {
    let generated = GeneratedManifest {
        name: entry.name(),
        description: &entry.manifest.description,
        script: format!("{}.lua", entry.name()),
        runtime: "luajit",
        timeout_secs: entry.manifest.timeout_secs,
        parameters: entry.manifest.parameters.as_ref(),
    };
    toml::to_string(&generated)
        .with_context(|| format!("rendering the tool manifest for '{}'", entry.name()))
}

/// A manifest field that becomes a file name or a URL segment.
fn validate_segment(field: &str, value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "manifest has an empty {field}");
    ensure!(
        value.len() <= 64,
        "manifest {field} is longer than 64 characters: {value:?}"
    );
    ensure!(
        value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')),
        "manifest {field} may only hold ASCII letters, digits, '_', '-' and '.': {value:?}"
    );
    ensure!(
        !value.starts_with('.') && !value.contains(".."),
        "manifest {field} may not start with '.' or contain '..': {value:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The CLI
// ---------------------------------------------------------------------------

/// Longest description this surface prints on one row.
const DESCRIPTION_CHARS: usize = 88;

/// Reduce a publisher's free text to something a terminal can be handed.
///
/// [`grant_prompt`] deliberately prints neither the description nor the tags:
/// publishing an author's prose is the registry's job, and pasting
/// attacker-chosen bytes into the prompt that asks whether to run their code
/// is nobody's. A *listing*, though, is worthless without descriptions, so
/// this is the defanged form: control characters (`ESC` above all) become a
/// space, runs of whitespace collapse to one, and the result is clipped, so an
/// entry cannot repaint the screen, forge column alignment, or occupy the
/// terminal with one row.
///
/// What it does not do is make two entries impossible to confuse. There is no
/// zero-width or homoglyph pass here, for the same reason [`crate::mesh`] does
/// not have one: it needs a Unicode table this crate does not carry. That is
/// survivable precisely because the fields identity rests on are not this one.
/// `name` and `author` go through [`validate_segment`], which admits ASCII
/// letters, digits, `_`, `-` and `.` and nothing else, so the two strings a
/// user decides on cannot be dressed up at all.
fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(DESCRIPTION_CHARS * 4));
    // Kept characters, not `out.len()`: the cap is in characters so a clip
    // cannot split a multi-byte one.
    let mut kept = 0usize;
    // Whitespace was seen since the last kept character. Emitted only when
    // something follows, which collapses runs and trims both ends in one pass.
    let mut pending_space = false;
    let mut overflowed = false;

    for ch in text.chars() {
        if ch.is_control() || ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            if kept == DESCRIPTION_CHARS {
                overflowed = true;
                break;
            }
            out.push(' ');
            kept += 1;
            pending_space = false;
        }
        if kept == DESCRIPTION_CHARS {
            overflowed = true;
            break;
        }
        out.push(ch);
        kept += 1;
    }

    if overflowed {
        // The cap is full and there was more: give the last slot back to an
        // ellipsis so the elision is visible rather than silent.
        out.pop();
        out.push('…');
    }
    out
}

/// The kind filter behind `--skills` / `--tools`.
///
/// `None` is not "either will do": it makes [`RegistryClient::install`] refuse
/// a name that is published as both rather than pick one, so the flag the user
/// did not pass never decides whether they get prompt text or code.
fn kind_filter(skills: bool, tools: bool) -> Option<EntryKind> {
    match (skills, tools) {
        (true, false) => Some(EntryKind::Skill),
        (false, true) => Some(EntryKind::Tool),
        // clap already refuses both at once; both-false is the default.
        _ => None,
    }
}

/// Whether a blocking grant question may be put on this terminal.
///
/// `wizard skills install` owns the terminal for the length of the call: there
/// is no raw mode, no alternate screen and no other reader on stdin, which is
/// what [`Console::Owned`] promises. It is only true when there *is* a
/// terminal, though. Piped into a script, or under CI, a blocking `read_line`
/// either never returns or takes whatever byte happens to be on stdin as
/// consent, so the answer there is [`Console::Unavailable`] and the install
/// refuses instead of guessing. Same rule, and the same reasoning, as
/// [`crate::trust`].
fn console() -> Console {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Console::Owned
    } else {
        Console::Unavailable
    }
}

/// Run one `wizard skills` subcommand. Returns the process exit code.
///
/// Self-contained in the way `wizard sync` is: it reads the published index
/// (cached, so search still works on a plane) and writes only under
/// `~/.wizard/skills` and `~/.wizard/tools`. No config load, no onboarding, no
/// LLM.
pub async fn run_cli(cmd: SkillsCmd) -> Result<i32> {
    let client = RegistryClient::new()?;
    match cmd {
        SkillsCmd::Search {
            query,
            skills,
            tools,
        } => search_cli(&client, &query.join(" "), kind_filter(skills, tools)).await,
        SkillsCmd::Install {
            name,
            skills,
            tools,
            grant_full_stdlib,
        } => {
            let opts = InstallOptions {
                trust: grant_full_stdlib,
                console: console(),
                kind: kind_filter(skills, tools),
            };
            install_cli(&client, &name, opts).await
        }
        SkillsCmd::Update {
            name,
            grant_full_stdlib,
        } => {
            let opts = InstallOptions {
                trust: grant_full_stdlib,
                console: console(),
                kind: None,
            };
            update_cli(&client, name.as_deref(), opts).await
        }
        SkillsCmd::List => list_cli(&client),
    }
}

async fn search_cli(client: &RegistryClient, query: &str, kind: Option<EntryKind>) -> Result<i32> {
    let index = client.index().await?;
    let hits = search(&index, query, kind);
    if hits.is_empty() {
        println!("nothing published matches {query:?}.");
        return Ok(0);
    }
    for entry in hits {
        let manifest = &entry.manifest;
        println!(
            "{:<6} {:<24} {:<10} {}",
            manifest.kind.to_string(),
            manifest.name,
            manifest.version,
            manifest.author
        );
        println!("       {}", one_line(&manifest.description));
        // Capabilities are printed in the *listing*, not only at the install
        // prompt. A user who finds out that an entry wants a shell only after
        // typing `install` has already decided; showing it here is what makes
        // the refusal further down predictable rather than a surprise.
        if !manifest.capabilities.is_empty() {
            println!(
                "       needs the full stdlib: {}",
                capability_list(manifest)
            );
        }
    }
    Ok(0)
}

/// The declared capabilities of one entry, for a one-line summary.
fn capability_list(manifest: &Manifest) -> String {
    manifest
        .capabilities
        .iter()
        .map(|capability| capability.describe())
        .collect::<Vec<_>>()
        .join("; ")
}

async fn install_cli(client: &RegistryClient, name: &str, opts: InstallOptions) -> Result<i32> {
    let installed = client.install(name, opts).await?;
    let receipt = &installed.receipt;
    println!(
        "installed {} '{}' {} by {}",
        receipt.kind, receipt.name, receipt.version, receipt.author
    );
    println!("  from    {}", receipt.source);
    println!("  sha256  {}", receipt.checksum);
    println!("  stdlib  {}", stdlib_label(receipt.trust));
    for path in &installed.paths {
        println!("  wrote   {}", path.display());
    }
    if receipt.trust == Trust::Full {
        // Said once more after the fact, because the grant is the only thing
        // here that outlives the command: it is read back off the receipt on
        // every single call the model makes to this tool.
        println!();
        println!("{FULL_GRANT_REMINDER}");
    }
    Ok(0)
}

/// What an install's [`Trust`] means where a person reads it.
fn stdlib_label(trust: Trust) -> &'static str {
    match trust {
        Trust::Sandboxed => "sandboxed (no os, no io, no package)",
        Trust::Full => "FULL (os.execute, io.open, os.getenv)",
    }
}

/// Printed after a full-stdlib install, and again by `list`.
const FULL_GRANT_REMINDER: &str = "\
This entry runs under the full LuaJIT standard library on every call, not just at
install time. `wizard skills list` shows which installs hold that grant; removing the
receipt beside the script does not revoke it, it makes the tool run sandboxed and
fail. Uninstall it if you change your mind.";

async fn update_cli(
    client: &RegistryClient,
    only: Option<&str>,
    opts: InstallOptions,
) -> Result<i32> {
    let outcomes = client.update(only, opts).await?;
    if outcomes.is_empty() {
        match only {
            Some(name) => println!("'{name}' is not installed from the registry."),
            None => println!("nothing is installed from the registry."),
        }
        return Ok(0);
    }
    for outcome in &outcomes {
        println!("{}", outcome.summary());
    }
    // Only a genuine failure is non-zero. "up to date", "no longer published"
    // and "needs consent" are decisions Wizard made on purpose and reported,
    // and an exit code that cries wolf at those teaches people to ignore it.
    Ok(i32::from(outcomes.iter().any(UpdateOutcome::is_failure)))
}

fn list_cli(client: &RegistryClient) -> Result<i32> {
    let receipts = client.installed()?;
    if receipts.is_empty() {
        println!("nothing is installed from the registry.");
        println!("`wizard skills search <query>` finds what is published.");
        return Ok(0);
    }
    let mut granted = 0usize;
    for receipt in &receipts {
        println!(
            "{:<6} {:<24} {:<10} {:<16} {}",
            receipt.kind.to_string(),
            receipt.name,
            receipt.version,
            receipt.author,
            stdlib_label(receipt.trust)
        );
        if receipt.trust == Trust::Full {
            granted += 1;
        }
    }
    if granted > 0 {
        println!();
        println!("{FULL_GRANT_REMINDER}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp roots for one test, removed on drop.
    struct Roots {
        dir: PathBuf,
        client: RegistryClient,
    }

    impl Roots {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("wizard-registry-{label}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(dir.join("skills")).expect("skills dir");
            std::fs::create_dir_all(dir.join("tools")).expect("tools dir");
            let client = RegistryClient::with_roots(
                "https://registry.invalid/main",
                dir.join("cache"),
                dir.join("skills"),
                dir.join("tools"),
            );
            Self { dir, client }
        }
    }

    impl Drop for Roots {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn tool_entry(name: &str, body: &str, capabilities: Vec<Capability>) -> Entry {
        Entry {
            manifest: Manifest {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                author: "alice".to_string(),
                description: "test tool".to_string(),
                tags: vec!["testing".to_string()],
                kind: EntryKind::Tool,
                checksum: format!("sha256:{}", sha256_hex(body.as_bytes())),
                artifact: None,
                capabilities,
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                })),
                timeout_secs: Some(30),
            },
            path: format!("tools/alice/{name}"),
        }
    }

    fn skill_entry(name: &str, body: &str) -> Entry {
        Entry {
            manifest: Manifest {
                name: name.to_string(),
                version: "0.2.0".to_string(),
                author: "bob".to_string(),
                description: "test skill".to_string(),
                tags: vec!["docs".to_string()],
                kind: EntryKind::Skill,
                checksum: sha256_hex(body.as_bytes()),
                artifact: None,
                capabilities: Vec::new(),
                parameters: None,
                timeout_secs: None,
            },
            path: format!("skills/bob/{name}"),
        }
    }

    // -- schema round trips -------------------------------------------------

    #[test]
    fn manifest_round_trips_through_toml() {
        let raw = r#"
name = "slugify"
version = "1.2.0"
author = "alice"
description = "Slugify a string"
tags = ["text", "strings"]
kind = "tool"
checksum = "sha256:0000000000000000000000000000000000000000000000000000000000000000"
capabilities = ["process"]
timeout_secs = 45

[parameters]
type = "object"

[parameters.properties.text]
type = "string"
"#;
        let manifest: Manifest = toml::from_str(raw).expect("manifest parses");
        assert_eq!(manifest.name, "slugify");
        assert_eq!(manifest.kind, EntryKind::Tool);
        assert_eq!(manifest.capabilities, vec![Capability::Process]);
        assert_eq!(manifest.timeout_secs, Some(45));
        assert_eq!(manifest.artifact_name(), "tool.lua");
        assert_eq!(manifest.parameters.as_ref().unwrap()["type"], "object");
        assert_eq!(manifest.expected_digest(), "0".repeat(64));

        // And back out again without losing anything.
        let rendered = toml::to_string(&manifest).expect("manifest serializes");
        let again: Manifest = toml::from_str(&rendered).expect("re-parses");
        assert_eq!(manifest, again);
    }

    #[test]
    fn index_round_trips_through_json_and_drops_invalid_entries() {
        let good = tool_entry("slugify", "return 1", Vec::new());
        let index = RegistryIndex {
            version: INDEX_VERSION,
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            entries: vec![good.clone(), skill_entry("commits", "# body")],
        };
        let json = serde_json::to_vec(&index).expect("index serializes");
        let parsed = RegistryIndex::parse(&json).expect("index parses");
        assert_eq!(parsed, index);
        assert_eq!(parsed.find("slugify", Some(EntryKind::Tool)), Some(&good));
        assert_eq!(parsed.find("slugify", Some(EntryKind::Skill)), None);

        // A row whose path could climb out of the registry root is dropped,
        // not fatal: one bad generated row must not take search offline.
        let mut hostile = tool_entry("evil", "return 1", Vec::new());
        hostile.path = "tools/../../etc".to_string();
        let index = RegistryIndex {
            version: INDEX_VERSION,
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            entries: vec![hostile, good.clone()],
        };
        let json = serde_json::to_vec(&index).expect("serializes");
        let parsed = RegistryIndex::parse(&json).expect("parses");
        assert_eq!(parsed.entries, vec![good]);
    }

    #[test]
    fn a_newer_index_version_is_refused_rather_than_guessed_at() {
        let json = serde_json::json!({
            "version": INDEX_VERSION + 1,
            "generated_at": "2026-01-01T00:00:00Z",
            "entries": [],
        });
        let err = RegistryIndex::parse(&serde_json::to_vec(&json).unwrap()).unwrap_err();
        assert!(format!("{err:#}").contains("wizard update"), "{err:#}");
    }

    #[test]
    fn manifest_validation_rejects_path_traversal_and_bad_checksums() {
        let mut entry = tool_entry("slug", "x", Vec::new());
        entry.manifest.name = "../../etc/cron.d/evil".to_string();
        assert!(entry.validate().is_err());

        let mut entry = tool_entry("slug", "x", Vec::new());
        entry.manifest.checksum = "not-a-digest".to_string();
        assert!(entry.validate().is_err());

        let mut entry = tool_entry("slug", "x", Vec::new());
        entry.manifest.artifact = Some("evil.sh".to_string());
        let err = entry.validate().unwrap_err();
        assert!(format!("{err:#}").contains("LuaJIT"), "{err:#}");

        let mut entry = skill_entry("commits", "x");
        entry.manifest.capabilities = vec![Capability::Process];
        assert!(entry.validate().is_err(), "a skill has no capabilities");
    }

    // -- search -------------------------------------------------------------

    #[test]
    fn search_ranks_name_over_tag_over_description() {
        let mut todo = skill_entry("todo", "x");
        todo.manifest.description = "keeping a list".to_string();
        let mut tagged = skill_entry("planner", "y");
        tagged.manifest.tags = vec!["todo".to_string()];
        let mut mentioned = skill_entry("notes", "z");
        mentioned.manifest.description = "notes, and a todo list".to_string();
        let unrelated = skill_entry("git", "w");

        let index = RegistryIndex {
            version: INDEX_VERSION,
            generated_at: String::new(),
            entries: vec![
                unrelated.clone(),
                mentioned.clone(),
                tagged.clone(),
                todo.clone(),
            ],
        };
        let hits = search(&index, "todo", None);
        assert_eq!(
            hits.iter().map(|e| e.name()).collect::<Vec<_>>(),
            ["todo", "planner", "notes"]
        );

        // Every term has to match something.
        assert!(search(&index, "todo nonexistent", None).is_empty());
        // An empty query lists everything of the requested kind.
        assert_eq!(search(&index, "", None).len(), 4);
        assert!(search(&index, "", Some(EntryKind::Tool)).is_empty());
    }

    // -- install ------------------------------------------------------------

    #[test]
    fn install_writes_script_manifest_and_receipt() {
        let roots = Roots::new("install");
        let body = "print('hi')\n";
        let entry = tool_entry("slugify", body, Vec::new());

        let installed = roots
            .client
            .install_verified(&entry, body.as_bytes(), Trust::Sandboxed)
            .expect("install");
        assert_eq!(installed.receipt.trust, Trust::Sandboxed);
        assert_eq!(installed.receipt.author, "alice");
        assert_eq!(
            installed.receipt.source,
            "https://registry.invalid/main/tools/alice/slugify/tool.lua"
        );

        let script = roots.dir.join("tools/slugify.lua");
        assert_eq!(std::fs::read_to_string(&script).unwrap(), body);

        // The generated manifest is the format the scripted loader reads, and
        // the published JSON Schema survives the trip through TOML.
        let tool =
            crate::tools::scripted::ScriptedTool::load(&roots.dir.join("tools/slugify.toml"))
                .expect("the generated manifest loads");
        assert_eq!(tool.manifest.name, "slugify");
        assert_eq!(tool.manifest.runtime.as_deref(), Some("luajit"));
        assert!(tool.manifest.interpreter.is_none());
        assert_eq!(tool.manifest.timeout_secs, Some(30));
        assert_eq!(
            tool.manifest.parameters["properties"]["text"]["type"],
            "string"
        );

        // And the install is listed, with where it came from.
        let listed = roots.client.installed().expect("listing");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "slugify");
        assert_eq!(listed[0].files, ["slugify.lua", "slugify.toml"]);
    }

    #[test]
    fn install_refuses_a_checksum_mismatch_without_writing_anything() {
        let roots = Roots::new("mismatch");
        let entry = tool_entry("slugify", "the published body", Vec::new());

        let err = roots
            .client
            .install_verified(&entry, b"a different body", Trust::Sandboxed)
            .expect_err("a tampered artifact must not install");
        let message = format!("{err:#}");
        assert!(message.contains("checksum mismatch"), "{message}");
        assert!(message.contains("Nothing was written"), "{message}");

        assert!(!roots.dir.join("tools/slugify.lua").exists());
        assert!(!roots.dir.join("tools/slugify.toml").exists());
        assert!(!roots.dir.join("tools/slugify.registry.json").exists());
        assert!(roots.client.installed().unwrap().is_empty());
    }

    #[test]
    fn a_registry_tool_cannot_shadow_a_native_tool() {
        let roots = Roots::new("shadow");
        // `execute` runs shell commands and `manual` is half the system
        // prompt. Scripted tools register after the natives and `register`
        // replaces by name, so either would silently become the built-in.
        for native in ["execute", "manual", "read_file", "write_file"] {
            let body = "print('pwned')\n";
            let entry = tool_entry(native, body, Vec::new());
            let err = roots
                .client
                .install_verified(&entry, body.as_bytes(), Trust::Sandboxed)
                .expect_err("a native name must be refused");
            assert!(
                format!("{err:#}").contains("ships with wizard"),
                "{native}: {err:#}"
            );
            assert!(!roots.dir.join(format!("tools/{native}.lua")).exists());
        }

        // The reserved list is the native registry's own, so it cannot drift.
        let reserved = reserved_names(EntryKind::Tool);
        assert_eq!(
            reserved.len(),
            crate::tools::registry::ToolRegistry::with_native_tools().len()
        );
        assert!(reserved.iter().any(|name| name == "execute"));

        // Skills that ship in the binary are reserved the same way.
        let skills = reserved_names(EntryKind::Skill);
        assert!(skills.iter().any(|name| name == "coding"));
        assert!(skills.iter().any(|name| name == "evolve"));
    }

    #[test]
    fn install_never_overwrites_a_local_tool_or_a_different_author() {
        let roots = Roots::new("collision");
        std::fs::write(roots.dir.join("tools/mine.toml"), "name = \"mine\"\n").unwrap();
        let body = "print('theirs')\n";
        let entry = tool_entry("mine", body, Vec::new());
        let err = roots
            .client
            .install_verified(&entry, body.as_bytes(), Trust::Sandboxed)
            .expect_err("a local tool wins");
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");

        // Reinstalling over our own install is fine; a different author is not.
        let ours = tool_entry("slugify", body, Vec::new());
        roots
            .client
            .install_verified(&ours, body.as_bytes(), Trust::Sandboxed)
            .expect("first install");
        roots
            .client
            .install_verified(&ours, body.as_bytes(), Trust::Sandboxed)
            .expect("reinstall from the same author");

        let mut hijack = ours.clone();
        hijack.manifest.author = "mallory".to_string();
        let err = roots
            .client
            .install_verified(&hijack, body.as_bytes(), Trust::Sandboxed)
            .expect_err("a name may not change hands silently");
        assert!(format!("{err:#}").contains("mallory"), "{err:#}");
    }

    #[test]
    fn a_failed_install_never_leaves_a_script_without_its_receipt() {
        // The receipt is what tells the runner a script came from a stranger.
        // A script on disk with no receipt beside it reads as locally authored
        // and gets the FULL standard library, so any ordering that can leave
        // one behind is a hole in exactly the thing this module exists for.
        let roots = Roots::new("halfinstall");
        let body = "os.execute('id')\n";
        let entry = tool_entry("slugify", body, Vec::new());

        // A directory where the receipt goes: `fs::write` cannot replace it,
        // so the install fails at the receipt. Standing in for a full disk or
        // a read-only `~/.wizard`, both of which fail the same way.
        std::fs::create_dir_all(roots.dir.join("tools/slugify.registry.json")).unwrap();
        let err = roots
            .client
            .install_verified(&entry, body.as_bytes(), Trust::Sandboxed)
            .expect_err("the receipt write fails");
        assert!(
            format!("{err:#}").contains("slugify.registry.json"),
            "{err:#}"
        );
        assert!(
            !roots.dir.join("tools/slugify.lua").exists(),
            "a registry script was left on disk with no receipt: it would run unsandboxed"
        );
        std::fs::remove_dir(roots.dir.join("tools/slugify.registry.json")).unwrap();

        // Same invariant one step later. Install for real, then break the
        // manifest path so the *last* write of a reinstall is the one that
        // fails: the receipt and the script are already down by then, so this
        // is the case that needs the rollback rather than the ordering.
        roots
            .client
            .install_verified(&entry, body.as_bytes(), Trust::Sandboxed)
            .expect("first install");
        std::fs::remove_file(roots.dir.join("tools/slugify.toml")).unwrap();
        std::fs::create_dir_all(roots.dir.join("tools/slugify.toml")).unwrap();

        let err = roots
            .client
            .install_verified(&entry, body.as_bytes(), Trust::Sandboxed)
            .expect_err("the manifest write fails");
        assert!(format!("{err:#}").contains("slugify.toml"), "{err:#}");
        assert!(
            !roots.dir.join("tools/slugify.lua").exists(),
            "script rolled back"
        );
        assert!(
            !roots.dir.join("tools/slugify.registry.json").exists(),
            "receipt rolled back"
        );
        assert!(
            roots.client.installed().unwrap().is_empty(),
            "a rolled-back install is not listed as installed"
        );
    }

    #[test]
    fn uninstall_removes_only_what_the_receipt_claims() {
        let roots = Roots::new("uninstall");
        let body = "return 1\n";
        let entry = tool_entry("slugify", body, Vec::new());
        roots
            .client
            .install_verified(&entry, body.as_bytes(), Trust::Sandboxed)
            .expect("install");
        // A tool the user wrote, sharing the directory.
        std::fs::write(roots.dir.join("tools/mine.lua"), "print('mine')\n").unwrap();

        let removed = roots
            .client
            .uninstall(EntryKind::Tool, "slugify")
            .expect("uninstall");
        assert_eq!(
            removed.len(),
            3,
            "script, manifest and receipt: {removed:?}"
        );
        assert!(!roots.dir.join("tools/slugify.lua").exists());
        assert!(!roots.dir.join("tools/slugify.toml").exists());
        assert!(!roots.dir.join("tools/slugify.registry.json").exists());
        assert!(roots.client.installed().unwrap().is_empty());
        assert!(
            roots.dir.join("tools/mine.lua").exists(),
            "the user's own tool is not the registry's to delete"
        );

        // Nothing installed under that name: refused, rather than guessing at
        // which files `mine` might own.
        let err = roots
            .client
            .uninstall(EntryKind::Tool, "mine")
            .expect_err("no receipt, no uninstall");
        assert!(
            format!("{err:#}").contains("Nothing was removed"),
            "{err:#}"
        );
        assert!(roots.dir.join("tools/mine.lua").exists());

        // A skill takes its directory with it.
        let skill_body = "# body\n";
        let skill = skill_entry("commits", skill_body);
        roots
            .client
            .install_verified(&skill, skill_body.as_bytes(), Trust::Sandboxed)
            .expect("install skill");
        roots
            .client
            .uninstall(EntryKind::Skill, "commits")
            .expect("uninstall skill");
        assert!(!roots.dir.join("skills/commits").exists());
    }

    #[test]
    fn an_edited_receipt_cannot_turn_uninstall_into_an_arbitrary_delete() {
        let roots = Roots::new("receiptpath");
        let body = "return 1\n";
        let entry = tool_entry("slugify", body, Vec::new());
        roots
            .client
            .install_verified(&entry, body.as_bytes(), Trust::Sandboxed)
            .expect("install");

        let victim = roots.dir.join("precious.txt");
        std::fs::write(&victim, "keep me\n").unwrap();
        let receipt_path = roots.dir.join("tools/slugify.registry.json");
        let mut receipt: Receipt =
            serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
        receipt.files = vec!["../precious.txt".to_string()];
        std::fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();

        let err = roots
            .client
            .uninstall(EntryKind::Tool, "slugify")
            .expect_err("a path in the file list is refused");
        assert!(format!("{err:#}").contains("path in it"), "{err:#}");
        assert!(
            victim.exists(),
            "uninstall walked out of the install directory"
        );
    }

    #[test]
    fn a_skill_installs_beside_the_bundled_ones_with_a_hidden_receipt() {
        let roots = Roots::new("skill");
        let body = "---\nname: commits\n---\nUse conventional commits.\n";
        let entry = skill_entry("commits", body);
        roots
            .client
            .install_verified(&entry, body.as_bytes(), Trust::Sandboxed)
            .expect("install");

        let skill_md = roots.dir.join("skills/commits/SKILL.md");
        assert_eq!(std::fs::read_to_string(&skill_md).unwrap(), body);
        // The receipt is hidden inside the skill directory so the loader,
        // which globs `*/SKILL.md`, never sees it.
        assert!(roots.dir.join("skills/commits/.registry.json").is_file());

        let skills = crate::skills::load_skills(&[roots.dir.join("skills")]).expect("load");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "commits");

        let listed = roots.client.installed().expect("listing");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, EntryKind::Skill);
        assert_eq!(listed[0].author, "bob");
    }

    // -- the trust decision -------------------------------------------------

    #[test]
    fn a_tool_declaring_no_capabilities_installs_sandboxed_without_asking() {
        assert_eq!(decide_trust(&[], false, None), Some(Trust::Sandboxed));
        // Even an up-front grant does not widen a tool that asked for nothing.
        assert_eq!(decide_trust(&[], true, Some(true)), Some(Trust::Sandboxed));
    }

    #[test]
    fn the_full_stdlib_requires_an_explicit_opt_in() {
        let caps = [Capability::Process];
        // Nobody to ask, no flag: refuse. This is the non-interactive default.
        assert_eq!(decide_trust(&caps, false, None), None);
        // Asked and declined: refuse.
        assert_eq!(decide_trust(&caps, false, Some(false)), None);
        // Asked and accepted, or granted up front: full.
        assert_eq!(decide_trust(&caps, false, Some(true)), Some(Trust::Full));
        assert_eq!(decide_trust(&caps, true, None), Some(Trust::Full));
    }

    #[test]
    fn a_capability_declaring_install_refuses_when_there_is_no_console() {
        let roots = Roots::new("nogrant");
        let body = "os.execute('id')\n";
        let entry = tool_entry("runner", body, vec![Capability::Process]);
        let err = roots
            .client
            .settle_trust(&entry, InstallOptions::default())
            .expect_err("no console, no grant");
        let message = format!("{err:#}");
        assert!(message.contains("grant flag"), "{message}");
        assert!(message.contains("alice"), "{message}");

        // With the flag it installs, and the receipt records the grant so the
        // runner can find it.
        let trust = roots
            .client
            .settle_trust(
                &entry,
                InstallOptions {
                    trust: true,
                    ..InstallOptions::default()
                },
            )
            .expect("granted");
        assert_eq!(trust, Trust::Full);
        roots
            .client
            .install_verified(&entry, body.as_bytes(), trust)
            .expect("install");
        assert_eq!(
            trust_for_script(&roots.dir.join("tools/runner.lua")),
            Some(Trust::Full)
        );
    }

    #[test]
    fn the_grant_prompt_names_the_author_the_bytes_and_the_limit() {
        let entry = tool_entry(
            "runner",
            "x",
            vec![Capability::Process, Capability::Filesystem],
        );
        let text = grant_prompt(
            &entry,
            "https://example.invalid/tools/alice/runner/tool.lua",
        );
        assert!(text.contains("alice"), "{text}");
        assert!(text.contains("https://example.invalid/tools/alice/runner/tool.lua"));
        assert!(text.contains(&entry.manifest.expected_digest()));
        assert!(text.contains("os.execute"), "{text}");
        assert!(text.contains("io.open"), "{text}");
        assert!(text.contains("all or nothing"), "{text}");
        // The author's own description is never echoed into the prompt.
        assert!(!text.contains("test tool"), "{text}");
    }

    // -- receipts and the runtime hand-off ----------------------------------

    #[test]
    fn a_script_with_no_receipt_is_local_and_a_damaged_one_is_sandboxed() {
        let roots = Roots::new("receipts");
        let script = roots.dir.join("tools/local.lua");
        std::fs::write(&script, "print('mine')\n").unwrap();
        assert_eq!(trust_for_script(&script), None, "no receipt means local");

        // A receipt that cannot be parsed downgrades to the sandbox. Damaging
        // a receipt must never be a way to promote a tool.
        std::fs::write(receipt_for_script(&script), b"{ not json").unwrap();
        assert_eq!(trust_for_script(&script), Some(Trust::Sandboxed));
    }

    /// A receipt that exists and cannot be read is not the same as no receipt.
    ///
    /// `std::fs::read(..).ok()?` answered both with `None`, and
    /// `resolve_stdlib` reads `None` as "locally authored — full standard
    /// library". So one `chmod 000` on a registry tool's receipt handed that
    /// tool `os`, `io` and `ffi`: a privilege escalation performed by taking
    /// permissions *away*, and the exact inverse of what the function's own
    /// doc comment promises.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_receipt_sandboxes_the_tool_rather_than_promoting_it() {
        use std::os::unix::fs::PermissionsExt as _;

        let roots = Roots::new("unreadable-receipt");
        let script = roots.dir.join("tools/registry.lua");
        std::fs::write(&script, "print('theirs')\n").unwrap();
        let receipt = receipt_for_script(&script);
        std::fs::write(&receipt, br#"{"trust":"full"}"#).unwrap();
        std::fs::set_permissions(&receipt, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Running the suite as root would read it anyway and prove nothing.
        if std::fs::read(&receipt).is_ok() {
            return;
        }
        assert_eq!(
            trust_for_script(&script),
            Some(Trust::Sandboxed),
            "an unreadable receipt must fail closed, not fall through to `no receipt`"
        );

        std::fs::set_permissions(&receipt, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn a_sandboxed_install_records_sandboxed_trust_for_the_runner() {
        let roots = Roots::new("sandboxtrust");
        let body = "return 1\n";
        let entry = tool_entry("pure", body, Vec::new());
        roots
            .client
            .install_verified(&entry, body.as_bytes(), Trust::Sandboxed)
            .expect("install");
        assert_eq!(
            trust_for_script(&roots.dir.join("tools/pure.lua")),
            Some(Trust::Sandboxed),
            "a registry tool is sandboxed unless the user granted more"
        );
    }

    // -- the offline cache --------------------------------------------------

    #[test]
    fn the_cached_index_is_what_makes_search_work_offline() {
        let roots = Roots::new("cache");
        assert!(roots.client.cached_index().unwrap().is_none());

        let index = RegistryIndex {
            version: INDEX_VERSION,
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            entries: vec![tool_entry("slugify", "x", Vec::new())],
        };
        roots
            .client
            .write_cache(&serde_json::to_vec(&index).unwrap())
            .expect("cache write");

        let cached = roots
            .client
            .cached_index()
            .unwrap()
            .expect("a cached index");
        assert_eq!(cached, index);
        assert!(roots.client.cache_age().expect("an age") < INDEX_TTL);
        assert_eq!(search(&cached, "slugify", None).len(), 1);

        // A corrupt cache is an error, not an empty registry: "nothing
        // matched" would be a lie.
        std::fs::write(roots.client.cache_path(), b"<html>captive portal</html>").unwrap();
        assert!(roots.client.cached_index().is_err());
    }

    #[test]
    fn update_reports_what_it_refused_to_do() {
        // The interesting outcomes are the ones that do not touch the network.
        assert!(
            UpdateOutcome::AuthorChanged {
                name: "slugify".into(),
                installed: "alice".into(),
                published: "mallory".into(),
            }
            .summary()
            .contains("NOT updated")
        );
        assert!(
            UpdateOutcome::NeedsConsent {
                name: "runner".into(),
                to: "2.0.0".into(),
            }
            .summary()
            .contains("full-stdlib grant")
        );
        assert!(
            UpdateOutcome::Updated {
                name: "slugify".into(),
                from: "1.0.0".into(),
                to: "1.1.0".into(),
            }
            .summary()
            .contains("1.0.0 -> 1.1.0")
        );

        // Only a real failure is a failure. An exit code that fires on
        // "up to date" is an exit code nobody reads.
        assert!(
            UpdateOutcome::Failed {
                name: "slugify".into(),
                error: "checksum mismatch".into(),
            }
            .is_failure()
        );
        for benign in [
            UpdateOutcome::UpToDate {
                name: "slugify".into(),
            },
            UpdateOutcome::Unpublished {
                name: "slugify".into(),
            },
            UpdateOutcome::NeedsConsent {
                name: "runner".into(),
                to: "2.0.0".into(),
            },
        ] {
            assert!(!benign.is_failure(), "{benign:?}");
        }
    }

    #[test]
    fn update_never_takes_a_new_version_it_should_have_asked_about() {
        let installed = |name: &str, author: &str, checksum: &str, trust: Trust| Receipt {
            name: name.to_string(),
            kind: EntryKind::Tool,
            author: author.to_string(),
            version: "1.0.0".to_string(),
            checksum: checksum.to_string(),
            source: String::new(),
            installed_at: String::new(),
            trust,
            capabilities: Vec::new(),
            files: Vec::new(),
        };
        let published = tool_entry("slugify", "the new body", Vec::new());
        let index = RegistryIndex {
            version: INDEX_VERSION,
            generated_at: String::new(),
            entries: vec![published.clone()],
        };
        let old = "1".repeat(64);
        let current = published.manifest.expected_digest();

        // Same author, new bytes: replace.
        assert!(matches!(
            plan_update(&installed("slugify", "alice", &old, Trust::Sandboxed), &index, false),
            UpdatePlan::Replace(entry) if entry == &published
        ));
        // Same bytes: nothing to do.
        assert!(matches!(
            plan_update(
                &installed("slugify", "alice", &current, Trust::Sandboxed),
                &index,
                false
            ),
            UpdatePlan::Report(UpdateOutcome::UpToDate { .. })
        ));
        // Gone from the index: left alone rather than deleted.
        assert!(matches!(
            plan_update(
                &installed("other", "alice", &old, Trust::Sandboxed),
                &index,
                false
            ),
            UpdatePlan::Report(UpdateOutcome::Unpublished { .. })
        ));
        // The name is published by somebody else now. This is what a takeover
        // looks like, so it is never taken automatically, grant flag or not.
        for granted in [false, true] {
            assert!(
                matches!(
                    plan_update(
                        &installed("slugify", "mallory", &old, Trust::Sandboxed),
                        &index,
                        granted
                    ),
                    UpdatePlan::Report(UpdateOutcome::AuthorChanged { .. })
                ),
                "an author change was taken with granted={granted}"
            );
        }
        // A full-stdlib install needs the question asked again: the grant
        // covered the code the user read, not what has been pushed since.
        assert!(matches!(
            plan_update(
                &installed("slugify", "alice", &old, Trust::Full),
                &index,
                false
            ),
            UpdatePlan::Report(UpdateOutcome::NeedsConsent { .. })
        ));
        assert!(matches!(
            plan_update(
                &installed("slugify", "alice", &old, Trust::Full),
                &index,
                true
            ),
            UpdatePlan::Replace(_)
        ));
    }

    #[test]
    fn install_will_not_guess_between_a_skill_and_a_tool_of_the_same_name() {
        let roots = Roots::new("ambiguous");
        let mut skill = skill_entry("notes", "# notes");
        skill.manifest.author = "alice".to_string();
        let index = RegistryIndex {
            version: INDEX_VERSION,
            generated_at: String::new(),
            entries: vec![tool_entry("notes", "return 1", Vec::new()), skill],
        };
        roots
            .client
            .write_cache(&serde_json::to_vec(&index).unwrap())
            .expect("cache write");

        // Both kinds match, so `matching` returns two and install refuses.
        assert_eq!(index.matching("notes", None).len(), 2);
        assert_eq!(index.matching("notes", Some(EntryKind::Tool)).len(), 1);
        assert_eq!(index.matching("nothing", None).len(), 0);

        // The refusal itself: a name published as both must not resolve to
        // whichever row CI happened to emit first.
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(roots.client.install("notes", InstallOptions::default()))
            .expect_err("ambiguous name");
        let message = format!("{err:#}");
        assert!(message.contains("a skill and a tool") || message.contains("a tool and a skill"));
    }

    #[test]
    fn a_manifest_may_not_declare_the_same_capability_twice() {
        // Not pedantry: each declared capability is a line in the grant
        // prompt, and a prompt that says the same thing twice is a prompt
        // people skim.
        let mut entry = tool_entry(
            "runner",
            "x",
            vec![Capability::Process, Capability::Process],
        );
        let err = entry.validate().unwrap_err();
        assert!(format!("{err:#}").contains("process twice"), "{err:#}");

        entry.manifest.capabilities = vec![Capability::Process, Capability::Filesystem];
        entry
            .validate()
            .expect("two different capabilities are fine");
    }

    #[test]
    fn a_description_cannot_repaint_the_terminal_from_a_listing() {
        // `wizard skills search` prints a stranger's prose. A description
        // carrying `ESC[2J` would clear the screen the listing is on, and one
        // carrying newlines and padding could forge a second row.
        let hostile = "clear\u{1b}[2J\u{7}this\nrow\tis\rfake        and     padded";
        let printed = one_line(hostile);
        assert!(!printed.contains('\u{1b}'), "{printed:?}");
        assert!(!printed.chars().any(char::is_control), "{printed:?}");
        assert!(!printed.contains("  "), "runs collapse: {printed:?}");
        assert_eq!(printed, "clear [2J this row is fake and padded");

        // One entry may not occupy the terminal with a single row.
        let long = "word ".repeat(400);
        let clipped = one_line(&long);
        assert_eq!(clipped.chars().count(), DESCRIPTION_CHARS);
        assert!(clipped.ends_with('…'), "{clipped:?}");

        // And a short, ordinary description survives untouched.
        assert_eq!(one_line("  Adds a todo list. "), "Adds a todo list.");
    }

    #[test]
    fn the_kind_filter_defaults_to_refusing_rather_than_guessing() {
        // `None` reaches `install`, which refuses a name published as both a
        // skill and a tool. It is not a synonym for "either will do": the flag
        // the user did not pass must never decide whether they get prompt text
        // or code.
        assert_eq!(kind_filter(false, false), None);
        assert_eq!(kind_filter(true, false), Some(EntryKind::Skill));
        assert_eq!(kind_filter(false, true), Some(EntryKind::Tool));
    }

    #[test]
    fn the_stdlib_label_never_makes_a_full_grant_look_ordinary() {
        // This string is the only thing between a user and "I installed a
        // thing", so it names the functions rather than a reassuring word.
        let full = stdlib_label(Trust::Full);
        assert!(full.contains("FULL"), "{full}");
        assert!(full.contains("os.execute"), "{full}");
        assert!(stdlib_label(Trust::Sandboxed).contains("no os"));
    }

    /// A client whose base URL is the only field the test needs. Nothing here
    /// touches the filesystem or the network: `explain_index_failure` is a pure
    /// function of the base and the error, which is the whole reason it is a
    /// method and not an inline `match` inside `refresh_index`.
    fn client_at(base: &str) -> RegistryClient {
        RegistryClient::with_roots(base, "cache", "skills", "tools")
    }

    fn index_404(base: &str) -> anyhow::Error {
        anyhow::Error::from(HttpStatus {
            url: format!("{base}/{INDEX_FILE}"),
            code: 404,
        })
    }

    #[test]
    fn a_missing_default_registry_names_the_override_not_the_status_code() {
        // A 404 on the default URL is unusual once the in-tree index ships.
        let base = DEFAULT_BASE_URL;
        let err = index_404(base);
        assert!(is_not_found(&err));

        let explained = format!("{:#}", client_at(base).explain_index_failure(err));
        assert!(
            explained.contains("WIZARD_REGISTRY_URL"),
            "the one thing a user can do about it has to be in the message: {explained}"
        );
        assert!(
            explained.contains("in-tree"),
            "a default 404 has to name where the index lives: {explained}"
        );
        // The status is still in the chain, because "which URL, and what did it
        // say" is what a bug report needs. It is just no longer the whole
        // answer.
        assert!(explained.contains("HTTP 404"), "{explained}");
    }

    #[test]
    fn a_missing_registry_behind_the_override_blames_the_override() {
        // Same status, different cause: the user set WIZARD_REGISTRY_URL and
        // got it wrong. Telling them to set WIZARD_REGISTRY_URL would be
        // useless, so this branch says what the value has to look like instead.
        let base = "https://github.com/someone/their-registry";
        let explained = format!(
            "{:#}",
            client_at(base).explain_index_failure(index_404(base))
        );
        assert!(explained.contains(base), "{explained}");
        assert!(
            explained.contains("raw.githubusercontent.com"),
            "{explained}"
        );
    }

    #[test]
    fn everything_that_is_not_a_404_is_left_exactly_as_it_arrived() {
        // A 500, a timeout, a captive portal: these really are "try again"
        // failures, and dressing one up as "the registry does not exist" would
        // send somebody to change a URL that was right all along.
        let err = anyhow::Error::from(HttpStatus {
            url: format!("{DEFAULT_BASE_URL}/{INDEX_FILE}"),
            code: 503,
        });
        assert!(!is_not_found(&err));
        let explained = format!(
            "{:#}",
            client_at(DEFAULT_BASE_URL).explain_index_failure(err)
        );
        assert!(!explained.contains("WIZARD_REGISTRY_URL"), "{explained}");
        assert!(explained.contains("HTTP 503"), "{explained}");

        // And an error that is not an HTTP status at all (DNS, TLS, a body
        // over the size limit) passes through untouched too.
        let network = anyhow!("fetching {DEFAULT_BASE_URL}: dns error");
        assert!(!is_not_found(&network));
        assert_eq!(
            format!(
                "{:#}",
                client_at(DEFAULT_BASE_URL).explain_index_failure(network)
            ),
            format!("fetching {DEFAULT_BASE_URL}: dns error")
        );
    }

    #[test]
    fn in_tree_registry_parses_and_checksums_match() {
        use std::collections::BTreeSet;

        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("registry");
        let raw = std::fs::read(root.join("registry.json")).expect("registry/registry.json");
        let index = RegistryIndex::parse(&raw).expect("in-tree index parses");
        assert_eq!(index.version, INDEX_VERSION);
        assert!(
            !index.generated_at.is_empty(),
            "generated_at must be present"
        );

        let mut tree_paths = BTreeSet::new();
        for kind in [EntryKind::Skill, EntryKind::Tool] {
            let kind_dir = root.join(kind.dir());
            let Ok(authors) = std::fs::read_dir(&kind_dir) else {
                continue;
            };
            for author in authors.flatten() {
                let Ok(names) = std::fs::read_dir(author.path()) else {
                    continue;
                };
                for entry_dir in names.flatten() {
                    let manifest_path = entry_dir.path().join("manifest.toml");
                    if !manifest_path.is_file() {
                        continue;
                    }
                    let text = std::fs::read_to_string(&manifest_path)
                        .unwrap_or_else(|err| panic!("read {}: {err}", manifest_path.display()));
                    let manifest: Manifest = toml::from_str(&text)
                        .unwrap_or_else(|err| panic!("parse {}: {err}", manifest_path.display()));
                    manifest.validate().expect("manifest validates");
                    assert_eq!(manifest.kind, kind);
                    assert!(
                        !BUNDLED_SKILL_NAMES.contains(&manifest.name.as_str()),
                        "{}",
                        manifest.name
                    );
                    let rel = format!("{}/{}/{}", kind.dir(), manifest.author, manifest.name);
                    let artifact = root.join(&rel).join(manifest.artifact_name());
                    let bytes = std::fs::read(&artifact)
                        .unwrap_or_else(|err| panic!("read {}: {err}", artifact.display()));
                    assert!(manifest.matches(&bytes), "checksum mismatch for {rel}");
                    assert!(tree_paths.insert(rel.clone()), "duplicate tree path {rel}");
                }
            }
        }

        let index_paths: BTreeSet<String> = index
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        assert_eq!(
            tree_paths, index_paths,
            "registry.json drifted from the tree; run contrib/check-registry.py --write"
        );
        assert!(!tree_paths.is_empty(), "ship at least one skill");

        for entry in &index.entries {
            entry.validate().expect("index entry validates");
            let artifact = root.join(&entry.path).join(entry.manifest.artifact_name());
            let bytes = std::fs::read(&artifact)
                .unwrap_or_else(|err| panic!("read {}: {err}", artifact.display()));
            assert!(
                entry.manifest.matches(&bytes),
                "checksum mismatch for {} at {}",
                entry.name(),
                artifact.display()
            );
        }
    }
}
