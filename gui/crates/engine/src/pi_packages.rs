//! Pi packages on this device: the personal package list from pi's agent
//! settings plus the `pi` CLI's own install/remove/update commands.
//! Settings → Plugins drives these over relay-forwardable RPCs, so the
//! page manages whichever device's Pi it targets.
//!
//! Pi owns the package state (`<agent-dir>/settings.json` `packages`, managed
//! npm/git checkouts under the agent dir). This module only reads it and runs
//! `pi` with a validated source as a direct argument — never through a shell.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeron_proto::HarnessId;

const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const REMOVE_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const UPDATE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_SOURCE_LEN: usize = 512;

/// Where a configured package comes from, as pi's `parseSource` classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PiPackageKind {
    Npm,
    Git,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiPackage {
    /// The source exactly as pi recorded it — what `pi remove`/`pi update` take.
    pub source: String,
    pub kind: PiPackageKind,
    /// npm package name, `host/owner/repo`, or the local path.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// An object-form entry narrowing which resources load.
    #[serde(default)]
    pub filtered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiPackages {
    pub pi_installed: bool,
    /// An installer for the Pi CLI is available on this device.
    pub can_install_pi: bool,
    pub packages: Vec<PiPackage>,
    /// `settings.json` exists but could not be read; `packages` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_error: Option<String>,
}

/// Pi's agent directory: `PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
pub fn agent_dir() -> Option<PathBuf> {
    agent_dir_with(std::env::var_os("PI_CODING_AGENT_DIR"), home_dir())
}

fn agent_dir_with(env: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(dir) = env.filter(|dir| !dir.is_empty()) {
        let dir = PathBuf::from(dir);
        if let (Ok(rest), Some(home)) = (dir.strip_prefix("~"), home.as_ref()) {
            return Some(home.join(rest));
        }
        return Some(dir);
    }
    home.map(|home| home.join(".pi").join("agent"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn pi_cli() -> Option<PathBuf> {
    zeron_harness::PiHarness::new().resolve_executable().ok()
}

/// The configured personal packages plus whether `pi` can run here.
pub fn list() -> PiPackages {
    let (packages, settings_error) = match agent_dir() {
        Some(dir) => read_packages(&dir),
        None => (Vec::new(), None),
    };
    PiPackages {
        pi_installed: pi_cli().is_some(),
        can_install_pi: zeron_harness::install::can_install(HarnessId::Pi),
        packages,
        settings_error,
    }
}

fn read_packages(agent_dir: &Path) -> (Vec<PiPackage>, Option<String>) {
    let text = match std::fs::read_to_string(agent_dir.join("settings.json")) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), None),
        Err(err) => {
            return (
                Vec::new(),
                Some(format!("Could not read Pi settings: {err}")),
            );
        }
    };
    match parse_packages(&text) {
        Ok(entries) => (
            entries
                .into_iter()
                .map(|(source, filtered)| describe(agent_dir, source, filtered))
                .collect(),
            None,
        ),
        Err(err) => (
            Vec::new(),
            Some(format!("Could not parse Pi settings: {err}")),
        ),
    }
}

/// `packages` entries as `(source, filtered)`: plain strings, or objects
/// carrying `source` plus optional resource filters.
fn parse_packages(text: &str) -> Result<Vec<(String, bool)>, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let Some(entries) = value.get("packages").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(entries
        .iter()
        .filter_map(|entry| match entry {
            serde_json::Value::String(source) => Some((source.trim().to_string(), false)),
            serde_json::Value::Object(object) => {
                let source = object.get("source")?.as_str()?.trim().to_string();
                let filtered = ["extensions", "skills", "prompts", "themes"]
                    .iter()
                    .any(|key| object.contains_key(*key));
                Some((source, filtered))
            }
            _ => None,
        })
        .filter(|(source, _)| !source.is_empty())
        .collect())
}

