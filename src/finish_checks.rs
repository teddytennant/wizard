//! Finish checks: four facts about the working tree, read off with `git` when
//! a headless run says it is done, before any quality gate or review runs.
//!
//! A gate ([`crate::gates`]) is a command the operator named. The completion
//! review ([`crate::agent::critic`]) is a model reading the result. These sit
//! beside both and are neither: no command to configure and no model call, so
//! a clean run pays a few `git` invocations and nothing else. Each one looks
//! for a mistake that is cheap to state exactly and that a reviewer reading a
//! diff tends to miss:
//!
//! 1. a name the request states (`snake_case`, `camelCase`, `--a-flag`) that
//!    appears nowhere in the tree, which is what an implementation under
//!    invented names looks like;
//! 2. a test that existed at the starting commit and is gone;
//! 3. a request that asks for a commit, with nothing committed or with work
//!    left uncommitted, or a change request that ends with the tree untouched;
//! 4. a new Go test file that redeclares a name another test file of the same
//!    package declares, which stops the whole package's tests compiling.
//!
//! What they find goes back to the agent as one message and one more turn.
//!
//! # Why every rule here errs towards silence
//!
//! A false alarm costs a model turn and can talk a correct solution out of
//! being correct, so precision is the whole design. A check that cannot parse
//! a file says nothing. A name counts as present in any case style, in any
//! file's contents or path. A name that was also absent at the starting commit
//! is the only kind reported, so "remove `old_name`" is never a finding. And
//! each check fires at most once per run: the agent is told to fix it or say
//! why it is intended, and a deliberate deviation is not asked about twice.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use tokio::sync::mpsc;

use crate::agent::AgentEvent;
use crate::cli::Cli;
use crate::config::{Config, Mode};

/// Wall clock for one pass over all four checks. A pass that runs out says
/// nothing and is not tried again.
const BUDGET: Duration = Duration::from_secs(20);

/// Stated names looked up per pass.
const MAX_NAMES: usize = 60;

/// Changed files past which the test check says nothing, and new Go test
/// files read by the duplicate check.
const MAX_FILES: usize = 300;

/// A source file larger than this is not parsed.
const MAX_SOURCE_BYTES: u64 = 2_000_000;

/// Items listed per finding before the rest are counted.
const MAX_LISTED: usize = 12;

/// The four checks, so each can be marked as having had its say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    Names,
    Tests,
    Commit,
    Duplicates,
}

/// A name the request states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatedName {
    /// As written in the request.
    pub text: String,
    /// Written in backticks, or a `--flag` leading its own backticked span.
    /// One missing marked name is a finding; unmarked names need two, because
    /// prose has `JavaScript` and `GitHub` in it.
    pub marked: bool,
}

/// What a pass found. Empty fields are checks with nothing to say.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Findings {
    /// Stated names absent from the tree now and at the starting commit.
    pub missing_names: Vec<String>,
    /// `(test, file it was in)` for tests that existed at the start.
    pub vanished_tests: Vec<(String, String)>,
    /// One sentence on what is not committed, or not changed.
    pub uncommitted: Option<String>,
    /// `(name, new test file, file that already declares it)`.
    pub duplicates: Vec<(String, String, String)>,
}

/// The finish checks of one run and which of them have already fired.
pub struct FinishChecks {
    root: PathBuf,
    request: String,
    /// `HEAD` when the run started.
    base: String,
    /// Paths already modified or untracked when the run started. They are the
    /// user's, so nothing about them is the agent's doing.
    dirty_at_start: BTreeSet<String>,
    fired: Vec<Check>,
}

impl FinishChecks {
    /// The checks for this run, or `None` when they do not apply.
    ///
    /// On for a headless sovereign run that was handed a request and is meant
    /// to finish, which is the run nobody is watching. Not in the TUI, where
    /// the user reads the result and a mechanical nag is noise. Not for a
    /// continuous mission, whose cycle ending is not a claim that the request
    /// is done and whose starting commit is long gone. Not for a run resumed
    /// without `-p`, which may already have committed before this process
    /// started. And not outside a git repo with a commit, because every check
    /// is a comparison against the starting commit.
    pub async fn for_run(
        config: &Config,
        cli: &Cli,
        project_root: &Path,
        request: &str,
    ) -> Option<Self> {
        if !config.finish_checks
            || config.mode != Mode::Sovereign
            || config.continuous
            || cli.prompt.is_none()
        {
            return None;
        }
        let base = head(project_root).await?;
        let dirty_at_start = Changes::since(project_root, &base).await?.paths();
        Some(Self {
            root: project_root.to_path_buf(),
            request: request.to_string(),
            base,
            dirty_at_start,
            fired: Vec::new(),
        })
    }

    /// Run the checks that have not fired yet. `Some(prompt)` is the one
    /// message to hand back as another turn.
    pub async fn check(&mut self, events: &mpsc::Sender<AgentEvent>) -> Option<String> {
        if self.fired.len() == 4 {
            return None;
        }
        let Ok(found) = tokio::time::timeout(BUDGET, self.inspect()).await else {
            tracing::warn!("finish checks ran past {BUDGET:?} and were dropped");
            self.fired = vec![Check::Names, Check::Tests, Check::Commit, Check::Duplicates];
            return None;
        };
        let found = found?;
        for (check, fired) in [
            (Check::Names, !found.missing_names.is_empty()),
            (Check::Tests, !found.vanished_tests.is_empty()),
            (Check::Commit, found.uncommitted.is_some()),
            (Check::Duplicates, !found.duplicates.is_empty()),
        ] {
            if fired {
                self.fired.push(check);
            }
        }
        let prompt = found.prompt()?;
        let _ = events
            .send(AgentEvent::Notice(format!(
                "finish check: {}",
                found.summary()
            )))
            .await;
        Some(prompt)
    }

    fn pending(&self, check: Check) -> bool {
        !self.fired.contains(&check)
    }

