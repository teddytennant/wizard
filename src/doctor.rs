//! Environment diagnostics: `wizard doctor` (CLI) and `/doctor` (TUI).
//!
//! Runs a battery of checks — config parses, providers reachable, MCP
//! servers handshake, tools registered, hooks parse, state directories
//! writable, checkpoint index sane — and prints one `✓` / `✗` / `–` line
//! per check. Provider probes are capped at [`PROBE_TIMEOUT`] and MCP
//! handshakes at the runtime's own [`crate::mcp::CONNECT_TIMEOUT`], so
//! doctor can never hang. The CLI exits 0 when nothing failed, 1 otherwise;
//! skipped (`–`) checks are not failures.
//!
//! Bundle mode ([`run_bundle`]) turns the same run into a bug report: a
//! redacted directory holding the version, the redacted config, the last
//! session transcript, the usage and evolution logs, and recent debug logs.
//! Redaction is structural, see [`redact_config_toml`] and [`scrub_text`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{Config, ProviderConfig};
use crate::tools::registry::ToolRegistry;

/// Cap on every provider health probe. MCP handshakes use
/// [`crate::mcp::CONNECT_TIMEOUT`] instead — the same budget the runtime
/// allows, so a slow-starting `npx`/`uvx` server that works in the app does
/// not fail doctor.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `✓` — works.
    Pass,
    /// `✗` — broken; the doctor run exits 1.
    Fail,
    /// `–` — not applicable / nothing to check (missing optional file,
    /// unset API key).
    Skip,
}

/// One check result: a label, an outcome, and a short detail.
#[derive(Debug, Clone)]
pub struct Check {
    pub label: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn pass(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: Status::Pass,
            detail: detail.into(),
        }
    }

    fn fail(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: Status::Fail,
            detail: detail.into(),
        }
    }

    fn skip(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: Status::Skip,
            detail: detail.into(),
        }
    }
}

/// Render checks as aligned report lines.
pub fn render(checks: &[Check]) -> String {
    let width = checks
        .iter()
        .map(|check| check.label.chars().count())
        .max()
        .unwrap_or(0);
    checks
        .iter()
        .map(|check| {
            let mark = match check.status {
                Status::Pass => "✓",
                Status::Fail => "✗",
                Status::Skip => "–",
            };
            format!("{mark} {:<width$}  {}", check.label, check.detail)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True when any check failed (drives the exit code).
pub fn has_failures(checks: &[Check]) -> bool {
    checks.iter().any(|check| check.status == Status::Fail)
}

// ---------------------------------------------------------------------------
// pure checks (unit-tested)
// ---------------------------------------------------------------------------

/// `config.toml` parses. A missing file is fine: defaults apply.
pub fn check_config_file(path: &Path) -> Check {
    let label = "config";
    match std::fs::read_to_string(path) {
        Ok(raw) => match toml::from_str::<Config>(&raw) {
            Ok(_) => Check::pass(label, format!("{} parses", path.display())),
            Err(err) => Check::fail(label, format!("{}: {err}", path.display())),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Check::skip(label, format!("{} absent (defaults apply)", path.display()))
        }
        Err(err) => Check::fail(label, format!("{}: {err}", path.display())),
    }
}

/// One `hooks.toml` parses. Missing file means no hooks — fine.
pub fn check_hooks_file(label: &str, path: &Path) -> Check {
    match std::fs::read_to_string(path) {
        Ok(raw) => match crate::hooks::parse(&raw) {
            Ok(hooks) => Check::pass(
                label,
                format!("{} hook(s) in {}", hooks.len(), path.display()),
            ),
            Err(err) => Check::fail(label, format!("{}: {err}", path.display())),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Check::skip(label, format!("{} absent (no hooks)", path.display()))
        }
        Err(err) => Check::fail(label, format!("{}: {err}", path.display())),
    }
}

/// `dir` exists (created if needed) and accepts a probe file.
pub fn check_writable(label: &str, dir: &Path) -> Check {
    if let Err(err) = std::fs::create_dir_all(dir) {
        return Check::fail(label, format!("cannot create {}: {err}", dir.display()));
    }
    let probe = dir.join(format!(".doctor-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::pass(label, format!("{} writable", dir.display()))
        }
        Err(err) => Check::fail(label, format!("{} not writable: {err}", dir.display())),
    }
}

/// The checkpoint index parses, and every snapshot directory under
/// `.wizard/checkpoints/` belongs to an indexed turn (stale directories are
/// left over from interrupted rewinds/gc and are reported but harmless).
pub fn check_checkpoints(project_root: &Path) -> Check {
    let label = "checkpoints";
    let root = project_root.join(".wizard").join("checkpoints");
    let index = root.join("index.jsonl");
    let raw = match std::fs::read_to_string(&index) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Check::skip(label, "no checkpoint index yet".to_string());
        }
        Err(err) => return Check::fail(label, format!("{}: {err}", index.display())),
    };
    let mut turns = std::collections::BTreeSet::new();
    let mut records = 0usize;
    let mut corrupt = 0usize;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<crate::checkpoint::SnapshotRecord>(line) {
            Ok(record) => {
                turns.insert(record.turn);
                records += 1;
            }
            Err(_) => corrupt += 1,
        }
    }
    if corrupt > 0 {
        return Check::fail(
            label,
            format!("{corrupt} corrupt line(s) in {}", index.display()),
        );
    }
    // Numeric subdirectories not referenced by any index record are stale.
    let stale = std::fs::read_dir(&root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
                .filter(|turn| !turns.contains(turn))
                .count()
        })
        .unwrap_or(0);
    let mut detail = format!("{records} snapshot(s) across {} turn(s)", turns.len());
    if stale > 0 {
        detail.push_str(&format!(", {stale} stale snap dir(s)"));
    }
    Check::pass(label, detail)
}

/// `active_provider` in `config.toml` names a configured provider. An unknown
/// name (typo, removed provider) silently falls back to the first provider,
/// so the user would run against a different backend without noticing.
pub fn check_active_provider(config: &Config) -> Check {
    let label = "active provider";
    match config.active_provider_mismatch() {
        Some(name) => Check::fail(
            label,
            format!(
                "active_provider '{name}' matches no configured provider; \
                 falling back to '{}'",
                config.active().name
            ),
        ),
        None => Check::pass(label, format!("'{}'", config.active().name)),
    }
}

/// `credentials.toml` parses cleanly and is not readable by other local users.
/// Normal reads degrade a corrupt file to "no stored keys", which silently
/// breaks every provider relying on a stored key, so doctor surfaces it.
///
/// The permission half asks [`crate::platform::secrets::is_protected`] rather
/// than reading mode bits here: this file *is* Wizard's secret storage (no
/// keyring, no encryption, just an owner-only file), so "who else can read it"
/// has to be a question every platform answers, not one only Unix can.
pub fn check_credentials_file(path: &Path) -> Check {
    let label = "credentials";
    if !path.exists() {
        return Check::skip(label, format!("{} absent (no stored keys)", path.display()));
    }
    let count = match crate::credentials::parse_strict(path) {
        Ok(count) => count,
        Err(err) => return Check::fail(label, format!("{err:#}")),
    };
    match crate::platform::secrets::is_protected(path) {
        Ok(true) => {}
        Ok(false) => {
            return Check::fail(
                label,
                format!(
                    "{} is readable by other users on this machine; it holds plaintext \
                     keys (chmod 600 it)",
                    path.display()
                ),
            );
        }
        // "This platform has no answer yet" is not "the file is exposed": the
        // Windows arm of [`crate::platform::secrets`] reports `Unsupported`
        // until its ACL support lands, and failing on it would fail every run
        // on that platform with no chmod its user could run to clear it.
        // Reporting a pass would be worse: it would claim a protection nothing
        // verified. So the permission half is skipped and says so.
        Err(err) if is_unsupported(&err) => {
            return Check::skip(
                label,
                format!(
                    "{count} stored key(s); this platform cannot report who else can \
                     read {} ({err:#})",
                    path.display()
                ),
            );
        }
        Err(err) => return Check::fail(label, format!("{err:#}")),
    }
    Check::pass(label, format!("{count} stored key(s), permissions ok"))
}

/// True when an error chain bottoms out in "this platform does not implement
/// that", which [`crate::platform::secrets`] reports for every mode-bit
/// question off unix.
///
/// Matched through the whole chain rather than on the outermost error, because
/// the seam attaches a `with_context` path to it before we see it.
fn is_unsupported(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::Unsupported)
    })
}

/// Every other path under `~/.wizard` that holds a secret is owner-only too.
///
/// [`check_credentials_file`] answers for the credential store; this answers
/// for everything sitting beside it that is just as sensitive and has no other
/// check: the state directory itself (session transcripts, memory, the running
/// registry), `logs/` (request traces, which carry headers), and the OAuth
/// token files, whose refresh tokens are longer-lived than an API key.
///
/// One check rather than one per path: a user who has to widen four modes wants
/// them named together, and a fresh install has none of the optional files.
/// Absent paths are skipped, never failed, and a path that cannot be inspected
/// at all is a failure rather than a silent pass (see
/// [`crate::platform::secrets::is_protected`]).
///
/// A loose tree is only a *failure* when the filesystem could carry the fix;
/// see [`filesystem_can_restrict`] for why, and [`secret_storage_verdict`] for
/// the rest of the policy.
pub fn check_secret_storage(wizard_dir: &Path) -> Check {
    let candidates = [
        wizard_dir.to_path_buf(),
        wizard_dir.join("logs"),
        wizard_dir.join("sessions"),
        wizard_dir.join("xai_oauth.json"),
        wizard_dir.join("chatgpt_oauth.json"),
    ];
    let states: Vec<(PathBuf, PathState)> = candidates
        .into_iter()
        .map(|path| {
            let state = inspect_path(&path);
            (path, state)
        })
        .collect();
    // The probe writes a directory, so it only runs when something is actually
    // loose: that is the one verdict whose wording and exit code turn on
    // whether this filesystem could ever carry the fix.
    let loose = states
        .iter()
        .any(|(_, state)| matches!(state, PathState::Loose));
    let restrictable = !loose || filesystem_can_restrict(wizard_dir);
    secret_storage_verdict(wizard_dir, &states, restrictable)
}

/// What one candidate path answered when asked who else can read it.
#[derive(Debug)]
enum PathState {
    /// Not there. A fresh install has none of the optional files.
    Absent,
    /// Owner-only.
    Protected,
    /// Readable by other local users.
    Loose,
    /// The platform has no answer (the Windows arm of
    /// [`crate::platform::secrets`] until its ACL support lands). Neither a
    /// pass, which would claim a protection nothing verified, nor a failure,
    /// which the user could not clear.
    Unknown(String),
    /// The path is there and could not be inspected at all.
    Error(String),
}

/// Ask one path who else can read it, keeping "not there" apart from "could
/// not look".
///
/// `Path::exists` is defined as `metadata().is_ok()`, so filtering candidates
/// on it reports a state directory whose parent lost its search bit (a
/// restored backup, an NFS mount that lost its export, a `chmod 600 $HOME`) as
/// *absent*: the user is told their transcripts are missing when they are
/// present and unreadable. `try_exists` asks the same question and keeps the
/// error, which is the whole difference.
fn inspect_path(path: &Path) -> PathState {
    match path.try_exists() {
        Ok(false) => PathState::Absent,
        Ok(true) => match crate::platform::secrets::is_protected(path) {
            Ok(true) => PathState::Protected,
            Ok(false) => PathState::Loose,
            Err(err) if is_unsupported(&err) => PathState::Unknown(format!("{err:#}")),
            Err(err) => PathState::Error(format!("{err:#}")),
        },
        Err(err) => PathState::Error(format!("inspecting {}: {err}", path.display())),
    }
}

/// Whether the filesystem under `dir` can express owner-only permissions at
/// all.
///
/// Asked by having the platform layer create one private directory there and
/// reading back what the filesystem actually gave us, because that is the same
/// primitive `Config::ensure_dirs` uses and this verdict has to agree with it.
/// [`crate::platform::secrets::create_private_dir`] deliberately *warns*
/// rather than fails on exFAT, FAT32, WSL DrvFs and shares without POSIX modes
/// (relocating the tree with `WIZARD_HOME` onto one of those is supported), so
/// a doctor check that failed the exit code over the same tree would fail it
/// on every run, forever, advising a `chmod` the filesystem cannot honour.
/// `wizard doctor && wizard -p "task"` is documented as a preflight, so that
/// is a user who can never run the second command.
fn filesystem_can_restrict(dir: &Path) -> bool {
    let probe = dir.join(format!(".doctor-perm-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&probe);
    let restricted = crate::platform::secrets::create_private_dir_strict(&probe).is_ok()
        && crate::platform::secrets::is_protected(&probe).unwrap_or(false);
    let _ = std::fs::remove_dir_all(&probe);
    restricted
}

/// Turn the per-path answers into the one line the report prints.
///
/// Pure, and separate from the paths it describes, so the states no test can
/// produce on a normal box (an un-stattable path, a platform with no answer, a
/// filesystem that cannot carry modes) are all reachable from a test.
fn secret_storage_verdict(
    wizard_dir: &Path,
    states: &[(PathBuf, PathState)],
    restrictable: bool,
) -> Check {
    let label = "secret storage";
    let mut protected = 0usize;
    let mut loose: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (path, state) in states {
        match state {
            PathState::Absent => {}
            PathState::Protected => protected += 1,
            PathState::Loose => loose.push(path.display().to_string()),
            PathState::Unknown(err) => unknown.push(err.clone()),
            PathState::Error(err) => errors.push(err.clone()),
        }
    }

    if !errors.is_empty() {
        return Check::fail(label, errors.join("; "));
    }
    if !loose.is_empty() {
        let loose = loose.join(", ");
        return if restrictable {
            Check::fail(
                label,
                format!(
                    "readable by other users on this machine: {loose} (chmod 700 the \
                     directories, 600 the token files)"
                ),
            )
        } else {
            Check::skip(
                label,
                format!(
                    "readable by other users on this machine: {loose}; this filesystem \
                     cannot express owner-only permissions (exFAT, FAT32, WSL DrvFs, a \
                     share without POSIX modes), so no chmod would fix it; move \
                     WIZARD_HOME onto a filesystem that can if the machine is shared"
                ),
            )
        };
    }
    if !unknown.is_empty() {
        return Check::skip(
            label,
            format!(
                "{} path(s) could not be checked: {}",
                unknown.len(),
                unknown.join("; ")
            ),
        );
    }
    if protected == 0 {
        return Check::skip(label, format!("{} absent", wizard_dir.display()));
    }
    Check::pass(label, format!("{protected} path(s) owner-only"))
}

/// What the system prompt costs, section by section.
///
/// Never a failure: it is a measurement, and the one every "why is my context
/// full after two turns" report needs. The breakdown cannot drift from the
/// prompt it describes because it is built from the same
/// [`crate::agent::prompts::PromptSection`] list the agent sends, which is why
/// that list exists in the first place.
///
/// Skills, `AGENTS.md` and the memory index are left out: they belong to a
/// session and a working directory, not to an install, so this measures the
/// baked prompt every run starts from. `cache_breakpoint` is the byte offset a
/// provider-side prompt cache should be cut at, and it is here because a
/// number nobody can see is a number nobody maintains.
pub fn check_system_prompt(mode: crate::config::Mode) -> Check {
    let sections = crate::agent::prompts::system_prompt_sections(mode, &[], None, None);
    let total = crate::agent::prompts::join_sections(&sections).len();
    let cached = crate::agent::prompts::cache_breakpoint(&sections);
    let tokens: u64 = sections.iter().map(|section| section.est_tokens()).sum();
    let breakdown = sections
        .iter()
        .map(|section| format!("{} {}", section.name, kib(section.bytes())))
        .collect::<Vec<_>>()
        .join(", ");
    Check::pass(
        "system prompt",
        format!(
            "{} section(s), {}, ~{tokens} tokens; cacheable through {}; {breakdown}",
            sections.len(),
            kib(total),
            kib(cached),
        ),
    )
}

/// Sizes in KiB to one decimal. The report is read by a human comparing
/// sections, not parsed.
fn kib(bytes: usize) -> String {
    format!("{:.1} KiB", bytes as f64 / 1024.0)
}

/// The native tool set is compiled in and registered.
pub fn check_native_tools() -> Check {
    let count = ToolRegistry::with_native_tools().len();
    if count == 0 {
        Check::fail("native tools", "no native tools registered")
    } else {
        Check::pass("native tools", format!("{count} tools registered"))
    }
}

/// Surface host-specific constraints (Termux has no prebuilt asset; doctor
/// should say so rather than leaving the user to discover a broken
/// `wizard update` or Local install).
pub fn check_platform() -> Check {
    let label = "platform";
    if crate::platform::is_termux() {
        return Check::pass(
            label,
            "Termux (Android): source-built binary expected; prebuilt \
             `wizard update` and stock llama-server assets are unavailable — \
             use a cloud provider or a Termux-built llama-server",
        );
    }
    if crate::platform::is_nixos() {
        return Check::pass(
            label,
            "NixOS: prefer the flake (`nix profile install github:teddytennant/wizard`); \
             musl prebuilts are the curl-installer fallback",
        );
    }
    Check::pass(
        label,
        format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
    )
}

/// The colour depth the UI will paint at, and the environment that decided it.
///
/// This is the migration path 2.0.0's changelog offers for the removed
/// `/theme`: the depth was the one genuinely useful thing that command
/// reported, and it is the answer to "why is Wizard monochrome on this box"
/// — a question whose answer is always an environment variable
/// ([`ColorDepth::from_env`] ranks them). Always a pass: a 16-colour terminal
/// is a fact about the host, not a fault, and the UI never encodes meaning in
/// colour alone.
///
/// [`ColorDepth::from_env`]: crate::theme::ColorDepth::from_env
pub fn check_color_depth() -> Check {
    let depth = crate::theme::ColorDepth::detect();
    let set: Vec<String> = ["NO_COLOR", crate::theme::ENV_COLOR, "COLORTERM", "TERM"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| format!("{name}={value}"))
        })
        .collect();
    let detail = if set.is_empty() {
        "nothing in the environment says otherwise".to_string()
    } else {
        set.join(", ")
    };
    Check::pass("color depth", format!("{} ({detail})", depth.label()))
}