fn describe(agent_dir: &Path, source: String, filtered: bool) -> PiPackage {
    let (kind, name, install_dir) = classify(agent_dir, &source);
    let manifest = install_dir
        .and_then(|dir| std::fs::read_to_string(dir.join("package.json")).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let field = |key: &str| {
        manifest
            .as_ref()
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|v| !v.is_empty())
    };
    PiPackage {
        kind,
        name,
        version: field("version"),
        description: field("description"),
        filtered,
        source,
    }
}

/// Mirrors pi's `parseSource`: `npm:` specs, `git:`/protocol URLs, and
/// everything else is a local path.
fn classify(agent_dir: &Path, source: &str) -> (PiPackageKind, String, Option<PathBuf>) {
    if let Some(spec) = source.strip_prefix("npm:") {
        let name = npm_name(spec.trim()).to_string();
        let dir = agent_dir.join("npm").join("node_modules").join(&name);
        return (PiPackageKind::Npm, name, Some(dir));
    }
    let git = source.strip_prefix("git:").map(str::trim).or_else(|| {
        ["https://", "http://", "ssh://", "git://"]
            .iter()
            .any(|p| source.starts_with(p))
            .then_some(source)
    });
    if let Some(repo) = git
        && let Some((host, path)) = git_host_path(repo)
    {
        let dir = agent_dir.join("git").join(&host).join(&path);
        return (PiPackageKind::Git, format!("{host}/{path}"), Some(dir));
    }
    let path = source.strip_prefix("file:").unwrap_or(source);
    let resolved = if let Some(rest) = path.strip_prefix("~/") {
        home_dir().map(|home| home.join(rest))
    } else {
        Some(agent_dir.join(path))
    };
    (PiPackageKind::Local, source.to_string(), resolved)
}

/// One identity for a package however its source was spelled, the way Pi
/// tells packages apart: npm by package name, git by `host/owner/repo`
/// without the ref, anything else by the source itself. For a configured
/// package this is its [`PiPackage::name`]; the Plugins page uses it to line
/// Wizard's installs up with Pi's.
pub fn package_key(source: &str) -> String {
    let source = source.trim();
    if let Some(spec) = source.strip_prefix("npm:") {
        return npm_name(spec.trim()).to_string();
    }
    let git = source.strip_prefix("git:").map(str::trim).or_else(|| {
        ["https://", "http://", "ssh://", "git://"]
            .iter()
            .any(|p| source.starts_with(p))
            .then_some(source)
    });
    if let Some(repo) = git
        && let Some((host, path)) = git_host_path(repo)
    {
        return format!("{host}/{path}");
    }
    source.to_string()
}

/// `@scope/name@1.2.3` → `@scope/name`; `name@^1` → `name`.
fn npm_name(spec: &str) -> &str {
    let search_from = usize::from(spec.starts_with('@'));
    match spec[search_from..].find('@') {
        Some(at) => &spec[..search_from + at],
        None => spec,
    }
}

/// `(host, owner/repo)` from a git source without its ref: scp-like
/// `git@host:owner/repo`, protocol URLs, or `host/owner/repo` shorthand.
fn git_host_path(repo: &str) -> Option<(String, String)> {
    let repo = repo.split('#').next().unwrap_or(repo);
    let (host, path) = if let Some(rest) = repo.strip_prefix("git@") {
        rest.split_once(':')?
    } else {
        let rest = ["https://", "http://", "ssh://", "git://"]
            .iter()
            .find_map(|p| repo.strip_prefix(p))
            .unwrap_or(repo);
        let (authority, path) = rest.split_once('/')?;
        // Drop `user@` and `:port` from the authority; an `@` after the
        // first slash is the ref, stripped below.
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (host.split(':').next().unwrap_or(host), path)
    };
    let path = path.split('@').next().unwrap_or(path);
    let path = path.trim_matches('/').trim_end_matches(".git");
    if host.is_empty() || path.split('/').filter(|s| !s.is_empty()).count() < 2 {
        return None;
    }
    Some((host.to_string(), path.to_string()))
}