    /// One pass. `None` when git could not answer, which is no finding.
    pub(crate) async fn inspect(&self) -> Option<Findings> {
        let root = &self.root;
        let head = head(root).await?;
        let mut changes = Changes::since(root, &self.base).await?;
        changes.forget(&self.dirty_at_start);
        let mut found = Findings::default();

        if self.pending(Check::Commit) {
            found.uncommitted = self.uncommitted(&head, &changes).await;
        }
        // An untouched tree has one thing wrong with it, and the commit check
        // has just said so. Listing every stated name as missing adds nothing.
        if changes.is_empty() {
            return Some(found);
        }
        if self.pending(Check::Names) {
            found.missing_names = self.missing_names().await;
        }
        if self.pending(Check::Tests) {
            found.vanished_tests = self.vanished_tests(&changes).await;
        }
        if self.pending(Check::Duplicates) {
            found.duplicates = self.duplicates(&changes).await;
        }
        Some(found)
    }

    async fn uncommitted(&self, head: &str, changes: &Changes) -> Option<String> {
        let short = &self.base[..self.base.len().min(8)];
        if asks_to_commit(&self.request) {
            if head == self.base {
                return Some(format!(
                    "The request asks for a commit, and HEAD is still the commit this run started \
                     from ({short}): nothing has been committed."
                ));
            }
            let mut left = Changes::since(&self.root, head).await?;
            left.forget(&self.dirty_at_start);
            if left.is_empty() {
                return None;
            }
            return Some(format!(
                "The request asks for a commit, and these are not committed: {}.",
                listed(left.paths().iter().map(|path| format!("`{path}`")))
            ));
        }
        (head == self.base
            && changes.is_empty()
            && self.dirty_at_start.is_empty()
            && opens_with_change_verb(&self.request))
        .then(|| {
            format!(
                "The request asks for a change, and the working tree is identical to the commit \
                 this run started from ({short}): no file was added, edited or removed."
            )
        })
    }

    async fn missing_names(&self) -> Vec<String> {
        let names = stated_names(&self.request);
        if names.is_empty() || names.len() > MAX_NAMES {
            return Vec::new();
        }
        let Some(paths) = self.tree_paths().await else {
            return Vec::new();
        };
        let mut missing: Vec<(&StatedName, Vec<String>)> = names
            .iter()
            .map(|name| (name, name_variants(&name.text)))
            .filter(|(_, variants)| !variants.iter().any(|v| paths.contains(v.as_str())))
            .collect();
        // Absent now and absent before: a name the request introduces that the
        // work never used. Anything but a clean "no match" is read as present.
        for rev in [None, Some(self.base.as_str())] {
            let needles: Vec<String> = missing.iter().flat_map(|(_, v)| v.clone()).collect();
            if needles.is_empty() {
                return Vec::new();
            }
            let Some(seen) = occurring(&self.root, &needles, rev).await else {
                return Vec::new();
            };
            let mut absent = Vec::new();
            for (name, variants) in missing {
                if variants.iter().any(|variant| seen.contains(variant)) {
                    continue;
                }
                // One scan for all names reports each stretch of text once, so
                // a match for one name can hide an overlapping other. A name
                // nothing was reported for is asked about again, alone.
                if seen.is_empty() || grep(&self.root, &variants, rev).await == Some(false) {
                    absent.push((name, variants));
                }
            }
            missing = absent;
        }
        let missing: Vec<&StatedName> = missing.into_iter().map(|(name, _)| name).collect();
        let marked = missing.iter().filter(|name| name.marked).count();
        if marked == 0 && missing.len() < 2 {
            return Vec::new();
        }
        missing.into_iter().map(|name| name.text.clone()).collect()
    }

    async fn vanished_tests(&self, changes: &Changes) -> Vec<(String, String)> {
        // A change too large to read in full cannot say where a test went.
        if changes.paths().len() > MAX_FILES {
            return Vec::new();
        }
        // Every test in the files as they are now: a test moved to another
        // file has not vanished.
        let mut now = BTreeSet::new();
        for path in changes.added.iter().chain(&changes.modified) {
            if let Some(names) = self.read(path).and_then(|source| test_names(path, &source)) {
                now.extend(names);
            }
        }
        let mut vanished = Vec::new();
        for path in changes.modified.iter().chain(&changes.deleted) {
            if !is_test_file(path) {
                continue;
            }
            let spec = format!("{}:./{path}", self.base);
            let Some(before) = git(&self.root, &["show", &spec]).await else {
                continue;
            };
            let before = String::from_utf8_lossy(&before);
            for name in test_names(path, &before).unwrap_or_default() {
                if !now.contains(&name) {
                    vanished.push((name, path.clone()));
                }
            }
        }
        vanished
    }

    async fn duplicates(&self, changes: &Changes) -> Vec<(String, String, String)> {
        let new_tests: Vec<&String> = changes
            .added
            .iter()
            .filter(|path| path.ends_with("_test.go"))
            .take(MAX_FILES)
            .collect();
        if new_tests.is_empty() {
            return Vec::new();
        }
        let Some(listing) = self.tree_paths().await else {
            return Vec::new();
        };
        let mut duplicates = Vec::new();
        for path in new_tests {
            let Some((package, mine)) = self.read(path).and_then(|src| go_declarations(path, &src))
            else {
                continue;
            };
            let dir = parent(path);
            for sibling in listing.lines().filter(|other| {
                other.ends_with("_test.go") && parent(other) == dir && *other != path.as_str()
            }) {
                let Some((other_package, theirs)) = self
                    .read(sibling)
                    .and_then(|src| go_declarations(sibling, &src))
                else {
                    continue;
                };
                if other_package != package {
                    continue;
                }
                for name in mine.intersection(&theirs) {
                    // Two new files that collide are one finding, not two.
                    if !duplicates
                        .iter()
                        .any(|(seen, a, b): &(String, String, String)| {
                            seen == name && a == sibling && b == path
                        })
                    {
                        duplicates.push((name.clone(), path.clone(), sibling.to_string()));
                    }
                }
            }
        }
        duplicates
    }