/// Messaging gateway configuration and token presence. Never prints the
/// secret. Warns when a telegram token is stored but `gateway.kind` is
/// still `none`, when the allow-list is empty (which now refuses every
/// message), and when kind is telegram but no process appears to be
/// listening.
pub fn check_gateway(config: &Config) -> Vec<Check> {
    let mut checks = Vec::new();
    let kind = config.gateway.kind;
    let token_in_credentials =
        crate::credentials::get("telegram").is_some_and(|t| !t.trim().is_empty());
    let env_name = config.gateway.token_env();
    let token_in_env = std::env::var(env_name)
        .ok()
        .is_some_and(|t| !t.trim().is_empty());
    let token_present = token_in_credentials || token_in_env;

    match kind {
        crate::config::GatewayKind::None => {
            if token_in_credentials {
                checks.push(Check::fail(
                    "gateway",
                    "token stored under [keys] telegram but gateway.kind is \"none\" \
                     — set kind = \"telegram\" in config.toml (or re-run wizard --onboard)",
                ));
            } else {
                checks.push(Check::skip(
                    "gateway",
                    "kind = none (terminal only; set kind = \"telegram\" to enable)",
                ));
            }
        }
        crate::config::GatewayKind::Telegram => {
            checks.push(Check::pass("gateway", "kind = telegram"));
            if token_present {
                let source = if token_in_credentials {
                    "credentials.toml"
                } else {
                    env_name
                };
                checks.push(Check::pass(
                    "gateway token",
                    format!("present ({source}; secret not shown)"),
                ));
            } else {
                checks.push(Check::fail(
                    "gateway token",
                    format!(
                        "missing — paste during `wizard --onboard`, store under [keys] \
                         telegram in ~/.wizard/credentials.toml, or export {env_name}"
                    ),
                ));
            }
            checks.push(check_gateway_allow_list(config));
            checks.push(check_gateway_process());
        }
    }
    checks
}

/// `gateway.allowed_chat_ids` names at least one chat.
///
/// The list is a closed allow-list (see [`crate::config::GatewayConfig`]):
/// empty means nobody is authorized, so a telegram gateway with no ids
/// refuses every inbound message and the bot answers nobody. That used to be
/// "allow all", which is why an existing config can still carry an empty list
/// and why this is a hard failure rather than a note: the two readings differ
/// in exactly the way a user notices only when the bot goes silent.
pub fn check_gateway_allow_list(config: &Config) -> Check {
    let label = "gateway allow-list";
    let ids = &config.gateway.allowed_chat_ids;
    if ids.is_empty() {
        return Check::fail(
            label,
            "gateway.allowed_chat_ids is empty, so every inbound message is refused \
             and the bot replies to nobody; run `wizard gateway setup` to discover \
             your chat id and add it under [gateway] in ~/.wizard/config.toml",
        );
    }
    // The group-chat note, in the words `crate::config::group_chat_warning`
    // gives every surface that has to say it. `pass`, not a failure: allow-
    // listing a group is a legitimate thing to configure deliberately and
    // doctor must not refuse a working setup. But it is the check most worth
    // surfacing here, because after setup nothing else ever tells the operator.
    if let Some(warning) = crate::config::group_chat_warning(ids) {
        return Check::pass(
            label,
            format!("{} chat id(s) allowed — note: {warning}", ids.len()),
        );
    }
    Check::pass(label, format!("{} chat id(s) allowed", ids.len()))
}

/// Best-effort: is a `wizard --gateway` process running on this machine?
/// Uses `pgrep -af`; a missing `pgrep` is a skip, not a failure.
#[cfg(test)]
mod gateway_allow_list_tests {
    use super::*;

    /// A group id in the allow-list is called out.
    ///
    /// The allow-list authorises a *chat*, not a person — the inbound message
    /// type does not carry the sender at all — so a group id admits every
    /// member of that group, now and later, and anyone in it who can add people
    /// grants that too. Given an allowed message runs a sovereign turn with
    /// `execute`, this is the only place that ever says so.
    #[test]
    fn a_group_chat_id_is_called_out() {
        let mut config = Config::default();
        config.gateway.allowed_chat_ids = vec![-1001234567890];
        let check = check_gateway_allow_list(&config);
        assert_eq!(
            check.status,
            Status::Pass,
            "a group id is legal, not a failure"
        );
        assert!(check.detail.contains("group chats"), "{}", check.detail);
        assert!(
            check.detail.contains("full tool access"),
            "{}",
            check.detail
        );

        // A one-to-one id says nothing extra.
        config.gateway.allowed_chat_ids = vec![123456789];
        let check = check_gateway_allow_list(&config);
        assert_eq!(check.status, Status::Pass);
        assert!(!check.detail.contains("group"), "{}", check.detail);

        // And an empty list is still the hard failure it was.
        config.gateway.allowed_chat_ids = vec![];
        assert_eq!(check_gateway_allow_list(&config).status, Status::Fail);
    }
}

pub fn check_gateway_process() -> Check {
    let label = "gateway process";
    let output = std::process::Command::new("pgrep")
        .args(["-af", "wizard"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let listening = stdout.lines().any(|line| {
                // Match the gateway flag without matching this doctor process.
                (line.contains("--gateway") || line.contains(" wizard-gateway"))
                    && !line.contains("pgrep")
            });
            if listening {
                Check::pass(label, "wizard --gateway appears to be running")
            } else {
                Check::fail(
                    label,
                    "no wizard --gateway process found — messages will get no reply \
                     until you run `cd <project> && wizard --gateway` (or enable the \
                     systemd user unit; see docs/gateway.md)",
                )
            }
        }
        Ok(_) => {
            // pgrep exits 1 when nothing matches.
            Check::fail(
                label,
                "no wizard --gateway process found — messages will get no reply \
                 until you run `cd <project> && wizard --gateway` (or enable the \
                 systemd user unit; see docs/gateway.md)",
            )
        }
        Err(_) => Check::skip(
            label,
            "pgrep not available; cannot check for a running gateway",
        ),
    }
}

// ---------------------------------------------------------------------------
// network checks (probe with timeout; never exercised by unit tests)
// ---------------------------------------------------------------------------

/// One configured provider answers its health probe within
/// [`PROBE_TIMEOUT`]. Skipped only when it has no key at all: neither the
/// API key env var nor a stored credential. The two conditions mirror
/// `ProviderConfig::resolved_key`, which reads the env var first and falls
/// back to `credentials.toml`, so either source being present means the probe
/// carries a real key.
async fn check_provider(provider: &ProviderConfig) -> Check {
    let label = format!("provider {}", provider.name);
    if let Some(env) = &provider.api_key_env
        && !std::env::var(env).is_ok_and(|value| !value.trim().is_empty())
        && crate::credentials::get(&provider.name).is_none()
    {
        return Check::skip(label, format!("${env} not set and no stored key"));
    }
    let client = match provider.build() {
        Ok(client) => client,
        Err(err) => return Check::fail(label, format!("build failed: {err:#}")),
    };
    match tokio::time::timeout(PROBE_TIMEOUT, client.health()).await {
        Ok(Ok(())) => Check::pass(
            label,
            format!("{} ({}) reachable", client.label(), provider.model),
        ),
        Ok(Err(err)) => Check::fail(label, format!("{err:#}")),
        Err(_) => Check::fail(
            label,
            format!("no answer within {}s", PROBE_TIMEOUT.as_secs()),
        ),
    }
}

/// Every `[[server]]` in `mcp.toml` spawns and completes the MCP handshake
/// within the runtime's [`crate::mcp::CONNECT_TIMEOUT`], so a server that
/// works in the app never fails doctor on startup time alone.
async fn check_mcp_servers(path: &Path) -> Vec<Check> {
    let connect_timeout = crate::mcp::CONNECT_TIMEOUT;
    let config = match crate::mcp::McpConfig::load(path) {
        Ok(config) => config,
        Err(err) => return vec![Check::fail("mcp", format!("{err:#}"))],
    };
    if config.servers.is_empty() {
        return vec![Check::skip("mcp", "no MCP servers configured")];
    }
    let mut checks = Vec::new();
    for server in config.servers {
        let label = format!("mcp {}", server.name);
        let check =
            match tokio::time::timeout(connect_timeout, crate::mcp::McpConnection::connect(server))
                .await
            {
                Ok(Ok(connection)) => {
                    let detail = match tokio::time::timeout(
                        connect_timeout,
                        connection.list_tools(),
                    )
                    .await
                    {
                        Ok(Ok(tools)) => format!("handshake ok, {} tool(s)", tools.len()),
                        _ => "handshake ok".to_string(),
                    };
                    Check::pass(label, detail)
                }
                Ok(Err(err)) => Check::fail(label, format!("{err:#}")),
                Err(_) => Check::fail(
                    label,
                    format!("no handshake within {}s", connect_timeout.as_secs()),
                ),
            };
        checks.push(check);
    }
    checks
}

// ---------------------------------------------------------------------------
// assembly
// ---------------------------------------------------------------------------

/// Run the full battery for `project_root`.
pub async fn run_checks(project_root: &Path) -> Vec<Check> {
    let mut checks = Vec::new();

    // Config first: later checks reuse it when it loads.
    let config_path = Config::path().unwrap_or_else(|_| PathBuf::from("~/.wizard/config.toml"));
    checks.push(check_config_file(&config_path));

    match Config::load() {
        Ok(config) => {
            checks.push(check_active_provider(&config));
            // The synthesized local default counts when nothing is
            // configured explicitly.
            let providers = if config.providers.is_empty() {
                vec![config.active()]
            } else {
                config.providers.clone()
            };
            for provider in &providers {
                checks.push(check_provider(provider).await);
            }
            checks.extend(check_gateway(&config));
            checks.push(check_system_prompt(config.mode));
        }
        Err(err) => checks.push(Check::fail(
            "providers",
            format!("config unusable: {err:#}"),
        )),
    }

    if let Ok(path) = crate::credentials::path() {
        checks.push(check_credentials_file(&path));
    }

    if let Ok(dir) = Config::wizard_dir() {
        checks.push(check_secret_storage(&dir));
    }

    if let Ok(path) = Config::mcp_config_path() {
        checks.extend(check_mcp_servers(&path).await);
    }

    checks.push(check_native_tools());
    checks.push(check_platform());
    checks.push(check_color_depth());

    if let Ok(dir) = Config::wizard_dir() {
        checks.push(check_hooks_file("hooks (global)", &dir.join("hooks.toml")));
        checks.push(check_writable("~/.wizard", &dir));
    }
    checks.push(check_hooks_file(
        "hooks (project)",
        &project_root.join(".wizard").join("hooks.toml"),
    ));
    checks.push(check_writable(
        "project .wizard",
        &project_root.join(".wizard"),
    ));
    if let Ok(dir) = Config::sessions_dir() {
        checks.push(check_writable("sessions", &dir));
    }
    checks.push(check_checkpoints(project_root));

    checks
}

/// `wizard doctor`: print the report, exit 0 when nothing failed. A spinner
/// covers the network probes (capped at [`PROBE_TIMEOUT`] each) while they
/// run, then clears before the report so the rendered output is unchanged;
/// it is silent when stderr is not a terminal. The TUI `/doctor` calls
/// [`run_checks`] directly: it owns the screen and draws no spinner here.
///
/// `bundle` is the parsed `wizard doctor --bundle` flag; see
/// [`bundle_requested`] for the environment equivalent.
pub async fn run(bundle: bool) -> Result<i32> {
    if bundle_requested(bundle) {
        return run_bundle().await;
    }
    let project_root = std::env::current_dir()?;
    let spinner = crate::progress::Spinner::start("running checks…");
    let checks = run_checks(&project_root).await;
    spinner.finish();
    println!("{}", render(&checks));
    Ok(if has_failures(&checks) { 1 } else { 0 })
}

/// True when this run should produce a bundle instead of a plain report.
///
/// `flag` is the parsed `cli::Command::Doctor { bundle }` field, which is the
/// documented route. `WIZARD_DOCTOR_BUNDLE=1` is the equivalent for callers
/// that never reach clap: a wrapper script, a systemd unit, or a support
/// instruction handed to someone whose binary predates the flag. It is
/// deliberately not an argv scan any more: `--bundle` can legitimately appear
/// inside `-p "add a --bundle flag"`, and scanning would have turned that run
/// into a bundle write.
pub fn bundle_requested(flag: bool) -> bool {
    flag || std::env::var_os("WIZARD_DOCTOR_BUNDLE")
        .is_some_and(|value| !value.is_empty() && value != "0")
}

// ---------------------------------------------------------------------------
// bug-report bundle (`wizard doctor --bundle`)
// ---------------------------------------------------------------------------

/// What every stripped value is replaced with.
pub const REDACTED: &str = "<redacted>";

/// Shortest literal a known secret must be before it is substituted out of the
/// text members. A one-character stored "key" would otherwise rewrite half the
/// transcript.
const MIN_SECRET_LEN: usize = 8;

/// Shortest a word must be before a vendor prefix (`sk-`, `ghp_`, ...) counts
/// as evidence of a credential. Real keys run 30+ characters, so the floor
/// keeps prose and variable names readable.
const MIN_PREFIXED_SECRET_LEN: usize = 16;

/// Per-member size cap. Transcripts and logs grow without bound, the tail is
/// the part that explains a crash, and a bug report has to stay attachable.
const MEMBER_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// How many files from `~/.wizard/logs/` the bundle carries, newest first.
const MAX_LOG_FILES: usize = 5;

/// A written bundle: where it landed and what went into it.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// Directory holding the members (0700 on unix: it contains a transcript).
    pub dir: PathBuf,
    /// Member paths relative to [`Bundle::dir`], in write order.
    pub members: Vec<String>,
    /// Inputs that were not there, recorded so a reader can tell "no logs" from
    /// "logs withheld".
    pub omitted: Vec<String>,
    /// Members whose head was dropped to fit [`MEMBER_MAX_BYTES`].
    pub truncated: Vec<String>,
}

impl Bundle {
    /// Write one member and record it.
    fn add(&mut self, rel: &str, contents: &str) -> Result<()> {
        let path = self.dir.join(rel);
        if let Some(parent) = path.parent() {
            // The same policy the bundle root got, for the same reason: a
            // plain `create_dir_all` runs at the process umask, so `logs/`
            // (request traces, headers included) would sit at 0755 inside a
            // 0700 root and the members under it would be readable by anyone
            // who could guess the path.
            crate::platform::secrets::create_private_dir_strict(parent)?;
        }
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
        self.members.push(rel.to_string());
        Ok(())
    }
}

/// Field names that may pass through into the bundled `config.toml`.
///
/// This is an allowlist and it has to stay one. A denylist of fields to strip
/// fails open: the release that adds `[provider] inline_key` leaks it until
/// somebody notices and patches the list, and nothing in the test suite would
/// have caught it. Anything not named here becomes [`REDACTED`], so a new
/// field costs one line here and leaks nothing in the meantime.
///
/// Names are matched at any depth rather than by path. TOML keys inside
/// `config.toml` are descriptive enough that a path-aware list would only add
/// noise, and matching by name is the conservative direction: a key allowed
/// under `[web]` is allowed everywhere, never the reverse.
const CONFIG_ALLOWLIST: &[&str] = &[
    // Config (top level).
    "model",
    "ollama_host",
    "llamacpp_host",
    "gguf_path",
    "mode",
    "reasoning_effort",
    "max_steps",
    "continuous",
    "plan_first",
    "omakase",
    "plan_each_cycle",
    "rollback_failed_cycles",
    "retry_base_secs",
    "retry_max_secs",
    "cycle_pause_secs",
    "compact_threshold_bytes",
    "providers",
    "active_provider",
    "gateway",
    "ui",
    "web",
    "checkpoints",
    "fleet",
    "update",
    "sync",
    "fusion",
    "ultra",
    // ProviderConfig. `api_key_env` names an environment variable, it never
    // holds the key itself, and knowing which variable a broken provider reads
    // is most of a provider bug report. Being on this list only allows the
    // *field*: the value still has to be confirmed as a variable name before
    // it is printed, and is redacted when it cannot be (see
    // [`is_confirmed_env_var_name`] and [`redact_toml_value`]).
    "name",
    "kind",
    "base_url",
    "api_key_env",
    "usd_per_mtok_in",
    "usd_per_mtok_out",
    // GatewayConfig.
    "token_env",
    "allowed_chat_ids",
    // UiConfig.
    "spinner_verbs",
    "vim",
    // WebConfig.
    "fetch_max_bytes",
    "allow_local",
    "search_backend",
    "search_api_key_env",
    // CheckpointConfig / FleetConfig.
    "keep_turns",
    "max_minutes",
    "synthesize",
    // UpdateConfig.
    "notify",
    "auto",
    "repo",
    "interval_hours",
    // SyncConfig. `source` is a URL, and a URL can carry credentials in its
    // userinfo, so [`scrub_text`] runs over the allowlisted output too.
    "source",
    // FusionConfig / UltraConfig.
    "panel",
    "synthesizer",
    "rounds",
    "lenses",
    "judges",
    "candidate_max_steps",
    "judge_max_steps",
    "timeout_secs",
    "max_draft_chars",
];