/// Turn what a user typed (or a gallery pick) into a source pi accepts.
///
/// Bare npm names gain `npm:`; `github:owner/repo`, `git@host:…`, and
/// `host.tld/owner/repo` shorthands gain the `git:` prefix pi needs to treat
/// them as git (without it pi reads them as local paths). Explicit paths
/// (`./`, `../`, `/`, `~/`) pass through. Anything that could read as a flag
/// or carries whitespace/control characters is refused.
pub fn normalize_source(input: &str) -> Result<String, String> {
    const HINT: &str = "Use npm:<package>, git:<host>/<owner>/<repo>, a URL, or a local path";
    let source = input.trim();
    if source.is_empty() {
        return Err("Enter a package source".into());
    }
    if source.len() > MAX_SOURCE_LEN {
        return Err("That source is too long".into());
    }
    if source.starts_with('-') {
        return Err(HINT.into());
    }
    if source.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("A package source can't contain spaces".into());
    }
    if let Some(spec) = source.strip_prefix("npm:") {
        return if is_npm_spec(spec) {
            Ok(source.to_string())
        } else {
            Err(format!("`{spec}` isn't a valid npm package name"))
        };
    }
    if let Some(rest) = source.strip_prefix("github:") {
        return git_host_path(&format!("github.com/{rest}"))
            .map(|_| format!("git:github.com/{rest}"))
            .ok_or_else(|| HINT.into());
    }
    if source.starts_with("git:") {
        return git_host_path(&source[4..])
            .map(|_| source.to_string())
            .ok_or_else(|| HINT.into());
    }
    if ["https://", "http://", "ssh://", "git://"]
        .iter()
        .any(|p| source.starts_with(p))
    {
        return git_host_path(source)
            .map(|_| source.to_string())
            .ok_or_else(|| HINT.into());
    }
    if source.starts_with("git@") {
        return git_host_path(source)
            .map(|_| format!("git:{source}"))
            .ok_or_else(|| HINT.into());
    }
    if ["./", "../", "/", "~/", "file:"]
        .iter()
        .any(|p| source.starts_with(p))
        || (cfg!(windows) && source.get(1..3) == Some(":\\"))
    {
        return Ok(source.to_string());
    }
    if let Some((host, _)) = source.split_once('/')
        && host.contains('.')
        && !host.starts_with('@')
    {
        return git_host_path(source)
            .map(|_| format!("git:{source}"))
            .ok_or_else(|| HINT.into());
    }
    if is_npm_spec(source) {
        return Ok(format!("npm:{source}"));
    }
    Err(HINT.into())
}

/// npm package name (optionally scoped) with an optional `@version` suffix.
fn is_npm_spec(spec: &str) -> bool {
    let name = npm_name(spec);
    let version = &spec[name.len()..];
    if version == "@" || version.chars().any(char::is_whitespace) {
        return false;
    }
    let valid_part = |part: &str| {
        !part.is_empty()
            && part.len() <= 214
            && !part.starts_with(['.', '_'])
            && part.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_' | '~')
            })
    };
    match name.strip_prefix('@') {
        Some(scoped) => scoped
            .split_once('/')
            .is_some_and(|(scope, pkg)| valid_part(scope) && valid_part(pkg)),
        None => valid_part(name),
    }
}

/// Serializes package operations: pi rewrites `settings.json` and the managed
/// checkouts, and two concurrent runs would race on both.
static OPERATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `pi install <source>` into the personal scope; returns the refreshed list.
pub async fn install(source: &str) -> Result<PiPackages, String> {
    let source = normalize_source(source)?;
    run_pi(&["install", &source], INSTALL_TIMEOUT).await?;
    Ok(list())
}

/// `pi remove <source>` for a configured personal package.
pub async fn remove(source: &str) -> Result<PiPackages, String> {
    let source = configured_source(source)?;
    run_pi(&["remove", &source], REMOVE_TIMEOUT).await?;
    Ok(list())
}