    /// Every tracked or untracked-and-not-ignored path, one per line.
    async fn tree_paths(&self) -> Option<String> {
        let raw = git(
            &self.root,
            &["ls-files", "--cached", "--others", "--exclude-standard"],
        )
        .await?;
        Some(String::from_utf8_lossy(&raw).into_owned())
    }

    fn read(&self, path: &str) -> Option<String> {
        let absolute = self.root.join(path);
        let meta = std::fs::metadata(&absolute).ok()?;
        if !meta.is_file() || meta.len() > MAX_SOURCE_BYTES {
            return None;
        }
        std::fs::read_to_string(absolute).ok()
    }
}

impl Findings {
    /// The message for the agent, or `None` when nothing was found.
    pub fn prompt(&self) -> Option<String> {
        let mut items = Vec::new();
        if !self.missing_names.is_empty() {
            items.push(format!(
                "The request states these names, and they appear nowhere in the working tree, in \
                 any file's contents or path, in any case style: {}. If the work implements them \
                 under other names, the request's spelling is the interface.",
                listed(self.missing_names.iter().map(|name| format!("`{name}`")))
            ));
        }
        if !self.vanished_tests.is_empty() {
            items.push(format!(
                "These tests existed when the run started and are gone, renamed or deleted: {}. \
                 Whoever checks this work may still run them by name.",
                listed(
                    self.vanished_tests
                        .iter()
                        .map(|(name, path)| format!("`{name}` ({path})"))
                )
            ));
        }
        if let Some(uncommitted) = &self.uncommitted {
            items.push(uncommitted.clone());
        }
        if !self.duplicates.is_empty() {
            items.push(format!(
                "A new Go test file declares a name another test file of the same package already \
                 declares, so the package's tests will not compile: {}.",
                listed(
                    self.duplicates
                        .iter()
                        .map(|(name, new, old)| format!("`{name}` in {new} and {old}"))
                )
            ));
        }
        if items.is_empty() {
            return None;
        }
        Some(format!(
            "Before this run is finished: a mechanical check of the working tree against the \
             request and the starting commit found the following. Each item is a literal fact \
             about the tree, not a judgement.\n\n{}\n\n\
             Fix each one that is a mistake. If one is intended, leave it and say why in a line. \
             Change nothing else, and these checks will not be raised again.",
            items
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }

    /// One line for the notice stream.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.missing_names.is_empty() {
            parts.push(format!(
                "{} stated name(s) missing",
                self.missing_names.len()
            ));
        }
        if !self.vanished_tests.is_empty() {
            parts.push(format!(
                "{} existing test(s) gone",
                self.vanished_tests.len()
            ));
        }
        if self.uncommitted.is_some() {
            parts.push("work not committed or not changed".to_string());
        }
        if !self.duplicates.is_empty() {
            parts.push(format!(
                "{} duplicate test declaration(s)",
                self.duplicates.len()
            ));
        }
        parts.join(", ")
    }
}

/// Up to [`MAX_LISTED`] items, then a count of the rest.
fn listed(items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    let mut text = items
        .iter()
        .take(MAX_LISTED)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > MAX_LISTED {
        text.push_str(&format!(" and {} more", items.len() - MAX_LISTED));
    }
    text
}

/// Files that differ between a commit and the working tree, untracked ones
/// included and ignored ones not. Paths are relative to the project root,
/// which need not be the top of the repository.
#[derive(Debug, Default)]
struct Changes {
    added: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
}

impl Changes {
    async fn since(root: &Path, base: &str) -> Option<Self> {
        let diff = git(
            root,
            &[
                "diff",
                "--name-status",
                "--no-renames",
                "--relative",
                "-z",
                base,
            ],
        )
        .await?;
        let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
        let mut changes = Self::default();
        let mut fields = nul_fields(&diff).into_iter();
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            match status.as_str() {
                "A" => changes.added.push(path),
                "D" => changes.deleted.push(path),
                _ => changes.modified.push(path),
            }
        }
        changes.added.extend(nul_fields(&untracked));
        changes.forget_where(|path| path.starts_with(".wizard/"));
        Some(changes)
    }

    fn paths(&self) -> BTreeSet<String> {
        self.added
            .iter()
            .chain(&self.modified)
            .chain(&self.deleted)
            .cloned()
            .collect()
    }

    fn forget(&mut self, paths: &BTreeSet<String>) {
        self.forget_where(|path| paths.contains(path));
    }

    fn forget_where(&mut self, drop: impl Fn(&String) -> bool) {
        for list in [&mut self.added, &mut self.modified, &mut self.deleted] {
            list.retain(|path| !drop(path));
        }
    }

    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

fn nul_fields(raw: &[u8]) -> Vec<String> {
    raw.split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect()
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Run `git` in `root`: its stdout on exit 0, `None` on anything else.
async fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    output.status.success().then_some(output.stdout)
}

async fn head(root: &Path) -> Option<String> {
    let raw = git(root, &["rev-parse", "--verify", "HEAD"]).await?;
    Some(String::from_utf8_lossy(&raw).trim().to_string())
}

fn grep_command(
    root: &Path,
    flags: &[&str],
    needles: &[String],
    rev: Option<&str>,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["grep", "-I", "-F"])
        .args(flags);
    if rev.is_none() {
        command.arg("--untracked");
    }
    for needle in needles {
        command.arg("-e").arg(needle);
    }
    command.args(rev).arg("--").kill_on_drop(true);
    command
}

/// The `needles` that occur in the working tree (untracked files included) or
/// in `rev`, in one scan. Sound for what it returns and not complete: see the
/// caller. `None` when git did not answer.
async fn occurring(root: &Path, needles: &[String], rev: Option<&str>) -> Option<BTreeSet<String>> {
    let output = grep_command(root, &["-o", "-h"], needles, rev)
        .output()
        .await
        .ok()?;
    match output.status.code() {
        Some(0) => Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_string)
                .collect(),
        ),
        Some(1) => Some(BTreeSet::new()),
        _ => None,
    }
}