/// Key names whose *value* is a credential wherever it appears in free text
/// (JSON logs, TOML, a transcript, an HTTP header dump).
///
/// Unlike [`CONFIG_ALLOWLIST`] this is a denylist, because free text has no
/// schema to allowlist against: the only alternative would be dropping the
/// transcript entirely, which is the one member a bug report cannot do
/// without. It is the third layer, after literal known secrets and vendor key
/// shapes, not the first.
const SECRET_KEY_NAMES: &[&str] = &[
    "access_token",
    "accesstoken",
    "api-key",
    "api_key",
    "apikey",
    "auth",
    "authorization",
    "bot_token",
    "client_secret",
    "credential",
    "credentials",
    "id_token",
    "passwd",
    "password",
    "private_key",
    "refresh_token",
    "refreshtoken",
    "secret",
    "secret_key",
    "session_key",
    "token",
    "x-api-key",
];

/// Stems that mean "credential" only as a vendor-prefixed tail (`TAVILY_KEY`,
/// `xai-key`), never on their own.
///
/// `key` is the whole list and the reason it exists. It is the second most
/// common spelling after `_API_KEY` and neither `api_key` nor `secret_key` is a
/// tail of `tavily_key`, so it was missed entirely; but promoting it to
/// [`SECRET_KEY_NAMES`] would redact the value of every bare `key =` in the
/// bundle, and a JSON map dump or a `[keys]` section header is prose a triager
/// needs. Tail-only splits the difference: `openai_key` goes, `key` stays.
const SECRET_KEY_TAILS: &[&str] = &["key"];

/// Vendor key prefixes. Matched case-insensitively against words of at least
/// [`MIN_PREFIXED_SECRET_LEN`] characters.
const SECRET_PREFIXES: &[&str] = &[
    "akia",
    "aiza",
    "csk-",
    "dckr_pat_",
    "ghp_",
    "gho_",
    "ghr_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "glpat-",
    "gsk_",
    "hf_",
    "npm_",
    "pplx-",
    "r8_",
    "shpat_",
    "sk-",
    "sk_",
    "xai-",
    "xapp-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
];

/// `wizard doctor --bundle`: run the checks, then write a redacted bug-report
/// bundle and print where it landed. Exits like [`run`] (1 when a check
/// failed) so scripting either mode behaves the same.
pub async fn run_bundle() -> Result<i32> {
    let project_root = std::env::current_dir()?;
    let spinner = crate::progress::Spinner::start("running checks…");
    let checks = run_checks(&project_root).await;
    spinner.finish();
    let report = render(&checks);
    println!("{report}");

    let wizard_dir = Config::wizard_dir()?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let dest = wizard_dir.join("bundles").join(format!("doctor-{stamp}"));
    let bundle = write_bundle(&wizard_dir, &dest, &report)?;

    println!();
    println!("bundle: {}", bundle.dir.display());
    println!(
        "  {} member(s): {}",
        bundle.members.len(),
        bundle.members.join(", ")
    );
    if !bundle.omitted.is_empty() {
        println!("  absent: {}", bundle.omitted.join(", "));
    }
    if !bundle.truncated.is_empty() {
        println!(
            "  truncated to the last {} MiB: {}",
            MEMBER_MAX_BYTES / (1024 * 1024),
            bundle.truncated.join(", ")
        );
    }
    println!(
        "Secrets are stripped by an allowlist, but the transcript is your own \
         text: read the bundle before you send it anywhere."
    );
    Ok(if has_failures(&checks) { 1 } else { 0 })
}

/// Assemble a bundle from the state under `wizard_dir` into `dest`.
///
/// Every input is optional: a fresh install has no sessions, a release binary
/// has no `logs/`, and neither is an error. `report` is the rendered check
/// output, carried as `doctor.txt` so the bundle stands alone.
pub fn write_bundle(wizard_dir: &Path, dest: &Path, report: &str) -> Result<Bundle> {
    write_bundle_with(wizard_dir, dest, report, |name| std::env::var(name).ok())
}

/// Testable core of [`write_bundle`]: `lookup` supplies the value of an
/// environment variable, or `None` when unset.
///
/// Split out for the same reason [`known_secrets_from`] is, and now for a
/// second one: whether a `*_env` value is printed or withheld depends on the
/// environment (see [`is_confirmed_env_var_name`]), so a test that asserts on
/// the bundled `config.toml` has to be able to say what the environment holds.
pub fn write_bundle_with(
    wizard_dir: &Path,
    dest: &Path,
    report: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Bundle> {
    let lookup: EnvLookup<'_> = &lookup;
    // The bundle holds a transcript and whatever the logs picked up, so it is
    // no more shareable by default than the state it was built from. Strict:
    // a directory we cannot make private is worse than no bundle at all, since
    // the whole point of the copy is that it is about to be handed around.
    crate::platform::secrets::create_private_dir_strict(dest)?;

    let secrets = known_secrets_from(wizard_dir, lookup);
    let mut bundle = Bundle {
        dir: dest.to_path_buf(),
        members: Vec::new(),
        omitted: Vec::new(),
        truncated: Vec::new(),
    };

    // config.toml: structural allowlist first, then the same text scrub every
    // other member gets, so a key pasted into an allowlisted field (say a
    // literal token in `api_key_env`) still does not survive.
    let config_path = wizard_dir.join("config.toml");
    match std::fs::read_to_string(&config_path) {
        Ok(raw) => {
            let redacted = redact_config_toml_with(&raw, lookup);
            // A config that reads but does not parse cannot be walked field by
            // field, so the member carries the parse error instead of the
            // contents. The manifest has to say so: otherwise `members` lists
            // config.toml, `omitted` does not, and a triager reads the pair as
            // "the config was fine" when nothing of it was ever inspected.
            if !redacted.walked {
                bundle
                    .omitted
                    .push("config.toml contents (does not parse)".to_string());
            }
            // Same reasoning one field down: a `*_env` value that could not be
            // confirmed to name a variable is withheld, and the reader has to
            // be able to tell that from a field this build does not know.
            for field in &redacted.withheld_env_fields {
                bundle.omitted.push(format!(
                    "config.toml: {field} value withheld (not a confirmed \
                     environment variable name)"
                ));
            }
            let body = scrub_member(&redacted.text, "config.toml", &secrets, &mut bundle);
            bundle.add("config.toml", &body)?;
        }
        Err(_) => bundle.omitted.push("config.toml".to_string()),
    }

    // The most recent session transcript: the reproduction, in the user's own
    // words. Only the newest one, and only its tail.
    match latest_file(&wizard_dir.join("sessions"), &["jsonl"]) {
        Some(path) => copy_scrubbed(&path, "session.jsonl", &secrets, &mut bundle)?,
        None => bundle.omitted.push("session.jsonl".to_string()),
    }

    for (name, path) in [
        ("usage.jsonl", wizard_dir.join("usage.jsonl")),
        ("evolution.jsonl", wizard_dir.join("evolution.jsonl")),
    ] {
        if path.is_file() {
            copy_scrubbed(&path, name, &secrets, &mut bundle)?;
        } else {
            bundle.omitted.push(name.to_string());
        }
    }

    // `~/.wizard/logs/<session>.jsonl` plus the scheduler's `.log` files. The
    // directory does not exist on every install, which is a skip, not a
    // failure.
    let logs = recent_files(&wizard_dir.join("logs"), &["jsonl", "log"], MAX_LOG_FILES);
    if logs.is_empty() {
        bundle.omitted.push("logs/".to_string());
    }
    for path in logs {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let member = format!("logs/{name}");
        copy_scrubbed(&path, &member, &secrets, &mut bundle)?;
    }

    let report = scrub_member(report, "doctor.txt", &secrets, &mut bundle);
    bundle.add("doctor.txt", &format!("{report}\n"))?;

    // The two trailing members describe the finished bundle, so the member
    // list they carry has to name them as well as everything written above.
    let mut members = bundle.members.clone();
    members.push("manifest.json".to_string());
    members.push("README.txt".to_string());

    let manifest = serde_json::json!({
        "wizard_version": crate::update::current_version(),
        "wizard_commit": build_commit(wizard_dir),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "members": members,
        "omitted": bundle.omitted,
        "truncated": bundle.truncated,
    });
    let manifest = serde_json::to_string_pretty(&manifest).context("serializing the manifest")?;
    bundle.add("manifest.json", &format!("{manifest}\n"))?;
    bundle.add("README.txt", &readme(&members))?;
    Ok(bundle)
}

/// Copy `src` into the bundle as `member`, scrubbed and tail-limited.
fn copy_scrubbed(src: &Path, member: &str, secrets: &[String], bundle: &mut Bundle) -> Result<()> {
    let (text, truncated) = match read_tail(src, MEMBER_MAX_BYTES) {
        Ok(read) => read,
        // An unreadable input is worth reporting, not worth failing the whole
        // bundle over: the rest of it still describes the bug.
        Err(err) => {
            bundle.omitted.push(format!("{member} ({err})"));
            return Ok(());
        }
    };
    let body = scrub_member(&text, member, secrets, bundle);
    bundle.add(member, &body)?;
    if truncated {
        bundle.truncated.push(member.to_string());
    }
    Ok(())
}

/// Scrub one member's text, recording anything the redactor withheld rather
/// than replaced.
///
/// The PEM guard is the only layer that can drop text it did not judge word by
/// word (see [`redact_pem_blocks`]), and a member that is quietly shorter than
/// its source is the one failure mode a triager cannot see: the transcript
/// simply ends. So it lands in `omitted` beside the inputs that were never
/// there, which is the field a reader already consults to tell "no logs" from
/// "logs withheld".
fn scrub_member(text: &str, member: &str, secrets: &[String], bundle: &mut Bundle) -> String {
    let (scrubbed, withheld) = scrub_text_parts(text, secrets);
    if withheld {
        bundle.omitted.push(format!(
            "{member}: text after an unterminated PEM header was withheld"
        ));
    }
    scrubbed
}

/// The note that ships with the bundle. Nothing here is machine-read; it
/// exists so the person about to attach this to an issue knows what is in it.
fn readme(members: &[String]) -> String {
    let mut out = String::new();
    out.push_str("wizard doctor bundle\n====================\n\n");
    out.push_str(
        "Generated by `wizard doctor --bundle` to accompany a bug report.\n\
         Members:\n",
    );
    for member in members {
        out.push_str(&format!("  {member}\n"));
    }
    out.push_str(
        "\nRedaction\n---------\n\
         config.toml passed through a field allowlist: any field not known to \n\
         be safe was replaced with \"<redacted>\", including fields added after \n\
         this build. A *_env field names an environment variable instead of \n\
         holding a key, so its value is printed only when it can be confirmed \n\
         to be a variable name; an unconfirmed one is redacted like everything \n\
         else and manifest.json records which field it was. Every member was \n\
         then scanned for stored API keys, OAuth \n\
         access and refresh tokens, the Telegram bot token, literal \n\
         credentials in mcp.toml, bearer credentials, PEM private key blocks, \n\
         credential-ish key names (anything ending in _api_key, _token, \n\
         _secret, ...), and vendor key shapes; matches were replaced with \n\
         \"<redacted>\". credentials.toml and the OAuth token files are never \n\
         copied in.\n\n\
         Review before sending\n---------------------\n\
         The session transcript and the logs are your own text and your own \n\
         file paths. No redactor can know which of that is sensitive, so read \n\
         the bundle before you attach it to anything.\n",
    );
    out
}

/// The commit this binary was built from, best effort. A release binary
/// carries none, so deep evolve's checkout at `~/.wizard/src` is the fallback
/// and "unknown" is the honest answer when there is neither.
///
/// `WIZARD_COMMIT` has to be set by whatever builds the binary; nothing in this
/// repo sets it yet, so today the checkout fallback is the only source that
/// ever fires and every release binary reports "unknown".
fn build_commit(wizard_dir: &Path) -> String {
    build_commit_from(option_env!("WIZARD_COMMIT"), wizard_dir)
}

/// Testable core of [`build_commit`]: `baked` is the commit compiled into the
/// binary, or `None`.
///
/// Split out because `option_env!` is fixed at compile time, so a test could
/// only ever exercise whichever arm this build happened to produce, and the
/// arm that is dead today is the one a release build will take tomorrow.
fn build_commit_from(baked: Option<&str>, wizard_dir: &Path) -> String {
    if let Some(commit) = baked
        && !commit.trim().is_empty()
    {
        return commit.trim().to_string();
    }
    let source = wizard_dir.join("src");
    if !source.join(".git").exists() {
        return "unknown".to_string();
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// Newest file directly under `dir` with one of `extensions`, by modified
/// time and then by name (session ids are zero-padded timestamps, so the name
/// is a sound tiebreak when mtimes collide).
fn latest_file(dir: &Path, extensions: &[&str]) -> Option<PathBuf> {
    recent_files(dir, extensions, 1).into_iter().next()
}

/// Up to `limit` files directly under `dir` with one of `extensions`, newest
/// first. A missing or unreadable directory yields none.
fn recent_files(dir: &Path, extensions: &[&str], limit: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| extensions.contains(&ext))
        })
        .map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (modified, path)
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    files.truncate(limit);
    files.into_iter().map(|(_, path)| path).collect()
}

/// Read at most `max_bytes` from the end of `path`, returning the text and
/// whether anything was dropped. The seek can land mid-line and mid-codepoint,
/// so the decode is lossy and the first partial line is discarded, keeping the
/// JSONL members parseable.
fn read_tail(path: &Path, max_bytes: u64) -> std::io::Result<(String, bool)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let truncated = len > max_bytes;
    if truncated {
        file.seek(SeekFrom::Start(len - max_bytes))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if !truncated {
        return Ok((text, false));
    }
    let rest = match text.split_once('\n') {
        Some((_, rest)) => rest.to_string(),
        // No newline anywhere in the tail: the file is one enormous record (a
        // base64 attachment, a minified page). "Drop the partial first line"
        // would drop the whole member and leave a zero-byte file that reads as
        // "there were no logs", so keep it: a record cut at the head is worth
        // more to a triager than nothing at all.
        None => text,
    };
    Ok((rest, true))
}

/// How the bundle reads an environment variable.
///
/// Injected rather than called directly so the `*_env` paths can be asserted
/// without mutating the process environment from a test thread, and passed as
/// a trait object because the config walker, the redactor and the field gate
/// all share the one closure.
type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Every literal secret this machine can hand us, longest first.
///
/// Covers what the shape rules cannot: an opaque provider key with no vendor
/// prefix is unrecognizable in a log line, but it is sitting in
/// `credentials.toml` where we can read it and substitute it out.
///
/// The source list is every file under `~/.wizard` that is documented as
/// holding a literal credential: `credentials.toml`, the two OAuth token
/// files, the `*_env` variables named by `config.toml`, and the `[server.env]`
/// / `[server.headers]` maps of `mcp.toml`. `credentials.toml` and the token
/// files are walked generically (every string leaf), so a credential added to
/// one of them is covered the day it lands rather than the day someone updates
/// this function; the two mcp maps are free-form and mostly hold ordinary
/// settings, so only their credential-named leaves are taken (see
/// [`collect_mcp_secrets`] for what that costs and why). `schedule.toml` and
/// `hooks.toml` are deliberately not walked: their strings are prompts and
/// shell commands, which are the bug report, and a token embedded in one is
/// caught by [`scrub_text`]'s shape and key-name layers instead.
pub fn known_secrets(wizard_dir: &Path) -> Vec<String> {
    known_secrets_from(wizard_dir, |name| std::env::var(name).ok())
}

/// Testable core of [`known_secrets`]: `lookup` supplies the value of an
/// environment variable, or `None` when unset. Mirrors
/// `ProviderConfig::resolved_key_from`, so the `*_env` path can be asserted
/// without mutating the process environment from a test thread.
pub fn known_secrets_from(
    wizard_dir: &Path,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let lookup: EnvLookup<'_> = &lookup;
    let mut secrets: Vec<String> = Vec::new();

    if let Ok(raw) = std::fs::read_to_string(wizard_dir.join("credentials.toml"))
        && let Ok(value) = raw.parse::<toml::Value>()
    {
        collect_toml_strings(&value, &mut secrets);
    }

    for name in ["xai_oauth.json", "chatgpt_oauth.json"] {
        if let Ok(raw) = std::fs::read_to_string(wizard_dir.join(name))
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            collect_json_strings(&value, &mut secrets);
        }
    }

    // config.toml only ever names the variable holding a key, so the values
    // come from the environment we are running in. A `*_env` value we cannot
    // confirm to be a variable name is the exception: it may well be a key
    // pasted into the wrong field, so it is treated as a literal to substitute
    // out of every member as well as being looked up (see
    // [`collect_env_var_names`], and [`redact_toml_value`], which withholds it
    // from the config itself).
    let mut env_names = vec![crate::config::GatewayConfig::DEFAULT_TOKEN_ENV.to_string()];
    if let Ok(raw) = std::fs::read_to_string(wizard_dir.join("config.toml"))
        && let Ok(value) = raw.parse::<toml::Value>()
    {
        collect_env_var_names(&value, &mut env_names, &mut secrets, lookup);
    }

    // mcp.toml is not walked generically: most of its strings are commands,
    // args, and endpoint URLs, and substituting those out would gut the bug
    // report. The credential-bearing leaves are the two free-form maps whose
    // own documentation shows literal tokens in them (`[server.env]` and
    // `[server.headers]`, see [`crate::mcp::McpConfig`]); a `env:VAR` header
    // is an indirection, so its variable joins the lookup list instead.
    if let Ok(raw) = std::fs::read_to_string(wizard_dir.join("mcp.toml"))
        && let Ok(value) = raw.parse::<toml::Value>()
    {
        collect_mcp_secrets(&value, &mut secrets, &mut env_names);
    }

    for name in env_names {
        if let Some(value) = lookup(&name) {
            secrets.push(value);
        }
    }

    secrets.retain(|secret| secret.trim().chars().count() >= MIN_SECRET_LEN);
    secrets.sort();
    secrets.dedup();
    // Longest first: when one secret contains another (a JWT and its header
    // segment), replacing the long one first avoids leaving a fragment behind.
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets
}