/// `pi update --extension <source>` for one package, or
/// `pi update --extensions` for all of them (never Pi itself).
pub async fn update(source: Option<&str>) -> Result<PiPackages, String> {
    match source {
        Some(source) => {
            let source = configured_source(source)?;
            run_pi(&["update", "--extension", &source], UPDATE_TIMEOUT).await?;
        }
        None => run_pi(&["update", "--extensions"], UPDATE_TIMEOUT).await?,
    }
    Ok(list())
}

/// Remove/update act only on sources pi already has configured, verbatim.
fn configured_source(source: &str) -> Result<String, String> {
    let source = source.trim();
    let configured = list();
    configured
        .packages
        .iter()
        .find(|package| package.source == source)
        .map(|package| package.source.clone())
        .ok_or_else(|| format!("{source} isn't an installed Pi package"))
}

async fn run_pi(args: &[&str], timeout: Duration) -> Result<(), String> {
    let program = pi_cli().ok_or("Pi isn't installed on this device")?;
    let _operation = OPERATION.lock().await;
    let mut command = zeron_harness::install::cli_command(&program);
    command
        .args(args)
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0");
    // Project-scoped settings come from the working directory; run from
    // home so only the personal scope is ever read or written.
    if let Some(home) = home_dir() {
        command.current_dir(home);
    }
    let verb = args.first().copied().unwrap_or("command");
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return Err(format!("Could not run pi: {err}")),
        Err(_) => {
            return Err(format!(
                "pi {verb} did not finish within {} minutes",
                timeout.as_secs() / 60
            ));
        }
    };
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = tail(if stderr.trim().is_empty() {
        &stdout
    } else {
        &stderr
    });
    Err(if detail.is_empty() {
        format!("pi {verb} failed ({})", output.status)
    } else {
        detail
    })
}