/// Does any of `needles` occur, as a fixed string, in the working tree
/// (untracked files included) or in `rev`? `None` when git did not answer
/// with a plain yes or no.
async fn grep(root: &Path, needles: &[String], rev: Option<&str>) -> Option<bool> {
    let output = grep_command(root, &["-q"], needles, rev)
        .output()
        .await
        .ok()?;
    match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// File extensions that make `some_name.ext` a file, not an identifier.
const FILE_EXTENSIONS: &[&str] = &[
    "c", "cfg", "cpp", "cs", "css", "csv", "go", "h", "hpp", "html", "ini", "java", "js", "json",
    "jsx", "lock", "log", "md", "proto", "py", "rb", "rs", "sh", "sql", "toml", "ts", "tsx", "txt",
    "xml", "yaml", "yml",
];

/// First words that make a multi-word backticked span code, not a shell
/// command. `cargo test --lib` names nothing the work has to contain; `fn
/// parse_all(input)` does.
const CODE_KEYWORDS: &[&str] = &[
    "async",
    "await",
    "class",
    "const",
    "def",
    "enum",
    "export",
    "fn",
    "func",
    "impl",
    "interface",
    "let",
    "new",
    "pub",
    "return",
    "static",
    "struct",
    "trait",
    "type",
    "var",
];

/// The identifiers and flags a request states, in order, without repeats.
///
/// Only shapes that are unmistakably code count: `snake_case`, `camelCase` or
/// `PascalCase` with at least two words, and `--long-flags`. Fenced code
/// blocks are skipped (an example's local variables are not requirements), and
/// so are paths, URLs, shell commands, placeholders like `<new_link>` or
/// `${name}`, `ENV_VAR=value`, file names, and test names, which a request
/// tends to quote from a suite the agent was never shown.
pub fn stated_names(request: &str) -> Vec<StatedName> {
    let mut names: Vec<StatedName> = Vec::new();
    let mut fenced = false;
    for line in request.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let segments: Vec<&str> = line.split('`').collect();
        let closed = segments.len() % 2 == 1;
        for (index, segment) in segments.iter().enumerate() {
            let marked = closed && index % 2 == 1;
            if marked && is_command(segment) {
                continue;
            }
            for (position, word) in segment.split_whitespace().enumerate() {
                scan_word(word, marked, marked && position == 0, &mut names);
            }
        }
    }
    names
}

fn is_command(span: &str) -> bool {
    let mut words = span.split_whitespace();
    let (Some(first), Some(_)) = (words.next(), words.next()) else {
        return false;
    };
    first
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !first.starts_with('-')
        && !CODE_KEYWORDS.contains(&first)
}

fn scan_word(word: &str, marked: bool, leads_span: bool, names: &mut Vec<StatedName>) {
    let mut push = |text: &str, marked: bool| match names.iter_mut().find(|n| n.text == text) {
        Some(seen) => seen.marked |= marked,
        None => names.push(StatedName {
            text: text.to_string(),
            marked,
        }),
    };
    if word.contains(['/', '\\', '@']) {
        return;
    }
    let bare = word.trim_start_matches(['(', '[', '"', '\'']);
    if let Some(body) = bare.strip_prefix("--") {
        let end = body
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
            .unwrap_or(body.len());
        let body = body[..end].trim_end_matches('-');
        if body.starts_with(|c: char| c.is_ascii_lowercase()) {
            push(&format!("--{body}"), leads_span);
        }
        return;
    }
    if bare.starts_with('-') {
        return;
    }
    static IDENT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("valid regex"));
    for token in IDENT.find_iter(word) {
        let before = word[..token.start()].chars().next_back();
        let after = &word[token.end()..];
        let name = token.as_str();
        if matches!(before, Some('<' | '{' | '$' | '%'))
            || (after.starts_with('=') && !name.contains(|c: char| c.is_ascii_lowercase()))
            || is_file_extension(after)
            || is_test_name(name)
            || !is_code_shaped(name)
        {
            continue;
        }
        push(name, marked);
    }
}

fn is_file_extension(after: &str) -> bool {
    let Some(rest) = after.strip_prefix('.') else {
        return false;
    };
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    FILE_EXTENSIONS.contains(&&rest[..end])
}

fn is_test_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("test_")
        || lower.ends_with("_test")
        || ["Test", "Benchmark", "Fuzz", "Example"]
            .iter()
            .any(|prefix| {
                name.strip_prefix(prefix).is_some_and(|rest| {
                    rest.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
                })
            })
}

/// Two or more words of two or more characters each, and not a date pattern
/// like `DD_HH`. Drops `vMAJOR`, `iOS` and `OAuth` along with every plain word.
fn is_code_shaped(name: &str) -> bool {
    let words = name_words(name);
    words.len() >= 2
        && words.iter().all(|word| word.len() >= 2)
        && !words.iter().all(|word| {
            let first = word.as_bytes()[0];
            word.bytes().all(|byte| byte == first)
        })
}

/// `maxRetryCount`, `max_retry_count`, `HTTPServer` and `--max-retry-count`
/// as lowercase words.
pub fn name_words(name: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for chunk in name.split(['_', '-']).filter(|chunk| !chunk.is_empty()) {
        let chars: Vec<char> = chunk.chars().collect();
        let mut word = String::new();
        for (index, &c) in chars.iter().enumerate() {
            let previous = index.checked_sub(1).map(|i| chars[i]);
            let next = chars.get(index + 1);
            let starts_word = c.is_uppercase()
                && previous.is_some_and(|p| {
                    !p.is_uppercase() || next.is_some_and(|next| next.is_lowercase())
                });
            if starts_word && !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            word.extend(c.to_lowercase());
        }
        if !word.is_empty() {
            words.push(word);
        }
    }
    words
}

/// Every spelling under which a stated name counts as present: as written,
/// and its words as snake, kebab, camel, Pascal and SCREAMING case. A clap
/// field `dry_run` is `--dry-run`, and a serde `fooBar` is a field `foo_bar`.
pub fn name_variants(name: &str) -> Vec<String> {
    let body = name.trim_start_matches('-');
    let words = name_words(body);
    let mut variants = vec![body.to_string()];
    if words.len() >= 2 {
        let capitalized: Vec<String> = words
            .iter()
            .map(|word| {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            })
            .collect();
        variants.extend([
            words.join("_"),
            words.join("-"),
            format!("{}{}", words[0], capitalized[1..].concat()),
            capitalized.concat(),
            words.join("_").to_uppercase(),
        ]);
    }
    variants.sort();
    variants.dedup();
    variants
}