/// Every string leaf of a TOML document.
fn collect_toml_strings(value: &toml::Value, out: &mut Vec<String>) {
    match value {
        toml::Value::String(text) => out.push(text.clone()),
        toml::Value::Table(table) => {
            for value in table.values() {
                collect_toml_strings(value, out);
            }
        }
        toml::Value::Array(items) => {
            for value in items {
                collect_toml_strings(value, out);
            }
        }
        _ => {}
    }
}

/// Every string leaf of a JSON document.
fn collect_json_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => out.push(text.clone()),
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_json_strings(value, out);
            }
        }
        serde_json::Value::Array(items) => {
            for value in items {
                collect_json_strings(value, out);
            }
        }
        _ => {}
    }
}

/// The value of every `*_env` key in a TOML document: by convention those name
/// an environment variable holding a secret (`api_key_env`, `token_env`,
/// `search_api_key_env`).
///
/// Every value is queued for lookup whether or not it is confirmed to be a
/// name, because looking one up costs a `getenv` that returns nothing when the
/// value was really a pasted key, while *not* looking up a name we failed to
/// recognize is how the real key stopped being substituted out of the
/// transcript. A value that is not confirmed additionally goes to `literals`:
/// the only other thing it can be is the credential itself, and that one has
/// to leave every member.
fn collect_env_var_names(
    value: &toml::Value,
    names: &mut Vec<String>,
    literals: &mut Vec<String>,
    lookup: EnvLookup<'_>,
) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                if key.ends_with("_env")
                    && let toml::Value::String(name) = value
                {
                    names.push(name.clone());
                    if !is_confirmed_env_var_name(name, lookup) {
                        literals.push(name.clone());
                    }
                }
                collect_env_var_names(value, names, literals, lookup);
            }
        }
        toml::Value::Array(items) => {
            for value in items {
                collect_env_var_names(value, names, literals, lookup);
            }
        }
        _ => {}
    }
}

/// True when `text` is *confirmed* to be the name of an environment variable
/// rather than a credential pasted into a `*_env` field.
///
/// Positive evidence only, and the direction is the whole point. The rule this
/// replaced asked whether a value "looked like a name rather than a secret",
/// which is a denylist of secret-looking shapes wearing a different hat: it
/// passed everything it did not recognize, so a 64-character all-lower-case
/// hex key (Together, DeepInfra and Voyage all issue them) read as a plain
/// identifier and shipped verbatim in a file the user is told to attach to a
/// public issue.
///
/// A name must be an identifier (`[A-Za-z_][A-Za-z0-9_]*`, what POSIX allows),
/// and then one of two independent proofs has to hold:
///
/// - This machine's environment defines a variable with that name. A key
///   pasted into `config.toml` is not a variable on the box it was pasted on.
/// - The name is spelled like the credential it points at: its tail is one of
///   the credential stems [`is_secret_name`] already knows (`..._API_KEY`,
///   `..._TOKEN`, `..._key`, `..._SECRET`). Every `*_env` default Wizard ships
///   satisfies this (`OPENAI_API_KEY`, `XAI_API_KEY`, `CLOUDFLARE_API_TOKEN`,
///   `WIZARD_TELEGRAM_TOKEN`), as does every convention a provider documents,
///   while an opaque key does not end in `_key` after a separator.
///
/// Anything else is unconfirmed, which is not a claim that it *is* a secret:
/// it is "we cannot tell", and a field that can hold either a name or a key is
/// one where not being able to tell has to mean redact. The cost of being
/// wrong that way is one variable name missing from the bundled config, and
/// the bundle records that it was withheld (see [`redact_config_toml_with`]);
/// the cost of being wrong the other way is the key itself.
fn is_confirmed_env_var_name(text: &str, lookup: EnvLookup<'_>) -> bool {
    let identifier = !text.is_empty()
        && !text.starts_with(|ch: char| ch.is_ascii_digit())
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    identifier && (is_secret_name(&text.to_ascii_lowercase()) || lookup(text).is_some())
}

/// Literal credentials in `~/.wizard/mcp.toml`: the credential-named entries of
/// a `[server.env]` or `[server.headers]` map, at any depth.
///
/// The *key* decides, through the same [`is_secret_name`] denylist the text
/// scrub uses, and that filter is not optional. Both maps are free-form and
/// mostly hold ordinary settings: a filesystem server's
/// `ALLOWED_DIRECTORIES = "/home/you/projects"` and a node server's
/// `NODE_ENV = "production"` are not secrets, and treating them as literals
/// meant substituting them by raw substring through every member, so
/// "the reproduction steps" came out as "the re<redacted> steps" and every
/// project path vanished from the transcript the bug report is made of.
///
/// The cost is real and worth stating: a genuine secret filed under a name
/// that says nothing (`SENTRY_DSN`, `PROJECT_TOKEN_V2`) is no longer collected,
/// so it survives unless [`scrub_text`]'s shape layer recognizes it. That is
/// the deliberate side to fail on here, because mcp.toml is not itself a
/// bundle member: the exposure is only a transcript that quotes it, while the
/// other direction rewrites every member by raw substring. Widening this back
/// out means giving layer 1 a value rule that can tell a token from a path,
/// not removing the key filter.
///
/// `env:VAR` values are the documented indirection, so the variable is added to
/// `env_names` and resolved from the environment instead. An
/// `Authorization = "Bearer <token>"` header also contributes the token on its
/// own, because that is how it appears in a request log.
fn collect_mcp_secrets(value: &toml::Value, out: &mut Vec<String>, env_names: &mut Vec<String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                if matches!(key.as_str(), "env" | "headers")
                    && let toml::Value::Table(map) = value
                {
                    for (name, value) in map {
                        if !is_secret_name(key_hint(&name.to_ascii_lowercase())) {
                            continue;
                        }
                        let Some(text) = value.as_str() else { continue };
                        if let Some(name) = text.strip_prefix("env:") {
                            env_names.push(name.to_string());
                            continue;
                        }
                        out.push(text.to_string());
                        if let Some((scheme, token)) = text.split_once(' ')
                            && matches!(
                                scheme.to_ascii_lowercase().as_str(),
                                "bearer" | "basic" | "token"
                            )
                        {
                            out.push(token.to_string());
                        }
                    }
                }
                collect_mcp_secrets(value, out, env_names);
            }
        }
        toml::Value::Array(items) => {
            for value in items {
                collect_mcp_secrets(value, out, env_names);
            }
        }
        _ => {}
    }
}

/// Rewrite `config.toml` keeping only [`CONFIG_ALLOWLIST`] fields.
///
/// An unparseable config is not copied at all: doctor already reports the
/// parse error, and a document we cannot walk is a document whose secrets we
/// cannot find.
pub fn redact_config_toml(raw: &str) -> String {
    redact_config_toml_with(raw, &|name| std::env::var(name).ok()).text
}

/// What walking `config.toml` for the bundle produced.
struct RedactedConfig {
    /// The member's text: the allowlisted document, or a comment saying why
    /// there is none.
    text: String,
    /// False when the document could not be walked field by field, so nothing
    /// in `text` came from the user's config.
    walked: bool,
    /// Names of `*_env` fields whose value could not be confirmed to be an
    /// environment variable name, so the value was withheld rather than
    /// printed. Recorded because `<redacted>` sitting in a field that normally
    /// carries a variable name reads as a bug in the redactor otherwise, and
    /// because it is the one hint the user has that they pasted a key there.
    withheld_env_fields: Vec<String>,
}

/// [`redact_config_toml`] with the environment injected, plus the two facts
/// [`write_bundle`] has to record in the manifest: whether the document was
/// walked at all, and which `*_env` values were withheld.
fn redact_config_toml_with(raw: &str, lookup: EnvLookup<'_>) -> RedactedConfig {
    let value = match raw.parse::<toml::Value>() {
        Ok(value) => value,
        Err(err) => {
            return RedactedConfig {
                text: format!(
                    "# config.toml does not parse ({err}), so it was left out of the\n\
                     # bundle: an unwalkable document cannot be redacted field by field.\n"
                ),
                walked: false,
                withheld_env_fields: Vec::new(),
            };
        }
    };
    let mut withheld_env_fields = Vec::new();
    let redacted = redact_toml_value(&value, lookup, &mut withheld_env_fields);
    withheld_env_fields.sort();
    withheld_env_fields.dedup();
    match toml::to_string_pretty(&redacted) {
        Ok(text) => RedactedConfig {
            text,
            walked: true,
            withheld_env_fields,
        },
        Err(err) => RedactedConfig {
            text: format!("# redacted config.toml could not be re-serialized ({err})\n"),
            walked: false,
            withheld_env_fields: Vec::new(),
        },
    }
}