/// The last few meaningful lines of CLI output, ANSI escapes stripped.
fn tail(text: &str) -> String {
    let clean = strip_ansi(text);
    let lines: Vec<&str> = clean
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(4);
    lines[start..].join("\n")
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_dir_honors_override_and_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            agent_dir_with(None, home.clone()),
            Some(PathBuf::from("/home/u/.pi/agent"))
        );
        assert_eq!(
            agent_dir_with(Some("/opt/pi".into()), home.clone()),
            Some(PathBuf::from("/opt/pi"))
        );
        assert_eq!(
            agent_dir_with(Some("~/pi-agent".into()), home.clone()),
            Some(PathBuf::from("/home/u/pi-agent"))
        );
        assert_eq!(
            agent_dir_with(Some("".into()), home),
            Some(PathBuf::from("/home/u/.pi/agent"))
        );
    }

    #[test]
    fn parses_string_and_object_package_entries() {
        let text = r#"{
            "defaultModel": "x",
            "packages": [
                "npm:pi-mcp-adapter",
                {"source": "git:github.com/o/r@v1", "skills": []},
                {"source": "./local"},
                {"nosource": true},
                42,
                "  "
            ]
        }"#;
        assert_eq!(
            parse_packages(text).unwrap(),
            vec![
                ("npm:pi-mcp-adapter".to_string(), false),
                ("git:github.com/o/r@v1".to_string(), true),
                ("./local".to_string(), false),
            ]
        );
        assert!(parse_packages("{}").unwrap().is_empty());
        assert!(parse_packages("{ not json").is_err());
    }

    #[test]
    fn describes_installed_npm_packages_from_their_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("npm/node_modules/@scope/tools");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@scope/tools","version":"1.4.0","description":"Handy tools"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"packages":["npm:@scope/tools@^1","git:github.com/o/r"]}"#,
        )
        .unwrap();
        let (packages, error) = read_packages(dir.path());
        assert_eq!(error, None);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].kind, PiPackageKind::Npm);
        assert_eq!(packages[0].name, "@scope/tools");
        assert_eq!(packages[0].version.as_deref(), Some("1.4.0"));
        assert_eq!(packages[0].description.as_deref(), Some("Handy tools"));
        assert_eq!(packages[1].kind, PiPackageKind::Git);
        assert_eq!(packages[1].name, "github.com/o/r");
        assert_eq!(packages[1].version, None);

        std::fs::write(dir.path().join("settings.json"), "{").unwrap();
        let (packages, error) = read_packages(dir.path());
        assert!(packages.is_empty());
        assert!(error.is_some());
        assert_eq!(
            read_packages(&dir.path().join("missing")),
            (Vec::new(), None)
        );
    }

    #[test]
    fn package_keys_match_what_pi_lists() {
        assert_eq!(package_key("npm:@s/tools@^1"), "@s/tools");
        assert_eq!(package_key("git:github.com/o/r@v1"), "github.com/o/r");
        assert_eq!(package_key("https://github.com/o/r"), "github.com/o/r");
        assert_eq!(package_key("./local"), "./local");
        let dir = tempfile::tempdir().unwrap();
        let (kind, name, _) = classify(dir.path(), "git:github.com/o/r@v2");
        assert_eq!(kind, PiPackageKind::Git);
        assert_eq!(name, package_key("git:github.com/o/r@v2"));
    }

    #[test]
    fn npm_names_drop_versions() {
        assert_eq!(npm_name("pi-tools"), "pi-tools");
        assert_eq!(npm_name("pi-tools@1.0.0"), "pi-tools");
        assert_eq!(npm_name("@scope/pi-tools"), "@scope/pi-tools");
        assert_eq!(npm_name("@scope/pi-tools@^2"), "@scope/pi-tools");
    }

    #[test]
    fn git_sources_resolve_host_and_repo() {
        let hp = |s| git_host_path(s).map(|(h, p)| format!("{h}|{p}"));
        assert_eq!(hp("github.com/o/r").as_deref(), Some("github.com|o/r"));
        assert_eq!(hp("github.com/o/r@v1").as_deref(), Some("github.com|o/r"));
        assert_eq!(
            hp("https://gitlab.com/g/sub/r.git").as_deref(),
            Some("gitlab.com|g/sub/r")
        );
        assert_eq!(
            hp("git@github.com:o/r.git").as_deref(),
            Some("github.com|o/r")
        );
        assert_eq!(
            hp("ssh://git@host.dev/o/r").as_deref(),
            Some("host.dev|o/r")
        );
        assert_eq!(hp("github.com/only"), None);
    }

    #[test]
    fn normalizes_what_users_type() {
        let ok = |s: &str| normalize_source(s).unwrap();
        assert_eq!(ok("pi-mcp-adapter"), "npm:pi-mcp-adapter");
        assert_eq!(ok(" @scope/tools@1.2.0 "), "npm:@scope/tools@1.2.0");
        assert_eq!(ok("npm:@scope/tools"), "npm:@scope/tools");
        assert_eq!(ok("github:o/r"), "git:github.com/o/r");
        assert_eq!(ok("github.com/o/r@v1"), "git:github.com/o/r@v1");
        assert_eq!(ok("git:github.com/o/r"), "git:github.com/o/r");
        assert_eq!(ok("git@github.com:o/r"), "git:git@github.com:o/r");
        assert_eq!(ok("https://github.com/o/r"), "https://github.com/o/r");
        assert_eq!(ok("./local-package"), "./local-package");
        assert_eq!(ok("~/pkgs/tools"), "~/pkgs/tools");

        for bad in [
            "",
            "   ",
            "--local",
            "-e",
            "npm:",
            "npm:Bad Name",
            "npm:UPPER",
            "has space",
            "pi tools",
            "line\nbreak",
            "https://github.com/only",
            "Not_A_Name!",
        ] {
            assert!(normalize_source(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(normalize_source(&"a".repeat(MAX_SOURCE_LEN + 1)).is_err());
    }

    #[test]
    fn cli_output_tail_is_clean_and_short() {
        let out = "\u{1b}[2mResolving…\u{1b}[0m\n\nstep 1\nstep 2\nstep 3\n\u{1b}[31mError: no such package\u{1b}[0m\n";
        assert_eq!(tail(out), "step 1\nstep 2\nstep 3\nError: no such package");
        assert_eq!(tail(""), "");
    }
}