/// Does the request ask for the work to be committed?
///
/// Deliberately narrow. "commit" is also what a transaction does and what a
/// hash names, so only the phrasings that address the agent count, and a
/// sentence that negates one ("do not commit your changes") counts for nothing.
pub fn asks_to_commit(request: &str) -> bool {
    static ASK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\bgit commit\b|\bcommit and push\b|\b(?:make|create) (?:a|one|a single|a new|a git) commit\b|\bcommit (?:everything|it all|when (?:you are|you're) (?:done|finished)|(?:all (?:of )?)?(?:your|the|these|those|all) (?:changes|work|results?|code|fix|fixes|files))\b",
        )
        .expect("valid regex")
    });
    static NEGATED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(?:do not|don't|dont|never|without|no need to|avoid|not to)\b")
            .expect("valid regex")
    });
    request
        .to_lowercase()
        .split(['.', '!', '?', ';', '\n'])
        .any(|sentence| ASK.is_match(sentence) && !NEGATED.is_match(sentence))
}

/// Does the request open with an instruction to change something?
pub fn opens_with_change_verb(request: &str) -> bool {
    const VERBS: &[&str] = &[
        "add",
        "change",
        "convert",
        "create",
        "delete",
        "extend",
        "fix",
        "implement",
        "introduce",
        "migrate",
        "port",
        "refactor",
        "remove",
        "rename",
        "replace",
        "rewrite",
        "support",
        "update",
        "write",
    ];
    let lower = request.to_lowercase();
    let mut words = lower
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|word| !word.is_empty());
    match words.next() {
        Some("please") => words.next().is_some_and(|word| VERBS.contains(&word)),
        Some(word) => VERBS.contains(&word),
        None => false,
    }
}

fn is_test_file(path: &str) -> bool {
    test_names(path, "").is_some()
}