/// Recursive half of [`redact_config_toml`]: a table key survives only when it
/// is allowlisted, and its value is then walked with the same rule, so a
/// stray field nested under an allowlisted table is caught too.
///
/// `withheld` collects the `*_env` fields whose value was replaced because it
/// could not be confirmed to name a variable.
fn redact_toml_value(
    value: &toml::Value,
    lookup: EnvLookup<'_>,
    withheld: &mut Vec<String>,
) -> toml::Value {
    match value {
        toml::Value::Table(table) => toml::Value::Table(
            table
                .iter()
                .map(|(key, value)| {
                    let value = if !CONFIG_ALLOWLIST.contains(&key.as_str()) {
                        toml::Value::String(REDACTED.to_string())
                    } else if key.ends_with("_env")
                        && value
                            .as_str()
                            .is_some_and(|text| !is_confirmed_env_var_name(text, lookup))
                    {
                        // `api_key_env` and friends are allowlisted because
                        // they name a variable, never because their value is
                        // safe. Printing one unconfirmed is the fail-open
                        // direction and no other layer covers it: an opaque
                        // key pasted here has no vendor prefix to recognize
                        // and is not on disk to substitute.
                        withheld.push(key.clone());
                        toml::Value::String(REDACTED.to_string())
                    } else {
                        redact_toml_value(value, lookup, withheld)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        toml::Value::Array(items) => toml::Value::Array(
            items
                .iter()
                .map(|item| redact_toml_value(item, lookup, withheld))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Strip credentials from free text: session transcripts, logs, the rendered
/// report, and the already-allowlisted config.
///
/// Four layers, cheapest and most certain first. `secrets` are literals we
/// read off this machine and can substitute exactly. Then whole PEM blocks go,
/// because they are the one credential that spans many words. Then each word
/// is judged on its shape (vendor prefix, JWT, Telegram bot token, URL
/// userinfo). Finally a word that follows a credential-ish key name or an auth
/// scheme (`Bearer`) is dropped whatever it looks like. The last layer
/// over-redacts prose occasionally; in a file the user is about to publish,
/// that is the direction to fail in.
pub fn scrub_text(text: &str, secrets: &[String]) -> String {
    scrub_text_parts(text, secrets).0
}

/// [`scrub_text`] plus whether any text was *withheld* rather than replaced,
/// which only [`redact_pem_blocks`] can do. [`scrub_member`] turns that into a
/// manifest entry so a member shorter than its source is never silent.
fn scrub_text_parts(text: &str, secrets: &[String]) -> (String, bool) {
    let mut text = text.to_string();
    for secret in secrets {
        if secret.chars().count() >= MIN_SECRET_LEN {
            text = text.replace(secret.as_str(), REDACTED);
        }
    }
    let (text, withheld) = redact_pem_blocks(&text);

    let words = split_words(&text);
    let mut out = String::with_capacity(text.len());
    // Set by an auth scheme or by a credential-ish key name in assignment
    // position: the *next* word is the secret.
    let mut pending = false;
    for (index, (word, gap)) in words.iter().enumerate() {
        if word.is_empty() {
            out.push_str(gap);
            continue;
        }
        // Is this word being used as a key rather than read as prose? Only
        // then does a name like `token` say anything about the next word;
        // otherwise "token count is 500" would eat the count. The separator
        // can sit in the gap (`token = x`), at the end of the word
        // (`token="x"`), or at the head of the next word (`"token": "x"`,
        // where the quote closed the word before the colon).
        let next = words.get(index + 1).map(|(word, _)| word.as_str());
        let assigns = word.ends_with([':', '='])
            || gap.contains([':', '='])
            || next.is_some_and(|next| next.starts_with([':', '=']));
        // Whether this word is a URL query parameter, which `?` and `&` being
        // word separators is what tells us. It changes what a name means: a
        // bare `key = …` in a config dump is prose a triager needs, and
        // `?key=…` in a URL never is. Without this the whole query string
        // reached the bundle byte for byte.
        let in_query = index > 0
            && words[..index]
                .iter()
                .rev()
                .find_map(|(word, gap)| {
                    let tail = format!("{word}{gap}");
                    tail.rfind(['?', '&', ' ', '\t', '\n'])
                        .map(|at| tail[at..].starts_with(['?', '&']))
                })
                .unwrap_or(false);
        out.push_str(&scrub_word(word, assigns, in_query, &mut pending));
        out.push_str(gap);
    }
    (out, withheld)
}

/// Replace whole PEM blocks (`-----BEGIN X-----` ... `-----END X-----`) with a
/// single [`REDACTED`].
///
/// The word rules cannot do this. `private_key` is on the denylist, but the
/// key-name layer only claims the *next* word, and a PEM body is a hundred
/// ordinary base64 words after it: no vendor prefix, not a JWT, no assignment.
/// A pasted SSH key therefore used to ship in full. The scan is over raw text,
/// so it catches the block whether the newlines are real (a log file) or
/// escaped (`\n` inside a JSON transcript string).
///
/// The span is bounded by [`is_pem_body_char`], not by the footer alone: a
/// block that opens and never closes redacts only the run that could be key
/// material, and the second return value says so. Unbounded, a `-----BEGIN`
/// quoted in one transcript record and an `-----END` quoted in a later one
/// would collapse every record in between into one `<redacted>`, and a header
/// with no footer at all would delete the entire rest of the member: both
/// delete the bug report while looking like a successful redaction.
fn redact_pem_blocks(text: &str) -> (String, bool) {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    const DASHES: &str = "-----";

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut withheld = false;
    while let Some(start) = rest.find(BEGIN) {
        out.push_str(&rest[..start]);
        out.push_str(REDACTED);
        let body = &rest[start + BEGIN.len()..];
        // How far the block can possibly run: everything from the label to the
        // footer is base64, punctuation PEM itself uses, and line breaks, so
        // the first character that cannot appear in one ends the block whether
        // or not a footer showed up.
        let limit = body
            .find(|ch: char| !is_pem_body_char(ch))
            .unwrap_or(body.len());
        match body[..limit].find(END) {
            Some(end) => {
                // Consume through the closing dashes of the END line, so the
                // label ("OPENSSH PRIVATE KEY-----") does not survive as loose
                // words. The search for those dashes stays inside the bounded
                // run: taken from the whole of `body` instead, a footer whose
                // own dashes were cut off (terminal wrapping, a truncated
                // paste) finds no `-----` anywhere and swallows every record
                // after it, silently, which is the failure the bound exists to
                // prevent.
                let label = &body[end + END.len()..limit];
                match label.find(DASHES) {
                    Some(at) => rest = &body[end + END.len() + at + DASHES.len()..],
                    None => {
                        // The footer never closed inside the run that could be
                        // key material. Stop at the boundary, keep what the
                        // file structure says came after it, and report the
                        // withholding like the header-only case below.
                        withheld = true;
                        rest = &body[limit..];
                    }
                }
            }
            None => {
                // Header without a footer: a truncated file, or a user quoting
                // the first line of their key. Fail closed over the run that
                // could be the body, keep everything after it, and report the
                // withholding so the shortened member is not silent.
                withheld = true;
                rest = &body[limit..];
            }
        }
    }
    out.push_str(rest);
    (out, withheld)
}

/// Characters that can occur between a PEM header and its footer: the base64
/// alphabet, the label's letters and spaces, the dashes of the markers, the
/// `\n` of an escaped newline inside a JSON string, and the `:`/`,` of an
/// encrypted key's `Proc-Type:`/`DEK-Info:` headers.
///
/// Everything else, `"` and `{` above all, is structure from the file the block
/// was quoted in, and structure means the block ended here whatever the text
/// after it says.
fn is_pem_body_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || ch.is_ascii_whitespace()
        || matches!(ch, '+' | '/' | '=' | '-' | '_' | '.' | ':' | ',' | '\\')
}

/// Split `text` into `(word, following separator run)` pairs covering it
/// exactly, so joining the pairs back up reproduces the input. A leading
/// separator run arrives as an empty word.
fn split_words(text: &str) -> Vec<(String, String)> {
    let mut words: Vec<(String, String)> = Vec::new();
    let mut word = String::new();
    let mut gap = String::new();
    for ch in text.chars() {
        if is_word_boundary(ch) {
            gap.push(ch);
        } else {
            if !gap.is_empty() {
                words.push((std::mem::take(&mut word), std::mem::take(&mut gap)));
            }
            word.push(ch);
        }
    }
    if !word.is_empty() || !gap.is_empty() {
        words.push((word, gap));
    }
    words
}

/// Characters that cannot occur inside a credential, so they bound one.
/// `:`, `/`, `@`, `=`, `.`, `-` and `_` deliberately stay inside a word: they
/// are what makes a JWT, a Telegram token, or a URL with userinfo recognizable
/// as one unit.
fn is_word_boundary(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\''
                | ','
                | ';'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '\\'
                | '`'
                | '|'
                | '#'
                | '?'
                | '&'
                | '!'
                | '*'
        )
}

/// Judge one word, updating the "next word is a secret" flag.
fn scrub_word(word: &str, assigns: bool, in_query: bool, pending: &mut bool) -> String {
    let lower = word.to_ascii_lowercase();

    // Already gone: an earlier layer left [`REDACTED`] here, and the angle
    // brackets bound a word that would otherwise be redacted a second time
    // into `<<redacted>>`.
    if lower == REDACTED.trim_matches(['<', '>']) {
        *pending = false;
        return word.to_string();
    }
    // An auth scheme is not itself the secret, and keeping it makes the
    // redacted line readable ("authorization: Bearer <redacted>"). Unlike a
    // key name this needs no assignment punctuation: `Bearer` is only ever
    // written in front of the credential it introduces.
    if matches!(lower.as_str(), "bearer" | "basic") {
        *pending = true;
        return word.to_string();
    }
    if *pending && word.chars().any(|ch| ch.is_alphanumeric()) {
        *pending = false;
        return REDACTED.to_string();
    }
    // `api_key=sk-...`, `token:abc` in one word — and, inside a URL query,
    // the wider set that is only ever a credential there.
    if let Some((name, sep, value)) = split_assignment(word)
        && (is_secret_name(&name.to_ascii_lowercase())
            || (in_query && SECRET_QUERY_KEYS.contains(&name.to_ascii_lowercase().as_str())))
        && value.chars().any(|ch| ch.is_alphanumeric())
    {
        *pending = false;
        return format!("{name}{sep}{REDACTED}");
    }
    if let Some(replacement) = secret_shape(word) {
        *pending = false;
        return replacement;
    }
    if assigns && is_secret_name(key_hint(&lower)) {
        *pending = true;
        return word.to_string();
    }
    if word.chars().any(|ch| ch.is_alphanumeric()) {
        *pending = false;
    }
    word.to_string()
}

/// A word used as a key name, with the punctuation callers write around it
/// (`--token`, `api_key=`, `"secret":`) trimmed off.
fn key_hint(lower: &str) -> &str {
    lower
        .trim_start_matches(['-', '.', '$'])
        .trim_end_matches([':', '=', '.', ',', '-'])
}

/// True when `name` is a key whose value is a credential.
///
/// Exact membership in [`SECRET_KEY_NAMES`] is not enough, because almost no
/// real key is named exactly `api_key`: the dominant forms are vendor-prefixed
/// (`TAVILY_API_KEY`, `GITHUB_TOKEN`, `slack_bot_token`, `openai_api_key`), so
/// a name also counts when one of the denylisted names is its tail after a
/// `_`, `-`, `.` or `:` separator.
///
/// Tail rather than substring, for two reasons. `token_env` and `api_key_env`
/// name a variable and must keep passing through (knowing which variable a
/// broken provider reads is most of a provider bug report), and a substring
/// rule would swallow both. And a leading match would fire on `token_count`,
/// which is prose the bundle needs.
fn is_secret_name(name: &str) -> bool {
    if SECRET_KEY_NAMES.contains(&name) {
        return true;
    }
    SECRET_KEY_NAMES.iter().chain(SECRET_KEY_TAILS).any(|stem| {
        name.len() > stem.len()
            && name.ends_with(stem)
            && name[..name.len() - stem.len()].ends_with(['_', '-', '.', ':'])
    })
}

/// Split `word` at its first `=` or `:` into a key, the separator, and a
/// value. `None` when there is no separator or either side is empty.
fn split_assignment(word: &str) -> Option<(&str, char, &str)> {
    let index = word.find(['=', ':'])?;
    let sep = word[index..].chars().next()?;
    let (name, rest) = word.split_at(index);
    let value = &rest[sep.len_utf8()..];
    if name.is_empty() || value.is_empty() {
        return None;
    }
    Some((name, sep, value))
}

/// The replacement for a word that is shaped like a credential, or `None` when
/// it is not.
fn secret_shape(word: &str) -> Option<String> {
    if let Some(rewritten) = redact_url_userinfo(word) {
        return Some(rewritten);
    }
    if is_jwt(word) || is_telegram_bot_token(word) || has_secret_prefix(word) {
        return Some(REDACTED.to_string());
    }
    None
}

/// A URL whose userinfo carries a password (`https://user:token@host/x`) keeps
/// its scheme and host, which is what a bug report needs, and loses the
/// credential.
fn redact_url_userinfo(word: &str) -> Option<String> {
    let scheme_end = word.find("://")? + 3;
    let authority_end = word[scheme_end..]
        .find('/')
        .map_or(word.len(), |offset| scheme_end + offset);
    let authority = &word[scheme_end..authority_end];
    let at = authority.rfind('@')?;
    if !authority[..at].contains(':') {
        // A bare username is not a credential.
        return None;
    }
    Some(format!(
        "{}{REDACTED}@{}",
        &word[..scheme_end],
        &word[scheme_end + at + 1..]
    ))
}

/// Query parameters that are credentials wherever they appear in a URL.
///
/// Separate from [`SECRET_KEY_NAMES`] because the context is different, and
/// that difference is the whole reason this list exists. A bare `key = …` in a
/// config dump is prose a triager needs, which is why `key` is tail-only
/// there — but `?key=…` in a URL is never prose. The tail rule also silently
/// missed it: it requires `name.len() > stem.len()`, and for `key` against the
/// stem `key` that is `3 > 3`, false.
///
/// Wizard writes such a URL into its own log on its own initiative — the MCP
/// unreachable warning carries the server's configured URL — at the default
/// `wizard=warn` filter, and `wizard doctor --bundle` copies the newest logs
/// into the archive it tells the user to attach to a bug report.
const SECRET_QUERY_KEYS: &[&str] = &[
    "access_token",
    "api-key",
    "api_key",
    "apikey",
    "auth",
    "code",
    "credential",
    "id_token",
    "key",
    "password",
    "refresh_token",
    "secret",
    "session",
    "sessionid",
    "sig",
    "signature",
    "token",
    "x-amz-signature",
];

/// A vendor key prefix on a word long enough to be a real key. Also checked
/// against the value half of `key=sk-...`, which arrives as one word.
fn has_secret_prefix(word: &str) -> bool {
    let value = word.rsplit_once('=').map_or(word, |(_, value)| value);
    [word, value].into_iter().any(|candidate| {
        let lower = candidate.to_ascii_lowercase();
        candidate.chars().count() >= MIN_PREFIXED_SECRET_LEN
            && SECRET_PREFIXES
                .iter()
                .any(|prefix| lower.starts_with(prefix))
    })
}

/// Three base64url segments, the first one a JSON header (`eyJ...`): an OAuth
/// access or id token.
fn is_jwt(word: &str) -> bool {
    let segments: Vec<&str> = word.split('.').collect();
    segments.len() == 3
        && word.chars().count() >= 40
        && segments[0].starts_with("eyJ")
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '='))
        })
}

/// `<bot id>:<secret>`, the Telegram bot token shape. The id half is a short
/// run of digits, the secret half a long alphanumeric run, which no ordinary
/// `key:value` word matches.
fn is_telegram_bot_token(word: &str) -> bool {
    let Some((id, secret)) = word.split_once(':') else {
        return false;
    };
    (5..=15).contains(&id.chars().count())
        && id.chars().all(|ch| ch.is_ascii_digit())
        && secret.chars().count() >= 30
        && secret
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::secrets;

    #[test]
    fn config_check_passes_skips_and_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        let check = check_config_file(&path);
        assert_eq!(check.status, Status::Skip);

        std::fs::write(&path, "mode = \"sovereign\"\n").unwrap();
        let check = check_config_file(&path);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);

        std::fs::write(&path, "mode = [broken\n").unwrap();
        let check = check_config_file(&path);
        assert_eq!(check.status, Status::Fail);
    }

    #[test]
    fn hooks_check_passes_skips_and_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hooks.toml");

        assert_eq!(check_hooks_file("hooks", &path).status, Status::Skip);

        std::fs::write(
            &path,
            "[[hooks]]\nevent = \"pre_tool_use\"\ncommand = \"true\"\n",
        )
        .unwrap();
        let check = check_hooks_file("hooks", &path);
        assert_eq!(check.status, Status::Pass);
        assert!(check.detail.contains("1 hook(s)"), "{}", check.detail);

        std::fs::write(&path, "[[hooks]]\nevent = \"no_such_event\"\n").unwrap();
        assert_eq!(check_hooks_file("hooks", &path).status, Status::Fail);
    }

    #[test]
    fn writable_check_creates_and_probes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fresh").join("nested");
        let check = check_writable("dir", &dir);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(dir.is_dir(), "directory was created");
        // The probe file is cleaned up.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn checkpoints_check_reports_records_and_stale_dirs() {
        let tmp = tempfile::tempdir().unwrap();

        // No index yet: skip.
        assert_eq!(check_checkpoints(tmp.path()).status, Status::Skip);

        let root = tmp.path().join(".wizard").join("checkpoints");
        std::fs::create_dir_all(root.join("3")).unwrap();
        std::fs::create_dir_all(root.join("9")).unwrap(); // stale: not indexed
        let record = serde_json::json!({
            "turn": 3,
            "tool": "write_file",
            "path": "/tmp/x",
            "snap": "3/0.snap",
            "existed_before": true,
        });
        std::fs::write(root.join("index.jsonl"), format!("{record}\n")).unwrap();

        let check = check_checkpoints(tmp.path());
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(check.detail.contains("1 snapshot(s)"), "{}", check.detail);
        assert!(check.detail.contains("1 stale"), "{}", check.detail);

        // Corrupt index lines fail the check.
        std::fs::write(root.join("index.jsonl"), "not json\n").unwrap();
        assert_eq!(check_checkpoints(tmp.path()).status, Status::Fail);
    }

    #[test]
    fn native_tools_check_counts_the_registry() {
        let check = check_native_tools();
        assert_eq!(check.status, Status::Pass);
        let count = ToolRegistry::with_native_tools().len();
        assert!(check.detail.contains(&count.to_string()));
    }

    #[test]
    fn platform_check_always_passes_and_names_the_host() {
        let check = check_platform();
        assert_eq!(check.status, Status::Pass);
        assert_eq!(check.label, "platform");
        // Off Termux/NixOS the detail is "os/arch"; on those hosts it is a
        // longer advisory. Either way it must be non-empty.
        assert!(!check.detail.is_empty());
        if crate::platform::is_termux() {
            assert!(check.detail.to_ascii_lowercase().contains("termux"));
        } else if crate::platform::is_nixos() {
            assert!(check.detail.to_ascii_lowercase().contains("nixos"));
        } else {
            assert!(check.detail.contains(std::env::consts::OS));
            assert!(check.detail.contains(std::env::consts::ARCH));
        }
    }

    #[test]
    fn color_depth_is_reported_and_never_a_failure() {
        // The line CHANGELOG 2.0.0 offers in place of the removed `/theme`. It
        // reports the depth the UI resolved and the variables behind it, and a
        // monochrome terminal is a fact about the host, not a fault — failing
        // it would break `wizard doctor &&` on every `NO_COLOR` machine.
        let check = check_color_depth();
        assert_eq!(check.status, Status::Pass);
        assert_eq!(check.label, "color depth");
        // The environment is whatever the suite runs under, so the assertion
        // is that the verdict is the same one the UI acts on.
        assert!(
            check
                .detail
                .starts_with(crate::theme::ColorDepth::detect().label()),
            "{}",
            check.detail
        );
    }

    #[test]
    fn the_check_battery_reports_the_colour_depth() {
        // Written as a check, but reached only from run_checks: an unwired
        // check is a doc claim with nothing behind it, which is what this
        // whole line was fixing.
        let tmp = std::env::temp_dir().join(format!("wizard-doctor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("temp project root");
        let checks = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_checks(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(
            checks.iter().any(|check| check.label == "color depth"),
            "run_checks must include the colour-depth line: {:?}",
            checks.iter().map(|c| &c.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn render_marks_and_aligns() {
        let checks = vec![
            Check::pass("ok", "fine"),
            Check::fail("broken-thing", "nope"),
            Check::skip("na", "nothing to do"),
        ];
        let report = render(&checks);
        let lines: Vec<&str> = report.lines().collect();
        assert!(lines[0].starts_with("✓ ok"));
        assert!(lines[1].starts_with("✗ broken-thing"));
        assert!(lines[2].starts_with("– na"));
        assert!(has_failures(&checks));
        assert!(!has_failures(&[Check::pass("a", ""), Check::skip("b", "")]));
    }

    #[tokio::test]
    async fn provider_check_skips_when_env_and_stored_key_are_both_absent() {
        // The provider name must also miss the (real) credentials store for
        // the probe to be skipped; a nonsense name guarantees that.
        let provider = ProviderConfig {
            name: "wizard-doctor-test-provider-never-stored".to_string(),
            kind: crate::config::ProviderKind::OPENAI,
            base_url: "https://example.invalid/v1".to_string(),
            model: "gpt-test".to_string(),
            api_key_env: Some("WIZARD_DOCTOR_TEST_KEY_THAT_IS_NEVER_SET".to_string()),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };
        let check = check_provider(&provider).await;
        assert_eq!(check.status, Status::Skip);
        assert!(check.detail.contains("not set"), "{}", check.detail);
        assert!(check.detail.contains("no stored key"), "{}", check.detail);
    }

    #[test]
    fn active_provider_check_flags_unknown_selection() {
        let provider = ProviderConfig {
            name: "local".to_string(),
            kind: crate::config::ProviderKind::LLAMACPP,
            base_url: "http://127.0.0.1:11435".to_string(),
            model: "qwen3.6:27b".to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };

        let config = Config {
            providers: vec![provider.clone()],
            active_provider: Some("local".to_string()),
            ..Config::default()
        };
        let check = check_active_provider(&config);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);

        let config = Config {
            providers: vec![provider],
            active_provider: Some("claud".to_string()),
            ..Config::default()
        };
        let check = check_active_provider(&config);
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("'claud'"), "{}", check.detail);
        assert!(
            check.detail.contains("falling back to 'local'"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn credentials_check_skips_passes_and_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials.toml");

        // Absent: nothing stored, nothing to check.
        assert_eq!(check_credentials_file(&path).status, Status::Skip);

        // Valid store with tight permissions: pass.
        std::fs::write(&path, "[keys]\nopenai = \"sk-test\"\n").unwrap();
        secrets::harden_file(&path).unwrap();
        let check = check_credentials_file(&path);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(check.detail.contains("1 stored key(s)"), "{}", check.detail);

        // Readable by other local users: fail (the file holds plaintext
        // secrets).
        secrets::expose_to_other_users(&path).unwrap();
        let check = check_credentials_file(&path);
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("readable by other users"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("chmod 600"), "{}", check.detail);
        secrets::harden_file(&path).unwrap();

        // Corrupt TOML: fail loudly instead of degrading to "no stored keys".
        std::fs::write(&path, "this is not valid toml = = =").unwrap();
        assert_eq!(check_credentials_file(&path).status, Status::Fail);
    }

    #[test]
    fn secret_storage_check_names_every_path_other_users_can_read() {
        // Nothing there yet: a fresh install has no state to judge, which is a
        // skip rather than a pass on zero paths.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("wizard-home");
        assert_eq!(check_secret_storage(&home).status, Status::Skip);

        // The tree as `ensure_dirs` leaves it, plus an OAuth token file.
        secrets::create_private_dir(&home).unwrap();
        secrets::create_private_dir(&home.join("logs")).unwrap();
        std::fs::write(home.join("xai_oauth.json"), "{\"access_token\":\"x\"}").unwrap();
        secrets::harden_file(&home.join("xai_oauth.json")).unwrap();
        let check = check_secret_storage(&home);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(check.detail.contains("3 path(s)"), "{}", check.detail);

        // A token file another local user can read (an older release, a
        // restored backup, a careless chmod) is the whole reason this check
        // exists, and the failure has to name the path so the user can fix
        // that one.
        secrets::expose_to_other_users(&home.join("xai_oauth.json")).unwrap();
        let check = check_secret_storage(&home);
        assert_eq!(check.status, Status::Fail, "{}", check.detail);
        assert!(check.detail.contains("xai_oauth.json"), "{}", check.detail);
        assert!(!check.detail.contains("logs"), "{}", check.detail);

        // A readable state directory counts too: the transcripts inside it are
        // readable through it whatever their own modes say.
        secrets::harden_file(&home.join("xai_oauth.json")).unwrap();
        secrets::expose_to_other_users(&home).unwrap();
        let check = check_secret_storage(&home);
        assert_eq!(check.status, Status::Fail, "{}", check.detail);
        assert!(
            check.detail.contains(&home.display().to_string()),
            "{}",
            check.detail
        );
    }

    #[test]
    fn secret_storage_reports_an_unreadable_path_instead_of_calling_it_absent() {
        // `Path::exists()` is `metadata().is_ok()`, so a path that cannot be
        // stat'd used to be filtered out as "not there" and the user was told
        // their state directory was absent when it was present and
        // unreadable. A `~/.wizard` that is a plain file reproduces that
        // without needing a permission trick (and so without depending on the
        // test running as a non-root user): every path under it stats ENOTDIR.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("wizard-home");
        std::fs::write(&home, "not a directory\n").unwrap();
        crate::platform::secrets::harden_file(&home).unwrap();

        let check = check_secret_storage(&home);
        assert_eq!(check.status, Status::Fail, "{}", check.detail);
        assert!(check.detail.contains("logs"), "{}", check.detail);
        assert!(!check.detail.contains("absent"), "{}", check.detail);
    }

    #[test]
    fn a_loose_tree_only_fails_when_the_filesystem_could_carry_the_fix() {
        // exFAT, FAT32, WSL DrvFs and shares without POSIX modes cannot
        // express owner-only permissions at all. `Config::ensure_dirs` warns
        // and carries on there by design, so failing doctor over the same tree
        // fails it on every run forever, advising a chmod the filesystem
        // cannot honour, and `wizard doctor && wizard -p "…"` never runs its
        // second command.
        let dir = PathBuf::from("/mnt/c/wizard");
        let states = vec![(dir.clone(), PathState::Loose)];

        let check = secret_storage_verdict(&dir, &states, true);
        assert_eq!(check.status, Status::Fail, "{}", check.detail);
        assert!(check.detail.contains("chmod 700"), "{}", check.detail);

        let check = secret_storage_verdict(&dir, &states, false);
        assert_eq!(check.status, Status::Skip, "{}", check.detail);
        // Still named, so the user learns the tree is exposed; just not
        // advised to run the command that cannot work.
        assert!(check.detail.contains("/mnt/c/wizard"), "{}", check.detail);
        assert!(!check.detail.contains("chmod 700"), "{}", check.detail);
        assert!(
            check.detail.contains("cannot express owner-only"),
            "{}",
            check.detail
        );

        // A platform with no answer at all is a skip too, and never a pass:
        // "protected" must never be claimed by something that did not look.
        let states = vec![(dir.clone(), PathState::Unknown("unsupported".to_string()))];
        let check = secret_storage_verdict(&dir, &states, true);
        assert_eq!(check.status, Status::Skip, "{}", check.detail);
        assert!(check.detail.contains("unsupported"), "{}", check.detail);
    }

    #[cfg(unix)]
    #[test]
    fn the_permission_probe_answers_yes_on_a_filesystem_with_modes_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(filesystem_can_restrict(tmp.path()));
        // The probe is a write, so it has to leave nothing behind: doctor runs
        // on every `/doctor` in the TUI.
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_unsupported_platform_answer_is_recognized_through_the_context_chain() {
        // `platform::secrets` attaches the path with `with_context` before we
        // see the error, so matching on the outermost one would never fire and
        // every check would turn "this platform has no answer" into a failure
        // its user cannot clear.
        let unsupported = anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "owner-only permissions are not implemented on this platform yet",
        ))
        .context("inspecting /home/you/.wizard/credentials.toml");
        assert!(is_unsupported(&unsupported));

        let denied =
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                .context("inspecting /home/you/.wizard/credentials.toml");
        assert!(!is_unsupported(&denied));
        assert!(!is_unsupported(&anyhow::anyhow!("no io error in here")));
    }

    #[test]
    fn system_prompt_check_breaks_the_prompt_down_by_section() {
        let check = check_system_prompt(crate::config::Mode::Genie);
        // A measurement, never a verdict: it must not move the exit code.
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        // Every section is named, because the point of the breakdown is
        // knowing which one grew.
        let sections = crate::agent::prompts::system_prompt_sections(
            crate::config::Mode::Genie,
            &[],
            None,
            None,
        );
        assert!(sections.len() >= 3, "{sections:?}");
        for section in &sections {
            assert!(check.detail.contains(section.name), "{}", check.detail);
        }
        // The totals are the assembled prompt's, not a placeholder.
        let total = crate::agent::prompts::join_sections(&sections).len();
        assert!(check.detail.contains(&kib(total)), "{}", check.detail);
        assert!(
            check
                .detail
                .contains(&kib(crate::agent::prompts::cache_breakpoint(&sections))),
            "{}",
            check.detail
        );
    }

    #[test]
    fn gateway_check_skips_when_kind_is_none_and_no_token() {
        let config = Config {
            gateway: crate::config::GatewayConfig {
                kind: crate::config::GatewayKind::None,
                ..Default::default()
            },
            ..Config::default()
        };
        // Without a stored telegram token this is a skip. We cannot force
        // credentials::get to miss if the real home has a token, so only
        // assert the none/no-token path when get returns None.
        if crate::credentials::get("telegram").is_none() {
            let checks = check_gateway(&config);
            assert_eq!(checks.len(), 1);
            assert_eq!(checks[0].status, Status::Skip, "{}", checks[0].detail);
            assert!(checks[0].detail.contains("none"), "{}", checks[0].detail);
        }
    }

    #[test]
    fn gateway_check_telegram_reports_token_status_without_leaking_secret() {
        let config = Config {
            gateway: crate::config::GatewayConfig {
                kind: crate::config::GatewayKind::Telegram,
                token_env: Some("WIZARD_DOCTOR_TEST_TG_TOKEN_NEVER_SET".to_string()),
                allowed_chat_ids: vec![1],
            },
            ..Config::default()
        };
        let checks = check_gateway(&config);
        assert!(
            checks
                .iter()
                .any(|c| c.label == "gateway" && c.status == Status::Pass),
            "{checks:?}"
        );
        // Token check: either pass (if real credentials have a token) or fail.
        let token = checks
            .iter()
            .find(|c| c.label == "gateway token")
            .expect("token check present");
        assert!(
            !token.detail.contains(":")
                || token.detail.contains("credentials.toml")
                || token.detail.contains("missing")
                || token.detail.contains("WIZARD_DOCTOR"),
            "must not leak a raw token: {}",
            token.detail
        );
        // Process check is always present for telegram.
        assert!(
            checks.iter().any(|c| c.label == "gateway process"),
            "{checks:?}"
        );
    }

    #[test]
    fn gateway_check_fails_on_an_empty_telegram_allow_list() {
        // The allow-list is closed: empty means nobody is authorized, so the
        // bot answers no one. Silence is the only symptom, which is exactly
        // what doctor exists to name.
        let config = Config {
            gateway: crate::config::GatewayConfig {
                kind: crate::config::GatewayKind::Telegram,
                token_env: Some("WIZARD_DOCTOR_TEST_TG_TOKEN_NEVER_SET".to_string()),
                allowed_chat_ids: Vec::new(),
            },
            ..Config::default()
        };
        let check = check_gateway_allow_list(&config);
        assert_eq!(check.status, Status::Fail, "{}", check.detail);
        assert!(
            check.detail.contains("allowed_chat_ids"),
            "{}",
            check.detail
        );
        // …and it rides along with the rest of the telegram battery.
        assert!(
            check_gateway(&config)
                .iter()
                .any(|c| c.label == "gateway allow-list" && c.status == Status::Fail),
            "{:?}",
            check_gateway(&config)
        );

        let configured = Config {
            gateway: crate::config::GatewayConfig {
                allowed_chat_ids: vec![-100123, 42],
                ..config.gateway.clone()
            },
            ..Config::default()
        };
        let check = check_gateway_allow_list(&configured);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(check.detail.contains("2 chat id(s)"), "{}", check.detail);
    }

    // -----------------------------------------------------------------------
    // bundle (`wizard doctor --bundle`)
    // -----------------------------------------------------------------------

    /// A stored provider key with no vendor prefix and no recognizable shape.
    /// Only the literal-substitution layer can catch this one, which is the
    /// point: most providers issue opaque keys.
    const OPAQUE_KEY: &str = "Zq7mVt2ePd91LsKr4HnWbC6yTx0Ug385";
    /// The same thing in the shape Together, DeepInfra and Voyage issue: 64
    /// characters of lower-case hex. It starts with a letter on purpose, so it
    /// is a perfectly legal identifier and therefore indistinguishable from a
    /// variable name by shape alone.
    const LOWERCASE_HEX_KEY: &str =
        "a8f2c1d3e4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1";
    /// The classic `<bot id>:<secret>` Telegram shape.
    const TELEGRAM_TOKEN: &str = "8123456789:AAF7hs_Jd0Kq2mLp9rTvXy31Zb4cNe6WQuA";
    const OAUTH_ACCESS: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ3aXphcmQtZG9jdG9yLXRlc3QifQ.7f3Qy1cV0mKpZr8sLdNbXt2AeJhUgW4iRc6Ov5nTyQk";
    const OAUTH_REFRESH: &str = "rt_9GkPz3XwQm5LbVn7TdYh2Rc8Ju4Fs61AeWoZ0iNqB";
    const OPENAI_KEY: &str = "sk-proj-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0";
    /// A bearer credential with no shape at all: recognizable only from the
    /// `Bearer` in front of it.
    const BEARER_OPAQUE: &str = "opaque-bearer-9f8e7d6c5b4a3210";
    /// Planted in the older session file, which must not be the one bundled.
    const STALE_MARKER: &str = "STALE-SESSION-MARKER";

    fn write_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// A fake `~/.wizard` holding one of every bundle input, each carrying a
    /// planted secret of a different class.
    fn fake_wizard_dir(root: &Path) -> PathBuf {
        let dir = root.join("wizard-home");
        write_file(
            &dir.join("config.toml"),
            &format!(
                "model = \"gpt-5\"\nmode = \"genie\"\n\n\
                 [[providers]]\n\
                 name = \"openai\"\nkind = \"openai\"\n\
                 base_url = \"https://api.openai.com/v1\"\nmodel = \"gpt-5\"\n\
                 api_key_env = \"OPENAI_API_KEY\"\n\
                 inline_api_key = \"{OPENAI_KEY}\"\n\n\
                 [gateway]\nkind = \"telegram\"\ntoken_env = \"WIZARD_TELEGRAM_TOKEN\"\n"
            ),
        );
        write_file(
            &dir.join("credentials.toml"),
            &format!("[keys]\nopenai = \"{OPAQUE_KEY}\"\ntelegram = \"{TELEGRAM_TOKEN}\"\n"),
        );
        write_file(
            &dir.join("chatgpt_oauth.json"),
            &format!(
                "{{\"access_token\":\"{OAUTH_ACCESS}\",\"refresh_token\":\"{OAUTH_REFRESH}\"}}\n"
            ),
        );
        // Two sessions: only the newest belongs in the bundle.
        write_file(
            &dir.join("sessions").join("2026-01-01T00-00-00.jsonl"),
            &format!("{{\"role\":\"user\",\"content\":\"{STALE_MARKER}\"}}\n"),
        );
        write_file(
            &dir.join("sessions").join("2026-02-02T00-00-00.jsonl"),
            &format!(
                "{{\"role\":\"user\",\"content\":\"curl -H 'Authorization: Bearer {BEARER_OPAQUE}' \
                 https://api.openai.com/v1/models\"}}\n\
                 {{\"role\":\"assistant\",\"content\":\"try api_key={OPENAI_KEY} instead\"}}\n"
            ),
        );
        write_file(
            &dir.join("usage.jsonl"),
            &format!("{{\"provider\":\"openai\",\"key\":\"{OPAQUE_KEY}\",\"tokens_in\":10}}\n"),
        );
        write_file(
            &dir.join("evolution.jsonl"),
            &format!("{{\"kind\":\"skill\",\"refresh_token\":\"{OAUTH_REFRESH}\"}}\n"),
        );
        write_file(
            &dir.join("logs").join("2026-02-02T00-00-00.jsonl"),
            &format!(
                "{{\"level\":\"debug\",\"msg\":\"GET https://api.telegram.org/bot{TELEGRAM_TOKEN}/getUpdates\"}}\n\
                 {{\"level\":\"debug\",\"msg\":\"access_token: {OAUTH_ACCESS}\"}}\n"
            ),
        );
        dir
    }

    /// Build a bundle over [`fake_wizard_dir`]. The rendered report carries a
    /// secret too, so `doctor.txt` is covered by the same assertions.
    fn planted_bundle() -> (tempfile::TempDir, Bundle) {
        let tmp = tempfile::tempdir().unwrap();
        let home = fake_wizard_dir(tmp.path());
        let dest = tmp.path().join("bundle");
        let report = format!("✗ gateway token  rejected by telegram ({TELEGRAM_TOKEN})");
        let bundle = write_bundle(&home, &dest, &report).expect("bundle writes");
        (tmp, bundle)
    }

    /// Every member's text, so a redaction test asserts over the whole bundle
    /// rather than one file at a time.
    fn all_members(bundle: &Bundle) -> String {
        bundle
            .members
            .iter()
            .map(|member| std::fs::read_to_string(bundle.dir.join(member)).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn bundle_writes_the_expected_members() {
        let (_tmp, bundle) = planted_bundle();
        for member in [
            "config.toml",
            "session.jsonl",
            "usage.jsonl",
            "evolution.jsonl",
            "logs/2026-02-02T00-00-00.jsonl",
            "doctor.txt",
            "manifest.json",
            "README.txt",
        ] {
            assert!(
                bundle.dir.join(member).is_file(),
                "missing member {member}: {:?}",
                bundle.members
            );
            assert!(
                bundle.members.iter().any(|listed| listed == member),
                "unlisted member {member}: {:?}",
                bundle.members
            );
        }
        // Version and commit travel with the bundle.
        let manifest = std::fs::read_to_string(bundle.dir.join("manifest.json")).unwrap();
        assert!(
            manifest.contains(crate::update::current_version()),
            "{manifest}"
        );
        assert!(manifest.contains("\"wizard_commit\""), "{manifest}");
        assert!(manifest.contains(std::env::consts::ARCH), "{manifest}");
        // The newest session, not whichever the directory listed first.
        let session = std::fs::read_to_string(bundle.dir.join("session.jsonl")).unwrap();
        assert!(!session.contains(STALE_MARKER), "{session}");
        // The redacted config is still TOML: a table followed by scalar keys
        // is the shape a naive re-serializer emits in the wrong order, and a
        // failure there would leave a comment here instead of a config.
        let config = std::fs::read_to_string(bundle.dir.join("config.toml")).unwrap();
        assert!(config.contains("[gateway]"), "{config}");
        assert!(config.contains("mode = \"genie\""), "{config}");
        // The credential store and the OAuth token files are never members.
        assert!(!bundle.dir.join("credentials.toml").exists());
        assert!(!bundle.dir.join("chatgpt_oauth.json").exists());
    }

    #[test]
    fn bundle_tolerates_absent_inputs_including_the_logs_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("fresh-install");
        std::fs::create_dir_all(&home).unwrap();
        let dest = tmp.path().join("bundle");

        let bundle = write_bundle(&home, &dest, "– mcp  no MCP servers configured").unwrap();

        assert!(dest.join("manifest.json").is_file());
        assert!(dest.join("doctor.txt").is_file());
        for absent in [
            "config.toml",
            "session.jsonl",
            "usage.jsonl",
            "evolution.jsonl",
            "logs/",
        ] {
            assert!(
                bundle.omitted.iter().any(|listed| listed == absent),
                "{absent} should be recorded as absent: {:?}",
                bundle.omitted
            );
        }
    }

    #[test]
    fn bundle_config_allowlist_redacts_a_field_no_rule_knows_about() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        // Neither value has a vendor shape, a credential-ish key name, nor a
        // literal on disk to substitute: the allowlist is the only layer that
        // can stop them, which is exactly what a field added next release
        // looks like.
        write_file(
            &home.join("config.toml"),
            "model = \"gpt-5\"\n\n\
             [[providers]]\n\
             name = \"openai\"\n\
             base_url = \"https://api.openai.com/v1\"\n\
             api_key_env = \"OPENAI_API_KEY\"\n\
             future_handshake_material = \"quokka-marmalade-77\"\n\n\
             [experimental]\n\
             nested = { deeper = \"pangolin-sunset-31\" }\n",
        );
        let dest = tmp.path().join("bundle");
        write_bundle(&home, &dest, "").unwrap();

        let config = std::fs::read_to_string(dest.join("config.toml")).unwrap();
        assert!(!config.contains("quokka-marmalade-77"), "{config}");
        assert!(!config.contains("pangolin-sunset-31"), "{config}");
        assert!(config.contains(REDACTED), "{config}");
        // …and the fields a provider bug report is made of survive, so the
        // assertions above are not passing on an empty file.
        assert!(config.contains("gpt-5"), "{config}");
        assert!(config.contains("https://api.openai.com/v1"), "{config}");
        assert!(config.contains("OPENAI_API_KEY"), "{config}");
    }

    #[test]
    fn bundle_omits_a_config_it_cannot_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        write_file(
            &home.join("config.toml"),
            &format!("this is not = = toml\ninline_key = \"{OPENAI_KEY}\"\n"),
        );
        let dest = tmp.path().join("bundle");
        let bundle = write_bundle(&home, &dest, "").unwrap();

        let config = std::fs::read_to_string(dest.join("config.toml")).unwrap();
        assert!(!config.contains(OPENAI_KEY), "{config}");
        assert!(config.contains("does not parse"), "{config}");
        // The member exists but holds nothing of the config, so the manifest
        // has to record the omission: `members` alone reads as "the config was
        // fine" to whoever triages the bundle.
        assert!(
            bundle
                .omitted
                .iter()
                .any(|listed| listed.starts_with("config.toml contents")),
            "{:?}",
            bundle.omitted
        );
        let manifest = std::fs::read_to_string(dest.join("manifest.json")).unwrap();
        assert!(manifest.contains("config.toml contents"), "{manifest}");
    }

    #[test]
    fn bundle_redacts_a_key_pasted_into_an_allowlisted_env_field() {
        // `api_key_env` is allowlisted because it names a variable. Confusing
        // it with the key itself is a one-character mistake, and no other
        // layer can catch an opaque value there: it is not on disk to
        // substitute and has no vendor prefix to recognize.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        write_file(
            &home.join("config.toml"),
            &format!(
                "[[providers]]\n\
                 name = \"openai\"\n\
                 base_url = \"https://api.openai.com/v1\"\n\
                 api_key_env = \"{OPAQUE_KEY}\"\n\n\
                 [web]\n\
                 search_api_key_env = \"TAVILY_API_KEY\"\n"
            ),
        );
        // The same paste also shows up in the transcript the user was
        // debugging with, so the literal has to leave the whole bundle.
        write_file(
            &home.join("sessions").join("2026-03-03T00-00-00.jsonl"),
            &format!("{{\"role\":\"user\",\"content\":\"is {OPAQUE_KEY} the right value?\"}}\n"),
        );
        let dest = tmp.path().join("bundle");
        let bundle = write_bundle(&home, &dest, "").unwrap();

        let text = all_members(&bundle);
        assert!(!text.contains(OPAQUE_KEY), "{text}");
        // A real variable name still survives: it is half of a provider bug
        // report and names nothing secret.
        let config = std::fs::read_to_string(dest.join("config.toml")).unwrap();
        assert!(config.contains("TAVILY_API_KEY"), "{config}");
        assert!(config.contains("https://api.openai.com/v1"), "{config}");
    }

    #[test]
    fn a_lower_case_opaque_key_pasted_into_api_key_env_never_reaches_the_bundle() {
        // The field holds a *name*, but confusing it with the key itself is a
        // one-character mistake, and then the field holds the credential. A
        // rule that asks whether the value "looks like a name rather than a
        // secret" passes everything it does not recognize, and this value is a
        // plain identifier: all lower-case, no punctuation, no vendor prefix,
        // and nothing on disk to substitute it with. Three layers miss it and
        // the bundle is a file the user is told to attach to a public issue.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        write_file(
            &home.join("config.toml"),
            &format!(
                "[[providers]]\n\
                 name = \"together\"\n\
                 base_url = \"https://api.together.xyz/v1\"\n\
                 api_key_env = \"{LOWERCASE_HEX_KEY}\"\n\n\
                 [[providers]]\n\
                 name = \"openai\"\n\
                 api_key_env = \"OPENAI_API_KEY\"\n"
            ),
        );
        // The same paste is in the transcript the user was debugging with, and
        // in the report, so every member has to lose it.
        write_file(
            &home.join("sessions").join("2026-05-05T00-00-00.jsonl"),
            &format!("{{\"role\":\"user\",\"content\":\"401 with {LOWERCASE_HEX_KEY}\"}}\n"),
        );
        let dest = tmp.path().join("bundle");
        let report = format!("✗ provider together  401 ({LOWERCASE_HEX_KEY})");
        // Nothing is set in the environment, so the only thing that can save
        // this key is the field gate itself.
        let bundle = write_bundle_with(&home, &dest, &report, |_| None).unwrap();

        let text = all_members(&bundle);
        assert!(
            !text.contains(LOWERCASE_HEX_KEY),
            "the pasted key shipped in the bundle:\n{text}"
        );
        let config = std::fs::read_to_string(dest.join("config.toml")).unwrap();
        assert!(config.contains(REDACTED), "{config}");
        // A `<redacted>` in a field that normally carries a variable name is
        // indistinguishable from a redactor bug unless the bundle says so.
        assert!(
            bundle
                .omitted
                .iter()
                .any(|listed| listed.contains("api_key_env")),
            "{:?}",
            bundle.omitted
        );
        let manifest = std::fs::read_to_string(dest.join("manifest.json")).unwrap();
        assert!(manifest.contains("api_key_env"), "{manifest}");
        // Not scorched earth: the confirmed name beside it is still readable,
        // which is what the assertions above would otherwise pass on.
        assert!(config.contains("OPENAI_API_KEY"), "{config}");
        assert!(config.contains("https://api.together.xyz/v1"), "{config}");
    }

    #[test]
    fn only_a_confirmed_name_passes_the_env_field_gate() {
        let unset: EnvLookup = &|_| None;
        // Spelled like the credential it points at, which every `*_env`
        // default Wizard ships and every provider convention is.
        for name in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "openrouter_key",
            "TAVILY_API_KEY",
            crate::config::GatewayConfig::DEFAULT_TOKEN_ENV,
            crate::llm::xai_oauth::DEFAULT_KEY_ENV,
            crate::llm::registry::defaults::OPENROUTER_KEY_ENV,
            crate::llm::registry::defaults::CLOUDFLARE_KEY_ENV,
        ] {
            // Wizard's own defaults are in the list on purpose: a default
            // renamed to something this gate cannot confirm would be withheld
            // from every bundled config, and this is where that shows up.
            assert!(is_confirmed_env_var_name(name, unset), "{name}");
        }
        // Opaque keys, including both single-case shapes the old rule waved
        // through and the mixed-case one it caught.
        for key in [
            LOWERCASE_HEX_KEY,
            "A8F2C1D3E4B5C6D7E8F9A0B1C2D3E4F5A6B7C8D9E0F1A2B3C4D5E6F7A8B9C0D1",
            OPAQUE_KEY,
        ] {
            assert!(!is_confirmed_env_var_name(key, unset), "{key}");
        }
        // A name that says nothing about credentials needs the other proof:
        // the environment defining it.
        let defined: EnvLookup = &|name| (name == "MY_PROVIDER_THING").then(|| "x".to_string());
        assert!(is_confirmed_env_var_name("MY_PROVIDER_THING", defined));
        assert!(!is_confirmed_env_var_name("MY_PROVIDER_THING", unset));
        // Not identifiers at all.
        for text in ["sk-proj-abcdef", "9LIVES_API_KEY", "", "two words"] {
            assert!(!is_confirmed_env_var_name(text, unset), "{text}");
        }
    }

    #[test]
    fn an_unconfirmed_env_var_name_is_looked_up_anyway_and_withheld_from_the_config() {
        // Both directions of the same "we cannot tell" case. The value is
        // queued for lookup whatever it is, because refusing to look one up is
        // how the real key stopped being substituted out of the transcript;
        // and it is withheld from the bundled config, because printing it is
        // how a pasted key shipped.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let raw = "[[providers]]\n\
                   name = \"custom\"\n\
                   api_key_env = \"WIZARD_DOCTOR_TEST_ODDLY_NAMED\"\n";
        write_file(&home.join("config.toml"), raw);

        let defined =
            |name: &str| (name == "WIZARD_DOCTOR_TEST_ODDLY_NAMED").then(|| OPAQUE_KEY.to_string());
        let secrets = known_secrets_from(&home, defined);
        assert!(
            secrets.iter().any(|secret| secret == OPAQUE_KEY),
            "the variable's value must be resolved: {secrets:?}"
        );
        // Defined in the environment: that is proof, so the name reads.
        let confirmed = redact_config_toml_with(raw, &defined);
        assert!(
            confirmed.text.contains("WIZARD_DOCTOR_TEST_ODDLY_NAMED"),
            "{}",
            confirmed.text
        );
        assert!(confirmed.withheld_env_fields.is_empty());

        // Undefined and spelled like nothing in particular: unconfirmable, so
        // it is withheld and the withholding is recorded.
        let unconfirmed = redact_config_toml_with(raw, &|_| None);
        assert!(
            !unconfirmed.text.contains("WIZARD_DOCTOR_TEST_ODDLY_NAMED"),
            "{}",
            unconfirmed.text
        );
        assert_eq!(unconfirmed.withheld_env_fields, vec!["api_key_env"]);
        // …and it joins the literals, because the other thing it can be is a
        // key that also sits in the transcript.
        let secrets = known_secrets_from(&home, |_| None);
        assert!(
            secrets
                .iter()
                .any(|secret| secret == "WIZARD_DOCTOR_TEST_ODDLY_NAMED"),
            "{secrets:?}"
        );
    }

    #[test]
    fn bundle_directory_is_private_even_when_it_already_existed_wide_open() {
        // The bundle holds a transcript and whatever the logs picked up, so it
        // is no more shareable than the state it was built from.
        //
        // The destination is deliberately pre-created readable by other users:
        // asserting on a directory this run created would pass on any host
        // whose umask is already 077, so the hardening could be deleted and the
        // test would still be green on the machine that deleted it.
        let tmp = tempfile::tempdir().unwrap();
        let home = fake_wizard_dir(tmp.path());
        let dest = tmp.path().join("bundle");
        std::fs::create_dir_all(&dest).unwrap();
        secrets::expose_to_other_users(&dest).unwrap();

        let bundle = write_bundle(&home, &dest, "").expect("bundle writes");
        assert!(
            secrets::is_protected(&bundle.dir).expect("stat the bundle dir"),
            "bundle dir is {}",
            secrets::protection_summary(&bundle.dir)
        );
    }

    #[test]
    fn the_bundles_logs_subdirectory_is_private_too() {
        // A private root with a readable `logs/` inside it hands the request
        // traces (headers included) to anyone who can guess the path.
        // Pre-created loose for the same reason the root's test does it:
        // asserting on a directory this run created would pass under a 077
        // umask with the hardening deleted.
        let tmp = tempfile::tempdir().unwrap();
        let home = fake_wizard_dir(tmp.path());
        let dest = tmp.path().join("bundle");
        std::fs::create_dir_all(dest.join("logs")).unwrap();
        secrets::expose_to_other_users(&dest.join("logs")).unwrap();

        let bundle = write_bundle(&home, &dest, "").expect("bundle writes");
        let logs = bundle.dir.join("logs");
        assert!(
            secrets::is_protected(&logs).expect("stat the logs dir"),
            "logs dir is {}",
            secrets::protection_summary(&logs)
        );
    }

    #[test]
    fn bundle_redacts_stored_provider_api_keys() {
        let (_tmp, bundle) = planted_bundle();
        let text = all_members(&bundle);
        // The opaque stored key, planted in usage.jsonl.
        assert!(!text.contains(OPAQUE_KEY), "{text}");
        // The prefixed one, planted in config.toml and mid-transcript.
        assert!(!text.contains(OPENAI_KEY), "{text}");
        assert!(text.contains(REDACTED), "{text}");
        // Not a scorched-earth pass: the transcript is still a transcript.
        assert!(text.contains("https://api.openai.com/v1"), "{text}");
    }

    #[test]
    fn bundle_redacts_oauth_access_and_refresh_tokens() {
        let (_tmp, bundle) = planted_bundle();
        let text = all_members(&bundle);
        assert!(!text.contains(OAUTH_ACCESS), "{text}");
        assert!(!text.contains(OAUTH_REFRESH), "{text}");
        // Not even the header segment of the JWT survives.
        assert!(
            !text.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"),
            "{text}"
        );
    }

    #[test]
    fn bundle_redacts_the_telegram_bot_token() {
        let (_tmp, bundle) = planted_bundle();
        let text = all_members(&bundle);
        assert!(!text.contains(TELEGRAM_TOKEN), "{text}");
        // Only the secret half is gone; the API host stays readable.
        assert!(text.contains("api.telegram.org"), "{text}");
    }

    #[test]
    fn bundle_redacts_bearer_credentials() {
        let (_tmp, bundle) = planted_bundle();
        let text = all_members(&bundle);
        assert!(!text.contains(BEARER_OPAQUE), "{text}");
        assert!(text.contains("Bearer"), "the scheme word stays: {text}");
    }

    #[test]
    fn scrub_text_redacts_credential_shapes_it_has_never_seen() {
        // Nothing on disk to substitute from: every one of these is caught on
        // shape alone, which is what covers a provider we do not store keys
        // for and a secret pasted into a transcript.
        let text = format!(
            "telegram 9988776655:BBFabcdefghijklmnopqrstuvwxyz0123456789 \
             openai {OPENAI_KEY} \
             jwt {OAUTH_ACCESS} \
             remote https://ci:hunter2secret@git.example.com/repo.git \
             header Authorization: Bearer {BEARER_OPAQUE}"
        );
        let scrubbed = scrub_text(&text, &[]);
        assert!(!scrubbed.contains("BBFabcdefghij"), "{scrubbed}");
        assert!(!scrubbed.contains(OPENAI_KEY), "{scrubbed}");
        assert!(!scrubbed.contains(OAUTH_ACCESS), "{scrubbed}");
        assert!(!scrubbed.contains("hunter2secret"), "{scrubbed}");
        assert!(!scrubbed.contains(BEARER_OPAQUE), "{scrubbed}");
        // The host survives the URL rewrite: it is half the bug report.
        assert!(scrubbed.contains("git.example.com/repo.git"), "{scrubbed}");
    }

    #[test]
    fn scrub_text_redacts_vendor_prefixed_credential_key_names() {
        // The dominant real-world form is `<VENDOR>_API_KEY` / `<vendor>_token`
        // with an opaque value: no vendor prefix, no JWT shape, nothing on
        // disk to substitute. An exact-match denylist misses every one of
        // them, which is how `env | grep API` in a transcript used to ship the
        // key verbatim.
        let text = "TAVILY_API_KEY=tvly-x9K2pQ7mVt2ePd91LsKr4HnW\n\
                    MISTRAL_API_KEY=Zx71QmRt4PkLd93VbNc0SgWy\n\
                    GITHUB_TOKEN=8f3a1c9d2e7b4056a1938d75cf20be44\n\
                    openai_api_key = Kd82nRv5TqXm40PbLw13ZsYh\n\
                    slack_bot_token: Gt64VnQz90MkRb27XcLp53Ws\n\
                    anthropic-api-key=Rw05KpZq83NbTm41VdXc72Lh";
        let scrubbed = scrub_text(text, &[]);
        for value in [
            "tvly-x9K2pQ7mVt2ePd91LsKr4HnW",
            "Zx71QmRt4PkLd93VbNc0SgWy",
            "8f3a1c9d2e7b4056a1938d75cf20be44",
            "Kd82nRv5TqXm40PbLw13ZsYh",
            "Gt64VnQz90MkRb27XcLp53Ws",
            "Rw05KpZq83NbTm41VdXc72Lh",
        ] {
            assert!(!scrubbed.contains(value), "{value} survived:\n{scrubbed}");
        }
        // The names stay: which variable a broken provider reads is most of a
        // provider bug report.
        assert!(scrubbed.contains("TAVILY_API_KEY"), "{scrubbed}");
        assert!(scrubbed.contains("GITHUB_TOKEN"), "{scrubbed}");
    }

    #[test]
    fn scrub_text_redacts_a_whole_pem_block() {
        // `private_key` was on the denylist, but the key-name layer claims one
        // word and a PEM body is a hundred ordinary base64 words, so a pasted
        // SSH key shipped in full.
        let body = "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAABlwAAAAdzc2gtcn\n\
                    NhAAAAAwEAAQAAAYEAwQ2mVt2ePd91LsKr4HnWbC6yTx0Ug385ZqJd0Kq2mLp9rTvXy31Z";
        let text = format!(
            "{{\"role\":\"user\",\"content\":\"here is my key\\n\
             -----BEGIN OPENSSH PRIVATE KEY-----\\n{body}\\n\
             -----END OPENSSH PRIVATE KEY-----\\nwhy does ssh refuse it?\"}}"
        );
        let scrubbed = scrub_text(&text, &[]);
        assert!(!scrubbed.contains("b3BlbnNzaC1rZXktdjEA"), "{scrubbed}");
        assert!(!scrubbed.contains("wQ2mVt2ePd91LsKr"), "{scrubbed}");
        assert!(!scrubbed.contains("PRIVATE KEY"), "{scrubbed}");
        assert!(scrubbed.contains(REDACTED), "{scrubbed}");
        // The question around the key is the bug report; it survives.
        assert!(scrubbed.contains("why does ssh refuse it?"), "{scrubbed}");

        // A block that never closes is fail-closed: everything after the
        // header goes, because there is no way to know where the body ends.
        let unterminated = format!("before\n-----BEGIN RSA PRIVATE KEY-----\n{body}\n");
        let (scrubbed, withheld) = scrub_text_parts(&unterminated, &[]);
        assert!(scrubbed.starts_with("before"), "{scrubbed}");
        assert!(!scrubbed.contains("b3BlbnNzaC1rZXktdjEA"), "{scrubbed}");
        assert!(withheld, "the withholding has to be reportable: {scrubbed}");
    }

    #[test]
    fn a_pem_header_does_not_swallow_the_records_after_it() {
        // The header and the footer land in two different transcript records,
        // 200 turns apart, because the user typed one line about their key and
        // then debugged the real bug. Spanning them would collapse the whole
        // session into one `<redacted>` and the triager would read the
        // truncation as the end of the transcript.
        let text = "{\"role\":\"user\",\"content\":\"my file starts with -----BEGIN OPENSSH PRIVATE KEY----- and ssh refuses it\"}\n\
                    {\"role\":\"assistant\",\"content\":\"the real bug is in the retry ladder\"}\n\
                    {\"role\":\"user\",\"content\":\"it ends with -----END OPENSSH PRIVATE KEY----- too\"}\n";
        let (scrubbed, withheld) = scrub_text_parts(text, &[]);
        assert!(
            scrubbed.contains("the real bug is in the retry ladder"),
            "{scrubbed}"
        );
        assert_eq!(scrubbed.lines().count(), 3, "{scrubbed}");
        assert!(withheld, "{scrubbed}");
        // The header itself is still gone: a quoted `-----BEGIN` costs the run
        // after it, which is the fail-closed half of the same rule.
        assert!(!scrubbed.contains("BEGIN OPENSSH"), "{scrubbed}");
    }

    #[test]
    fn a_pem_footer_without_its_closing_dashes_does_not_swallow_the_records_after_it() {
        // The other half of the same rule, and the arm the bound was missing:
        // an `-----END` whose trailing dashes were cut off by terminal
        // wrapping. Searching the *unbounded* remainder for those dashes finds
        // none, so the span ran to the end of the member and two whole records
        // disappeared with `omitted` still reading as complete.
        let text = "{\"role\":\"user\",\"content\":\"paste -----BEGIN OPENSSH PRIVATE KEY----- AAAA -----END OPENSSH PRIVATE KEY\"}\n\
                    {\"role\":\"assistant\",\"content\":\"the real bug is in the retry ladder\"}\n\
                    {\"role\":\"user\",\"content\":\"here is the stack trace\"}\n";
        let (scrubbed, withheld) = scrub_text_parts(text, &[]);
        assert!(
            scrubbed.contains("the real bug is in the retry ladder"),
            "{scrubbed}"
        );
        assert!(scrubbed.contains("here is the stack trace"), "{scrubbed}");
        assert_eq!(scrubbed.lines().count(), 3, "{scrubbed}");
        // The key material itself is still gone, footer or no footer.
        assert!(!scrubbed.contains("AAAA"), "{scrubbed}");
        assert!(!scrubbed.contains("BEGIN OPENSSH"), "{scrubbed}");
        assert!(withheld, "the withholding has to be reportable: {scrubbed}");

        // A block that does close normally is still consumed through its
        // footer, label and all, so the assertions above are not passing on a
        // redactor that stopped doing its job.
        let closed =
            "before -----BEGIN RSA PRIVATE KEY----- AAAA -----END RSA PRIVATE KEY----- after";
        let (scrubbed, withheld) = scrub_text_parts(closed, &[]);
        assert_eq!(scrubbed, format!("before {REDACTED} after"));
        assert!(!withheld, "{scrubbed}");
    }

    #[test]
    fn a_withheld_pem_tail_is_recorded_in_the_manifest() {
        // The one redaction that deletes text instead of replacing it, so the
        // member is shorter than its source with nothing in the bundle saying
        // so. `omitted` is where a reader already looks to tell "no logs" from
        // "logs withheld".
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        write_file(
            &home.join("sessions").join("2026-04-04T00-00-00.jsonl"),
            "my key starts with -----BEGIN OPENSSH PRIVATE KEY----- and then stops\n",
        );
        let dest = tmp.path().join("bundle");
        let bundle = write_bundle(&home, &dest, "").unwrap();

        assert!(
            bundle
                .omitted
                .iter()
                .any(|listed| listed.starts_with("session.jsonl:")
                    && listed.contains("unterminated PEM")),
            "{:?}",
            bundle.omitted
        );
        let manifest = std::fs::read_to_string(dest.join("manifest.json")).unwrap();
        assert!(manifest.contains("unterminated PEM"), "{manifest}");
    }

    #[test]
    fn scrub_text_leaves_prose_and_config_names_alone() {
        // A key name only redacts what follows it in assignment position;
        // "token count" is prose, and a bundle nobody can read is a bundle
        // nobody can debug from.
        let text = "the token count was 500 and $OPENAI_API_KEY was not set. \
                    base_url = https://api.openai.com/v1 (auth via credentials.toml)";
        assert_eq!(scrub_text(text, &[]), text);
    }

    #[test]
    fn known_secrets_reads_the_credential_store_and_oauth_files() {
        let tmp = tempfile::tempdir().unwrap();
        let home = fake_wizard_dir(tmp.path());
        let secrets = known_secrets(&home);
        for planted in [OPAQUE_KEY, TELEGRAM_TOKEN, OAUTH_ACCESS, OAUTH_REFRESH] {
            assert!(
                secrets.iter().any(|secret| secret == planted),
                "{planted} not collected: {secrets:?}"
            );
        }
        // Longest first, so a secret containing another is replaced whole.
        assert!(
            secrets
                .windows(2)
                .all(|pair| pair[0].len() >= pair[1].len()),
            "{secrets:?}"
        );
    }

    #[test]
    fn known_secrets_reads_literal_mcp_credentials() {
        // `[server.env]` has no `env:` indirection at all and the MCP docs
        // show a literal `Authorization` header, so mcp.toml is a file whose
        // own documentation demonstrates pasting a token into it. The agent
        // reading it with the read tool puts the whole body in the transcript.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        write_file(
            &home.join("mcp.toml"),
            &format!(
                "[[server]]\n\
                 name = \"search\"\n\
                 transport = \"stdio\"\n\
                 command = \"uvx\"\n\
                 args = [\"mcp-tavily\"]\n\
                 [server.env]\n\
                 TAVILY_API_KEY = \"{OPAQUE_KEY}\"\n\n\
                 [[server]]\n\
                 name = \"remote\"\n\
                 transport = \"http\"\n\
                 url = \"https://mcp.example.com/mcp\"\n\
                 [server.headers]\n\
                 Authorization = \"Bearer {BEARER_OPAQUE}\"\n\
                 X-Api-Key = \"env:WIZARD_DOCTOR_TEST_MCP_KEY\"\n"
            ),
        );
        let secrets = known_secrets_from(&home, |name| {
            (name == "WIZARD_DOCTOR_TEST_MCP_KEY").then(|| "mcp-env-key-9f8e7d6c".to_string())
        });
        for planted in [OPAQUE_KEY, BEARER_OPAQUE, "mcp-env-key-9f8e7d6c"] {
            assert!(
                secrets.iter().any(|secret| secret == planted),
                "{planted} not collected: {secrets:?}"
            );
        }
        // The command, the args, and the endpoint are not credentials; taking
        // them out would gut the bug report they belong to.
        for kept in ["mcp-tavily", "https://mcp.example.com/mcp"] {
            assert!(
                !secrets.iter().any(|secret| secret == kept),
                "{kept} must not be substituted out: {secrets:?}"
            );
        }
        // A transcript quoting the file loses the tokens and keeps the rest.
        let transcript = std::fs::read_to_string(home.join("mcp.toml")).unwrap();
        let scrubbed = scrub_text(&transcript, &secrets);
        assert!(!scrubbed.contains(OPAQUE_KEY), "{scrubbed}");
        assert!(!scrubbed.contains(BEARER_OPAQUE), "{scrubbed}");
        assert!(
            scrubbed.contains("https://mcp.example.com/mcp"),
            "{scrubbed}"
        );
    }

    #[test]
    fn known_secrets_resolves_env_named_keys_and_pasted_literals() {
        // config.toml names variables; the values live in the environment. A
        // `*_env` value that is not a variable name is the exception: it is a
        // key pasted into the wrong field, and it is a literal we can strip
        // from every member.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        write_file(
            &home.join("config.toml"),
            &format!(
                "[[providers]]\n\
                 name = \"openai\"\n\
                 api_key_env = \"WIZARD_DOCTOR_TEST_PROVIDER_KEY\"\n\n\
                 [[providers]]\n\
                 name = \"pasted\"\n\
                 api_key_env = \"{OPAQUE_KEY}\"\n\n\
                 [gateway]\n\
                 kind = \"telegram\"\n",
            ),
        );
        let secrets = known_secrets_from(&home, |name| match name {
            "WIZARD_DOCTOR_TEST_PROVIDER_KEY" => Some("resolved-provider-key-8821".to_string()),
            // The gateway's default token variable is always consulted, even
            // when config.toml never names it.
            crate::config::GatewayConfig::DEFAULT_TOKEN_ENV => Some(TELEGRAM_TOKEN.to_string()),
            _ => None,
        });
        for planted in ["resolved-provider-key-8821", TELEGRAM_TOKEN, OPAQUE_KEY] {
            assert!(
                secrets.iter().any(|secret| secret == planted),
                "{planted} not collected: {secrets:?}"
            );
        }
        // The variable *name* is not a secret; substituting it would redact
        // the one thing a provider bug report needs.
        assert!(
            !secrets
                .iter()
                .any(|secret| secret == "WIZARD_DOCTOR_TEST_PROVIDER_KEY"),
            "{secrets:?}"
        );
    }

    #[test]
    fn a_lower_case_env_var_name_is_still_resolved_not_treated_as_a_key() {
        // Lower-case environment variable names are legal and people use them.
        // Refusing to recognize one meant its value was never read, so the real
        // key stopped being substituted out of the transcript, and the name was
        // replaced with <redacted> in the bundled config on top of that: the
        // fail-open direction, wearing the fail-closed rule's clothes.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let raw = "[[providers]]\n\
                   name = \"openrouter\"\n\
                   api_key_env = \"openrouter_key\"\n";
        write_file(&home.join("config.toml"), raw);
        let secrets = known_secrets_from(&home, |name| {
            (name == "openrouter_key").then(|| OPAQUE_KEY.to_string())
        });
        assert!(
            secrets.iter().any(|secret| secret == OPAQUE_KEY),
            "the variable's value must be resolved: {secrets:?}"
        );
        assert!(
            !secrets.iter().any(|secret| secret == "openrouter_key"),
            "the name is not a secret: {secrets:?}"
        );

        // The transcript line the resolved literal is the only defence for:
        // `key` alone is not a credential-ish name, the value has no vendor
        // prefix, and it is not a JWT.
        let line = format!("{{\"body\":{{\"key\":\"{OPAQUE_KEY}\"}}}}");
        assert!(!scrub_text(&line, &secrets).contains(OPAQUE_KEY));

        // …and the variable name still survives into the bundled config, which
        // is what tells a triager which variable the broken provider reads.
        assert!(redact_config_toml(raw).contains("openrouter_key"));
    }

    #[test]
    fn an_ordinary_mcp_env_value_is_not_treated_as_a_secret() {
        // `[server.env]` is a free-form map and most of what it holds is
        // configuration. Collecting all of it made `known_secrets` return
        // "production" and "/home/you/projects", and layer 1 substitutes
        // literals by raw substring, so "the reproduction steps" came out as
        // "the re<redacted> steps" and every project path left the transcript.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        write_file(
            &home.join("mcp.toml"),
            &format!(
                "[[server]]\n\
                 name = \"files\"\n\
                 transport = \"stdio\"\n\
                 command = \"npx\"\n\
                 [server.env]\n\
                 NODE_ENV = \"production\"\n\
                 ALLOWED_DIRECTORIES = \"/home/you/projects\"\n\
                 TAVILY_API_KEY = \"{OPAQUE_KEY}\"\n"
            ),
        );
        let secrets = known_secrets_from(&home, |_| None);
        assert!(
            secrets.iter().any(|secret| secret == OPAQUE_KEY),
            "{secrets:?}"
        );
        for kept in ["production", "/home/you/projects"] {
            assert!(
                !secrets.iter().any(|secret| secret == kept),
                "{kept} must not be substituted out: {secrets:?}"
            );
        }
        let prose = "the reproduction steps run in /home/you/projects";
        assert_eq!(scrub_text(prose, &secrets), prose);
    }

    #[test]
    fn scrub_text_redacts_a_vendor_prefixed_bare_key_name() {
        // `TAVILY_KEY` and `XAI_KEY` are the second most common spelling after
        // `_API_KEY`, and neither `api_key` nor `secret_key` is a tail of them.
        let text = "TAVILY_KEY=tvly-x9K2pQ7mVt2ePd91LsKr4HnW\n\
                    XAI_KEY: Zx71QmRt4PkLd93VbNc0SgWy\n\
                    openai.key = Kd82nRv5TqXm40PbLw13ZsYh";
        let scrubbed = scrub_text(text, &[]);
        for value in [
            "tvly-x9K2pQ7mVt2ePd91LsKr4HnW",
            "Zx71QmRt4PkLd93VbNc0SgWy",
            "Kd82nRv5TqXm40PbLw13ZsYh",
        ] {
            assert!(!scrubbed.contains(value), "{value} survived:\n{scrubbed}");
        }
        assert!(scrubbed.contains("TAVILY_KEY"), "{scrubbed}");
        // A bare `key` stays prose: a map dump ("key = model") and the `[keys]`
        // section of a config are what a triager reads the bundle for.
        assert_eq!(scrub_text("key = model", &[]), "key = model");
        assert_eq!(
            scrub_text("the key was rotated", &[]),
            "the key was rotated"
        );
    }

    #[test]
    fn an_oversized_member_with_no_line_break_keeps_its_tail() {
        // One enormous record (a base64 attachment, a minified page) is the
        // shape that used to produce a zero-byte member: `read_tail` dropped
        // "the partial first line", and the whole file was one line, so the
        // triager got an empty log instead of the tail that explains the crash.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("huge.jsonl");
        let mut giant = "x".repeat(MEMBER_MAX_BYTES as usize + 4096);
        giant.push_str("TAILMARKER");
        std::fs::write(&path, &giant).unwrap();

        let (text, truncated) = read_tail(&path, MEMBER_MAX_BYTES).unwrap();
        assert!(truncated);
        assert!(!text.is_empty(), "the member must not be empty");
        assert!(text.ends_with("TAILMARKER"), "the tail is what is kept");
        assert!(text.len() as u64 <= MEMBER_MAX_BYTES);

        // A file that does have line breaks still loses its partial first line,
        // so the JSONL members stay parseable.
        let path = tmp.path().join("lines.jsonl");
        let record = format!("{{\"filler\":\"{}\"}}\n", "y".repeat(4096));
        let mut lines = String::new();
        while (lines.len() as u64) < MEMBER_MAX_BYTES + 8192 {
            lines.push_str(&record);
        }
        std::fs::write(&path, &lines).unwrap();
        let (text, truncated) = read_tail(&path, MEMBER_MAX_BYTES).unwrap();
        assert!(truncated);
        assert!(text.starts_with("{\"filler\""), "{}", &text[..40]);
    }

    /// A credential in a URL query string is redacted, and the rest of the URL
    /// survives.
    ///
    /// The gap this closes: `?key=…` reached the bundle byte for byte. `?`/`&`
    /// are word boundaries, so `key=<token>` arrived as its own word;
    /// `is_secret_name("key")` is false because the tail rule needs
    /// `name.len() > stem.len()` and `3 > 3` is not; there is no `@`, so the
    /// userinfo pass missed it; and `collect_mcp_secrets` walks `[server.env]`
    /// and `[server.headers]`, never `url`.
    ///
    /// It is not hypothetical. Wizard writes that URL into its own log on its
    /// own initiative — the MCP unreachable warning carries the configured
    /// URL — at the default `wizard=warn` filter, and `--bundle` copies the
    /// newest logs into the archive it tells the user to attach to a report.
    #[test]
    fn a_credential_in_a_url_query_string_is_redacted() {
        let line = "failed to reach MCP server 'notion' at \
                    https://mcp.example.com/mcp?key=1Dt6hQ9xR3vB7nK2mP5wZ8yA4cE0fG6jL1sT9uV";
        let scrubbed = scrub_text(line, &[]);
        assert!(
            !scrubbed.contains("1Dt6hQ9xR3vB7nK2mP5wZ8yA4cE0fG6jL1sT9uV"),
            "the key survived: {scrubbed}"
        );
        // What a triager still needs.
        assert!(scrubbed.contains("mcp.example.com/mcp"), "{scrubbed}");
        assert!(scrubbed.contains("key="), "{scrubbed}");
        assert!(scrubbed.contains("notion"), "{scrubbed}");

        // The other shapes from the same family.
        for (name, value) in [
            ("sig", "abc123def456"),
            ("code", "4/0AbCdEfGhIjKlMnOp"),
            ("session", "s%3Aabcdef.ghijkl"),
            ("X-Amz-Signature", "deadbeefcafe"),
        ] {
            let url = format!("https://h.example.com/p?{name}={value}");
            let out = scrub_text(&url, &[]);
            assert!(!out.contains(value), "{name} survived: {out}");
        }

        // Non-credential parameters are left alone — a URL with its query
        // intact is often the whole point of the log line.
        let plain = "https://api.example.com/v1/search?q=rust&page=2";
        assert_eq!(scrub_text(plain, &[]), plain);
    }

    #[test]
    fn url_query_redaction_keeps_the_rest_of_the_query() {
        let out = scrub_text("https://h.example.com/p?a=1&token=SEKRIT&b=2", &[]);
        assert!(out.contains("a=1"), "{out}");
        assert!(out.contains("b=2"), "{out}");
        assert!(!out.contains("SEKRIT"), "{out}");
    }

    #[test]
    fn url_userinfo_redaction_stays_targeted() {
        // Only a password in the userinfo is a credential.
        assert_eq!(
            redact_url_userinfo("https://ci:hunter2secret@git.example.com/repo.git").as_deref(),
            Some("https://<redacted>@git.example.com/repo.git")
        );
        // A bare username is not, and a bug report needs to know whose remote
        // failed.
        assert_eq!(redact_url_userinfo("https://user@host/repo.git"), None);
        // An `@` in the path is not userinfo at all.
        assert_eq!(redact_url_userinfo("https://host/pkg@1.2.3/file"), None);
        assert_eq!(redact_url_userinfo("not a url"), None);
        // Non-ASCII around the split points: the slicing is by byte offset, so
        // a multi-byte host or path must not panic.
        assert_eq!(
            redact_url_userinfo("https://héllo.example.com/ünïcode"),
            None
        );
        assert_eq!(
            redact_url_userinfo("https://üser:påss@héllo.example.com/ünï").as_deref(),
            Some("https://<redacted>@héllo.example.com/ünï")
        );
    }

    #[test]
    fn a_baked_in_commit_wins_over_the_deep_evolve_checkout() {
        // The arm no build in this repo takes yet: nothing sets WIZARD_COMMIT,
        // so `option_env!` is None in every binary we produce and the branch is
        // reachable only through this seam. Testing it through `build_commit`
        // would assert whatever this particular build happened to compile in.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        assert_eq!(build_commit_from(Some("deadbee"), &home), "deadbee");
        // Trimmed, because a build system that pipes `git rev-parse` into the
        // variable hands us the trailing newline with it.
        assert_eq!(build_commit_from(Some("deadbee\n"), &home), "deadbee");
        // An empty or blank value is not a commit: fall through rather than
        // reporting "" as the build's identity.
        assert_eq!(build_commit_from(Some(""), &home), "unknown");
        assert_eq!(build_commit_from(Some("  "), &home), "unknown");
        assert_eq!(build_commit_from(None, &home), "unknown");
    }

    #[test]
    fn build_commit_falls_back_to_the_deep_evolve_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");

        // No ~/.wizard/src at all: honest "unknown", no git invocation.
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(build_commit_from(None, &home), "unknown");

        // A src/ directory that is not a checkout is still "unknown".
        std::fs::create_dir_all(home.join("src")).unwrap();
        assert_eq!(build_commit_from(None, &home), "unknown");

        // A real deep-evolve checkout reports its HEAD. Identity is passed on
        // the command line so the test never depends on the machine's git
        // config (CI has none).
        let source = home.join("src");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .args([
                    "-c",
                    "user.email=doctor@wizard.test",
                    "-c",
                    "user.name=doctor",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
        };
        let Ok(init) = git(&["init", "--quiet"]) else {
            return; // no git on this machine; the fallback above is covered.
        };
        assert!(init.status.success(), "git init: {init:?}");
        std::fs::write(source.join("lib.rs"), "// deep evolve checkout\n").unwrap();
        assert!(git(&["add", "lib.rs"]).unwrap().status.success());
        let commit = git(&["commit", "--quiet", "-m", "seed"]).unwrap();
        assert!(commit.status.success(), "git commit: {commit:?}");

        let short = build_commit_from(None, &home);
        assert_ne!(short, "unknown", "expected a short HEAD");
        assert!(
            short.len() >= 7 && short.chars().all(|ch| ch.is_ascii_hexdigit()),
            "not a short commit hash: {short}"
        );
    }
}