/// The tests a file declares, or `None` for a file this cannot read as a test
/// file: Go `func TestX` in `_test.go`, Python `def test_x` and `class TestX`
/// in `test_*.py` or `*_test.py`, JS/TS `it`/`test`/`describe` titles written
/// as plain string literals, and Rust functions under a `#[test]` attribute.
pub fn test_names(path: &str, source: &str) -> Option<BTreeSet<String>> {
    static GO: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^func[ \t]+((?:Test|Benchmark|Fuzz|Example)\w*)[ \t]*\(")
            .expect("valid regex")
    });
    static PYTHON: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^[ \t]*(?:(?:async[ \t]+)?def[ \t]+(test\w*)|class[ \t]+(Test\w*))")
            .expect("valid regex")
    });
    static JS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"\b(?:it|test|describe)(?:\.(?:only|skip|concurrent|sequential|todo|fails))*\s*\(\s*(?:'((?:[^'\\\n]|\\.)+)'|"((?:[^"\\\n]|\\.)+)"|`([^`$\\\n]+)`)"#,
        )
        .expect("valid regex")
    });
    static RUST: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"#\[(?:\w+::)*test\b[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([a-z]+\))?\s+)?(?:async\s+)?fn\s+(\w+)",
        )
        .expect("valid regex")
    });
    let file = path.rsplit('/').next().unwrap_or(path);
    let (stem, extension) = file.rsplit_once('.')?;
    let pattern = match extension {
        "go" if stem.ends_with("_test") => &GO,
        "py" if stem.starts_with("test_") || stem.ends_with("_test") => &PYTHON,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts"
            if stem.ends_with(".test")
                || stem.ends_with(".spec")
                || path
                    .split('/')
                    .any(|dir| matches!(dir, "__tests__" | "test" | "tests")) =>
        {
            &JS
        }
        "rs" => &RUST,
        _ => return None,
    };
    Some(
        pattern
            .captures_iter(source)
            .filter_map(|found| found.iter().skip(1).flatten().next())
            .map(|name| name.as_str().to_string())
            .collect(),
    )
}

/// A Go file's package and the top-level names it declares, or `None` for a
/// file that only builds under some constraint (a build tag, or an OS or
/// architecture suffix), where a second declaration is the point.
pub fn go_declarations(path: &str, source: &str) -> Option<(String, BTreeSet<String>)> {
    const PLATFORMS: &[&str] = &[
        "386",
        "aix",
        "amd64",
        "android",
        "arm",
        "arm64",
        "darwin",
        "dragonfly",
        "freebsd",
        "illumos",
        "ios",
        "js",
        "linux",
        "loong64",
        "mips",
        "mips64",
        "netbsd",
        "openbsd",
        "plan9",
        "ppc64",
        "ppc64le",
        "riscv64",
        "s390x",
        "solaris",
        "unix",
        "wasip1",
        "wasm",
        "windows",
    ];
    static CONSTRAINT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^//[ \t]*(?:go:build|\+build)\b").expect("valid regex"));
    static PACKAGE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^package[ \t]+(\w+)").expect("valid regex"));
    static DECLARATION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^(?:func|type|var|const)[ \t]+([A-Za-z_]\w*)").expect("valid regex")
    });
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.strip_suffix(".go")?;
    let stem = stem.strip_suffix("_test").unwrap_or(stem);
    if stem
        .rsplit('_')
        .next()
        .is_some_and(|last| PLATFORMS.contains(&last))
        || CONSTRAINT.is_match(source)
    {
        return None;
    }
    let package = PACKAGE.captures(source)?[1].to_string();
    let names = DECLARATION
        .captures_iter(source)
        .map(|found| found[1].to_string())
        .filter(|name| name != "init" && name != "_")
        .collect();
    Some((package, names))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn names(request: &str) -> Vec<(String, bool)> {
        stated_names(request)
            .into_iter()
            .map(|name| (name.text, name.marked))
            .collect()
    }

    fn texts(request: &str) -> Vec<String> {
        names(request).into_iter().map(|(text, _)| text).collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn stated_names_are_the_code_shaped_tokens_and_flags() {
        assert_eq!(
            names(
                "Add `new_evaluation_handle()` and `Context::cancel_with_reason(reason)`. \
                 Expose Metrics.DurableCommitCount uint64 and a `--sort-seed <n>` flag; \
                 --dirs-first sorts directories first."
            ),
            vec![
                ("new_evaluation_handle".to_string(), true),
                ("cancel_with_reason".to_string(), true),
                ("DurableCommitCount".to_string(), false),
                ("--sort-seed".to_string(), true),
                ("--dirs-first".to_string(), false),
            ]
        );
    }

    #[test]
    fn plain_words_and_single_word_names_are_not_stated_names() {
        assert!(
            texts("Add `Encode` to `io.Writer` on `ShapeIndex`, like JSON does.") == ["ShapeIndex"]
        );
        assert!(texts("Use vMAJOR.MINOR tags, iOS, OAuth and YYYY_MM_DD dates.").is_empty());
    }

    #[test]
    fn what_a_request_quotes_but_does_not_require_is_skipped() {
        // Fenced examples, commands, paths, placeholders, env assignments,
        // file names and test names.
        let request = "Run `cargo test --all_targets` first.\n\
                       ```go\nmy_index := NewShapeIndex()\n```\n\
                       See `src/shape_index.go` and https://example.com/some_page.\n\
                       Append `, <new_link>` and `${launcher_name}_pass`.\n\
                       Set `RUST_BACKTRACE=1`, read `sample_data.csv`.\n\
                       `TestShapeIndexEncode` and `test_round_trip` must pass.";
        assert!(texts(request).is_empty(), "{:?}", texts(request));
    }

    #[test]
    fn a_backticked_declaration_is_code_not_a_command() {
        assert_eq!(texts("Add `fn parse_all(input: &str)`."), ["parse_all"]);
        assert_eq!(texts("Add `def load_config(path)`."), ["load_config"]);
        assert!(texts("Then `git checkout feature_branch`.").is_empty());
    }

    #[test]
    fn a_name_is_present_under_any_case_style() {
        assert_eq!(
            name_variants("--dry-run"),
            ["DRY_RUN", "DryRun", "dry-run", "dryRun", "dry_run"]
        );
        assert!(name_variants("HTTPServerConfig").contains(&"http_server_config".to_string()));
        assert!(name_variants("maxRetryCount").contains(&"MaxRetryCount".to_string()));
        assert_eq!(name_variants("--reverse"), ["reverse"]);
    }

    #[test]
    fn only_a_request_to_the_agent_asks_for_a_commit() {
        for asks in [
            "IMPORTANT: Please work on this in a new branch from main and commit everything when you are done.",
            "Fix it and commit your changes.",
            "Make a commit with the fix.",
            "Run `git commit` at the end.",
        ] {
            assert!(asks_to_commit(asks), "{asks}");
        }
        for does_not in [
            "Fires exactly once per Sync commit after the WAL sync completes.",
            "Validates invariants and commits only on success.",
            "Otherwise perform a 3-way merge and create a merge commit.",
            "The store must commit the transaction atomically.",
            "Do not commit your changes.",
            "Bisect from the base commit abc123.",
        ] {
            assert!(!asks_to_commit(does_not), "{does_not}");
        }
    }

    #[test]
    fn a_change_request_opens_with_a_change_verb() {
        assert!(opens_with_change_verb("Add retries to the client."));
        assert!(opens_with_change_verb("## Please fix the flaky test"));
        assert!(!opens_with_change_verb("What does this repo do?"));
        assert!(!opens_with_change_verb(
            "`ShapeIndex` lacks serialization. Add it."
        ));
    }

    #[test]
    fn tests_are_read_from_go_python_js_and_rust() {
        assert_eq!(
            test_names(
                "s2/index_test.go",
                "func TestEncode(t *testing.T) {}\nfunc helper() {}\nfunc BenchmarkEncode(b *testing.B) {}\nfunc (s *S) TestMethod() {}\n"
            ),
            Some(set(&["BenchmarkEncode", "TestEncode"]))
        );
        assert_eq!(
            test_names(
                "tests/test_cookies.py",
                "class TestJar:\n    def test_set(self):\n        pass\n\nasync def test_async():\n    pass\ndef helper():\n    pass\n"
            ),
            Some(set(&["TestJar", "test_async", "test_set"]))
        );
        assert_eq!(
            test_names(
                "tests/utils/report-file_tests.js",
                "describe('close', function() {\n  it(\"resolves when written\", () => {})\n  it.skip(`plain`, f)\n  it(`has ${x}`, f)\n  it(title, f)\n})"
            ),
            Some(set(&["close", "plain", "resolves when written"]))
        );
        assert_eq!(
            test_names(
                "src/lib.rs",
                "#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() {}\n    #[tokio::test]\n    #[ignore]\n    async fn waits() {}\n    fn helper() {}\n}\n"
            ),
            Some(set(&["adds", "waits"]))
        );
    }

    #[test]
    fn a_file_in_a_language_nobody_parses_here_says_nothing() {
        assert_eq!(
            test_names("spec/thing_spec.rb", "it 'works' do\nend\n"),
            None
        );
        assert_eq!(
            test_names("src/index.go", "func TestX(t *testing.T) {}"),
            None
        );
        assert_eq!(
            test_names("src/app.ts", "it('is not a test file', f)"),
            None
        );
    }

    #[test]
    fn go_declarations_are_the_top_level_names_of_an_unconstrained_file() {
        let source = "package s2\n\nfunc TestA(t *testing.T) {}\nfunc (x *T) Method() {}\ntype fixture struct{}\nvar shared = 1\nfunc init() {}\n";
        assert_eq!(
            go_declarations("s2/a_test.go", source),
            Some(("s2".to_string(), set(&["TestA", "fixture", "shared"])))
        );
        assert_eq!(go_declarations("s2/a_linux_test.go", source), None);
        assert_eq!(
            go_declarations(
                "s2/a_test.go",
                &format!("//go:build integration\n\n{source}")
            ),
            None
        );
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
        assert!(output.status.success(), "git {args:?} failed");
    }

    fn write(repo: &Path, path: &str, text: &str) {
        let path = repo.join(path);
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    /// A repo with one commit: a Go package with a test and a `.gitignore`.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q"]);
        write(dir.path(), ".gitignore", "ignored/\n");
        write(
            dir.path(),
            "s2/index.go",
            "package s2\n\nfunc Build() {}\nfunc old_helper() {}\n",
        );
        write(
            dir.path(),
            "s2/index_test.go",
            "package s2\n\nfunc TestBuild(t *testing.T) {}\nfunc TestQuery(t *testing.T) {}\n",
        );
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "base"]);
        dir
    }

    async fn checks(repo: &Path, request: &str) -> FinishChecks {
        let config = Config {
            mode: Mode::Sovereign,
            ..Config::default()
        };
        let cli = Cli::try_parse_from(["wizard", "-p", request]).expect("valid args");
        FinishChecks::for_run(&config, &cli, repo, request)
            .await
            .expect("finish checks apply")
    }

    #[tokio::test]
    async fn a_clean_run_finds_nothing() {
        let dir = repo();
        let request = "Add `encode_index` to the s2 package and commit your changes.";
        let checks = checks(dir.path(), request).await;
        write(
            dir.path(),
            "s2/encode.go",
            "package s2\n\nfunc EncodeIndex() {}\n",
        );
        write(
            dir.path(),
            "s2/encode_test.go",
            "package s2\n\nfunc TestEncode(t *testing.T) {}\n",
        );
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "work"]);
        write(dir.path(), ".wizard/checkpoints/1.json", "{}");
        assert_eq!(checks.inspect().await, Some(Findings::default()));
    }

    #[tokio::test]
    async fn each_check_names_what_it_found() {
        let dir = repo();
        let request = "Add `encode_index` and `--strict-mode` to the s2 package, remove \
                       `old_helper`, and commit your changes.";
        let checks = checks(dir.path(), request).await;
        write(dir.path(), "s2/index.go", "package s2\n\nfunc Build() {}\n");
        write(
            dir.path(),
            "s2/encode.go",
            "package s2\n\nfunc Serialize() {}\n",
        );
        write(
            dir.path(),
            "s2/index_test.go",
            "package s2\n\nfunc TestBuild(t *testing.T) {}\nfunc TestQueryRenamed(t *testing.T) {}\n",
        );
        write(
            dir.path(),
            "s2/encode_test.go",
            "package s2\n\nfunc TestBuild(t *testing.T) {}\n",
        );
        write(dir.path(), "ignored/scratch.txt", "not source\n");

        let found = checks.inspect().await.expect("git answered");
        assert_eq!(found.missing_names, ["encode_index", "--strict-mode"]);
        assert_eq!(
            found.vanished_tests,
            [("TestQuery".to_string(), "s2/index_test.go".to_string())]
        );
        assert!(
            found
                .uncommitted
                .as_deref()
                .is_some_and(|text| text.contains("nothing has been committed"))
        );
        assert_eq!(
            found.duplicates,
            [(
                "TestBuild".to_string(),
                "s2/encode_test.go".to_string(),
                "s2/index_test.go".to_string()
            )]
        );
        let prompt = found.prompt().expect("there are findings");
        for needle in [
            "`encode_index`",
            "`TestQuery` (s2/index_test.go)",
            "s2/encode_test.go and s2/index_test.go",
        ] {
            assert!(prompt.contains(needle), "{needle} missing from:\n{prompt}");
        }

        // Committed but one file left out.
        git(dir.path(), &["add", "-u"]);
        git(dir.path(), &["add", "s2/encode.go"]);
        git(dir.path(), &["commit", "-q", "-m", "work"]);
        let found = checks.inspect().await.expect("git answered");
        assert_eq!(
            found.uncommitted.as_deref(),
            Some(
                "The request asks for a commit, and these are not committed: `s2/encode_test.go`."
            )
        );
    }

    #[tokio::test]
    async fn a_check_fires_once_and_the_others_stay_armed() {
        let dir = repo();
        let mut checks = checks(dir.path(), "Add `encode_index` and commit your changes.").await;
        let (tx, mut rx) = mpsc::channel(8);

        let prompt = checks.check(&tx).await.expect("nothing was committed");
        assert!(prompt.contains("nothing has been committed"));
        assert!(matches!(
            rx.try_recv(),
            Ok(AgentEvent::Notice(notice)) if notice.starts_with("finish check: ")
        ));
        // Still uncommitted: said once, not said again.
        assert_eq!(checks.check(&tx).await, None);

        write(
            dir.path(),
            "s2/encode.go",
            "package s2\n\nfunc Serialize() {}\n",
        );
        let prompt = checks.check(&tx).await.expect("the stated name is missing");
        assert!(prompt.contains("`encode_index`"));
        assert!(!prompt.contains("nothing has been committed"));
        assert_eq!(checks.check(&tx).await, None);
    }

    #[tokio::test]
    async fn a_name_counts_when_any_file_or_the_starting_commit_has_it() {
        let dir = repo();
        let request =
            "Use `Build`, add `encode_index`, `report_writer` and `max_cells`; drop `TestQuery`.";
        let checks = checks(dir.path(), request).await;
        // A different case style, an untracked file's path, and nothing for
        // the third.
        write(
            dir.path(),
            "s2/encode.go",
            "package s2\n\nfunc EncodeIndex() {}\n",
        );
        write(dir.path(), "s2/report-writer.go", "package s2\n");
        let found = checks.inspect().await.expect("git answered");
        assert_eq!(found.missing_names, ["max_cells"]);
    }

    #[tokio::test]
    async fn one_unmarked_missing_name_is_not_a_finding() {
        let dir = repo();
        let checks = checks(dir.path(), "Port the JavaScript helper to Go.").await;
        write(dir.path(), "s2/helper.go", "package s2\n");
        assert_eq!(checks.inspect().await, Some(Findings::default()));
    }

    #[tokio::test]
    async fn what_was_already_dirty_is_not_the_agents() {
        let dir = repo();
        write(
            dir.path(),
            "s2/index_test.go",
            "package s2\n\nfunc TestBuild(t *testing.T) {}\n",
        );
        write(dir.path(), "notes.txt", "mine\n");
        let checks = checks(dir.path(), "Add `encode_index` and commit your changes.").await;
        write(
            dir.path(),
            "s2/encode.go",
            "package s2\n\nfunc EncodeIndex() {}\n",
        );
        git(dir.path(), &["add", "s2/encode.go"]);
        git(dir.path(), &["commit", "-q", "-m", "work"]);
        assert_eq!(checks.inspect().await, Some(Findings::default()));
    }

    #[tokio::test]
    async fn a_project_root_below_the_top_of_the_repository_reads_its_own_paths() {
        let dir = repo();
        let root = dir.path().join("s2");
        let checks = checks(&root, "Add `encode_index` and commit your changes.").await;
        write(
            &root,
            "index_test.go",
            "package s2\n\nfunc TestBuild(t *testing.T) {}\n",
        );
        write(&root, "encode.go", "package s2\n\nfunc EncodeIndex() {}\n");
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "work"]);
        write(&root, "left.go", "package s2\n");

        let found = checks.inspect().await.expect("git answered");
        assert_eq!(
            found.vanished_tests,
            [("TestQuery".to_string(), "index_test.go".to_string())]
        );
        assert_eq!(
            found.uncommitted.as_deref(),
            Some("The request asks for a commit, and these are not committed: `left.go`.")
        );
    }

    #[tokio::test]
    async fn an_untouched_tree_on_a_change_request_is_said_once_and_alone() {
        let dir = repo();
        let mut checks = checks(dir.path(), "Add `encode_index` to the s2 package.").await;
        let (tx, _rx) = mpsc::channel(8);
        let prompt = checks.check(&tx).await.expect("nothing changed");
        assert!(prompt.contains("identical to the commit this run started from"));
        assert!(!prompt.contains("encode_index"));
        assert_eq!(checks.check(&tx).await, None);
    }

    #[tokio::test]
    async fn the_checks_are_for_an_unattended_request_in_a_git_repo() {
        let dir = repo();
        let sovereign = Config {
            mode: Mode::Sovereign,
            ..Config::default()
        };
        let cli = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("wizard").chain(args.iter().copied()))
                .expect("valid args")
        };
        let applies = async |config: &Config, cli: &Cli, root: &Path| {
            FinishChecks::for_run(config, cli, root, "Add it.")
                .await
                .is_some()
        };
        let prompt = cli(&["-p", "Add it."]);
        assert!(applies(&sovereign, &prompt, dir.path()).await);
        assert!(!applies(&Config::default(), &prompt, dir.path()).await);
        assert!(!applies(&sovereign, &cli(&[]), dir.path()).await);
        for off in [
            Config {
                finish_checks: false,
                ..sovereign.clone()
            },
            Config {
                continuous: true,
                ..sovereign.clone()
            },
        ] {
            assert!(!applies(&off, &prompt, dir.path()).await);
        }
        let plain = tempfile::tempdir().expect("tempdir");
        assert!(!applies(&sovereign, &prompt, plain.path()).await);
    }

    /// Replay over recorded benchmark trials: how many failed and how many
    /// passing ones each check flags. `WIZARD_FINISH_REPLAY` names a directory
    /// of trials, each holding `repo/` (the tree the agent left), `base` (the
    /// starting commit), `request.md` and `passed` (`1` or `0`).
    #[tokio::test]
    #[ignore = "needs WIZARD_FINISH_REPLAY pointing at prepared trials"]
    async fn replay_recorded_trials() {
        let root = std::env::var("WIZARD_FINISH_REPLAY").expect("WIZARD_FINISH_REPLAY is set");
        let mut trials: Vec<PathBuf> = std::fs::read_dir(root)
            .expect("replay dir")
            .map(|entry| entry.expect("entry").path())
            .collect();
        trials.sort();
        let read = |path: PathBuf| std::fs::read_to_string(path).expect("trial file");
        // [failed, passed] trial counts, then per check.
        let mut totals = [0usize; 2];
        let mut flagged = [[0usize; 2]; 5];
        for trial in trials {
            let passed = usize::from(read(trial.join("passed")).trim() == "1");
            let checks = FinishChecks {
                root: trial.join("repo"),
                request: read(trial.join("request.md")),
                base: read(trial.join("base")).trim().to_string(),
                dirty_at_start: BTreeSet::new(),
                fired: Vec::new(),
            };
            let began = std::time::Instant::now();
            let found = checks.inspect().await.expect("git answered");
            totals[passed] += 1;
            let hits = [
                !found.missing_names.is_empty(),
                !found.vanished_tests.is_empty(),
                found.uncommitted.is_some(),
                !found.duplicates.is_empty(),
                found.prompt().is_some(),
            ];
            for (index, hit) in hits.into_iter().enumerate() {
                flagged[index][passed] += usize::from(hit);
            }
            if found.prompt().is_some() {
                println!(
                    "{} {} {:?}\n    names {:?}\n    tests {:?}\n    commit {:?}\n    dups {:?}",
                    if passed == 1 { "PASSED" } else { "FAILED" },
                    trial.file_name().expect("name").to_string_lossy(),
                    began.elapsed(),
                    found.missing_names,
                    found.vanished_tests,
                    found.uncommitted,
                    found.duplicates,
                );
            }
        }
        println!("trials: {} failed, {} passed", totals[0], totals[1]);
        for (name, [failed, passed]) in ["names", "tests", "commit", "duplicates", "any"]
            .into_iter()
            .zip(flagged)
        {
            println!("{name}: flags {failed} failed, {passed} passed");
        }
    }
}
