//! Self-update: the `wizard update` command and the passive startup check.
//!
//! Releases are published on GitHub as `wizard-<target>.tar.gz` (each tarball
//! holding a single `wizard` binary) with a companion `checksums.txt`. This
//! module picks the right asset for the machine (mirroring `install.sh`),
//! downloads it, verifies its sha256, and swaps it in atomically via a rename
//! in the same directory as the running executable — renaming over a running
//! binary is fine on Unix, and the displaced binary is kept as `<name>.bak`
//! for `--rollback`.
//!
//! Three rules hold everywhere in here, because a self-updater that breaks them
//! is a remote code execution primitive:
//!
//! - **Verification is mandatory.** No `checksums.txt`, an unreadable one, or
//!   no entry for the asset all abort the update. Nothing unverified is ever
//!   written over the binary, and nothing unverified is ever executed.
//! - **The checksums are signed.** `checksums.txt` is fetched from the same
//!   host as the tarball, so on its own it proves the download was not
//!   corrupted in transit and nothing else. Its detached minisign signature
//!   (`checksums.txt.minisig`) is what makes the digest an assertion by the
//!   holder of the release key, and it is checked against a public key
//!   compiled into this binary. Missing, unparseable, signed by another key,
//!   or simply wrong: all abort. There is no flag that skips it.
//! - **Staging is private.** Downloads land in `~/.wizard/update` at 0700,
//!   never the shared system temp dir: on the escalation path the staged file
//!   is handed to `sudo install`, so a world-writable staging path with a
//!   predictable name would let any local user have root install their binary.
//!
//! A download is not always possible. A binary compiled with the placeholder
//! key cannot verify any release, and some hosts (NixOS without a static musl
//! loader, Termux) have no runnable prebuilt. Those two cases fall back to
//! cloning the tag and `cargo build --release --locked`, which is the same
//! trust as `WIZARD_BUILD_FROM_SOURCE=1` on `install.sh`. A failed signature
//! or digest is never a reason to compile something else. The background
//! auto-update path does not take this fallback: compiling for minutes in a
//! fire-and-forget task is worse than leaving the notice.
//!
//! A download mirror can be put in front of GitHub with `WIZARD_MIRROR` (off by
//! default). It changes which host answers and nothing else: the rules above
//! hold whoever that is, any mirror failure falls back to GitHub, and the user
//! is told which one served the binary. See the mirror section below.
//!
//! [`crate::platform::exe_swap::install_executable`] is the one place a binary
//! is swapped into position; deep evolve (`crate::evolve`) installs its rebuilt
//! binary through it too.
//!
//! The passive check ([`maybe_check_on_startup`]) is a courtesy notice by
//! default and only ever installs anything when `[update].auto` is set. It is
//! fire-and-forget so it never delays the TUI, and swallows every error.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::config::{Config, UpdateConfig};
use crate::platform::exe_swap;
use crate::platform::paths;

/// GitHub repo serving Wizard releases. `[update].repo` overrides this for the
/// passive startup check and auto-update only; `wizard update` always uses it.
const DEFAULT_REPO: &str = "teddytennant/wizard";

/// Ceiling on a single downloaded file, applied while it streams.
///
/// Everything that decides whether these bytes are ours runs after all of them
/// have arrived, so this is the only bound that exists before verification.
/// 256 MB is an order of magnitude above the largest real asset and well below
/// anything that troubles a machine.
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Ceiling on the two files fetched before any tarball: `checksums.txt` and
/// `checksums.txt.minisig`.
///
/// Both are read into memory rather than streamed to disk, and both come from
/// the same not-yet-trusted host as the tarball — earlier, in fact, since they
/// are what decides whether the tarball is ours. A real checksums.txt is a few
/// hundred bytes and a minisig is four lines, so 1 MB is three orders of
/// magnitude of headroom and still far below anything that troubles a machine
/// that has agreed to trust nothing yet.
const MAX_METADATA_BYTES: u64 = 1024 * 1024;

/// Per-request bound on those same two fetches. The client itself carries no
/// total timeout, because the request after these streams a release tarball
/// over whatever link the user has; a small file that never finishes arriving
/// is a different thing, and it must not hang `wizard update` forever.
const METADATA_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP timeout for the passive startup check — short so a hung network can
/// never leave the fire-and-forget task lingering.
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// HTTP timeout for the interactive `wizard update` command, which is allowed
/// to wait a little longer than the passive check.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// The compiled version of this binary (`CARGO_PKG_VERSION`). Always a full
/// three-component semver — release tags and self-update comparison depend on
/// it parsing.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The version as shown to the user (`wizard --version`, the welcome banner):
/// a trailing `.0` patch is dropped, so `0.7.0` reads as `0.7` while
/// `0.7.1` stays `0.7.1`. Cosmetic only — never used for version comparison.
pub fn display_version() -> &'static str {
    short_version(current_version())
}

/// Drop a trailing `.0` patch component (`0.7.0` → `0.7`; `0.7.1` unchanged).
fn short_version(version: &str) -> &str {
    version.strip_suffix(".0").unwrap_or(version)
}

/// User-Agent the GitHub API requires (`wizard/<version>`).
fn user_agent() -> String {
    format!("wizard/{}", current_version())
}

/// Strip a single leading `v` (`v0.5.0` → `0.5.0`) for semver parsing.
fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// True when `latest` parses to a strictly greater semver than `current`.
/// Unparseable versions compare `false`, so a garbled tag degrades to "no
/// update" rather than an error.
fn is_newer(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(strip_v(latest)),
        semver::Version::parse(strip_v(current)),
    ) {
        (Ok(l), Ok(c)) => l > c,
        _ => false,
    }
}

/// Ensure a user-supplied tag carries the leading `v` the release tags use.
fn normalize_tag(tag: &str) -> Result<String> {
    let trimmed = tag.trim();
    // The tag is interpolated straight into the URLs this fetches from, and it
    // comes from `--to` or from a `[update].repo` the user was talked into
    // pointing somewhere. `../../` in it redirects the fetch. Nothing is
    // *installed* that way — the signature is checked either way, and now bound
    // to this very string — but a fetch that quietly goes somewhere else is not
    // a thing to leave available, and an unusable tag is better refused with a
    // reason than turned into a 404 three requests later.
    //
    // The character set is what a release tag is made of, which is narrower
    // than what a URL tolerates.
    if trimmed.is_empty() {
        bail!("an empty release tag cannot be fetched");
    }
    let body = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let usable = !body.is_empty()
        && body
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+'));
    if !usable {
        bail!(
            "{trimmed:?} is not a release tag; expected something like v2.0.0              (letters, digits, and . - _ + only)"
        );
    }
    Ok(match trimmed.starts_with('v') {
        true => trimmed.to_string(),
        false => format!("v{trimmed}"),
    })
}

/// Normalize `std::env::consts::ARCH` to the release naming (`x86_64` /
/// `aarch64`). `None` for architectures we publish no asset for.
fn normalize_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    }
}

/// Release-asset file names to try, most-preferred first — the pure decision
/// behind [`asset_candidates`], factored out so it is unit-testable. Mirrors
/// `install.sh`: macOS → the per-arch Darwin build; Termux → empty (no Android
/// prebuilt; source build only); NixOS Linux → musl then gnu (no FHS loader
/// for the gnu build); other Linux → gnu then musl.
fn asset_candidates_for(os: &str, arch: &str, nixos: bool, termux: bool) -> Vec<String> {
    if os == "macos" {
        return vec![format!("wizard-{arch}-apple-darwin.tar.gz")];
    }
    // Termux is Android/Bionic: stock gnu/musl release binaries do not run.
    // Returning no candidates makes `wizard update` fail cleanly with a
    // source-build hint instead of downloading a binary that cannot start.
    if termux {
        return Vec::new();
    }
    let gnu = format!("wizard-{arch}-unknown-linux-gnu.tar.gz");
    let musl = format!("wizard-{arch}-unknown-linux-musl.tar.gz");
    if nixos {
        vec![musl, gnu]
    } else {
        vec![gnu, musl]
    }
}

/// Rewrite the plain-build candidates into native-GUI ones: a binary with the
/// `native` feature must not silently update itself into a binary without it —
/// `wizard gui` would stop opening a window, and the launcher entry
/// or shell alias pointing at it would stop working with no explanation.
///
/// Only `-gnu` and `-darwin` survive: there is no musl native asset. winit
/// reaches X11 and Wayland through `dlopen`, which a fully static musl binary
/// cannot do, so on a machine where the gnu build will not run there is
/// nothing to fall back to, and failing loudly beats quietly removing the
/// window.
fn native_assets(candidates: Vec<String>) -> Vec<String> {
    candidates
        .into_iter()
        .filter(|asset| asset.contains("-gnu") || asset.contains("-darwin"))
        .map(|asset| asset.replacen("wizard-", "wizard-native-", 1))
        .collect()
}

/// The release-asset candidates for this machine, or an error on an
/// architecture we ship no binary for / a host with no matching prebuilt.
fn asset_candidates() -> Result<Vec<String>> {
    let arch = normalize_arch(std::env::consts::ARCH).ok_or_else(|| {
        anyhow!(
            "no prebuilt wizard release for this CPU architecture ({})",
            std::env::consts::ARCH
        )
    })?;
    let candidates = asset_candidates_for(
        std::env::consts::OS,
        arch,
        crate::platform::is_nixos(),
        crate::platform::is_termux(),
    );
    if candidates.is_empty() {
        if let Some(hint) = crate::platform::termux_prebuilt_hint() {
            bail!("{hint}");
        }
        bail!(
            "no prebuilt wizard release for this platform ({}/{})",
            std::env::consts::OS,
            arch
        );
    }
    if cfg!(feature = "native") {
        return Ok(native_assets(candidates));
    }
    Ok(candidates)
}

// ---------------------------------------------------------------------------
// The download mirror
// ---------------------------------------------------------------------------
//
// GitHub Releases is the source of truth: tags, release notes, provenance and
// the fallback all live there. A mirror in front of it is a bandwidth
// optimisation and a set of stable URLs, and it must never be a single point of
// failure, so every failure of the mirror falls back to GitHub and the user is
// told which one served the download.
//
// Two rules keep the mirror from being a second place to compromise:
//
// - **Verification does not know where the bytes came from.** The signature on
//   `checksums.txt` and the sha256 of every tarball are checked by the same
//   code on the same bytes whichever host answered, and a failure of either is
//   fatal rather than a reason to ask someone else (see [`SourceFailure`]).
// - **The mirror never decides which version you get.** The tag is resolved
//   from the GitHub API and the mirror is then read at `<mirror>/<tag>/`, so a
//   mirror that stopped updating can only fail to answer. It is the reason the
//   client never reads the mutable `latest/` prefix, which exists on the mirror
//   for humans who want a URL that does not change.

/// The mirror used when `WIZARD_MIRROR` is unset.
///
/// Empty on purpose: `dl.<domain>` does not exist yet, and a default pointing
/// at a host that does not answer would make every install and every update
/// pay a failed request and a fallback warning to gain nothing. Put the host
/// here in the same change that makes it real; until then the mirror is opt-in.
const DEFAULT_MIRROR: &str = "";

/// Environment variable overriding [`DEFAULT_MIRROR`]. A host (`dl.example.com`
/// or `https://dl.example.com`) turns the mirror on; `off`, `none`, `0` or the
/// empty string turn it off.
const MIRROR_ENV: &str = "WIZARD_MIRROR";

/// The configured mirror setting: the environment first, else the default.
fn mirror_setting() -> String {
    std::env::var(MIRROR_ENV).unwrap_or_else(|_| DEFAULT_MIRROR.to_string())
}

/// Normalize a mirror setting into a scheme-qualified root with no trailing
/// slash, or `None` when the mirror is off. Pure, so the off-switch and the
/// URL shape are testable without touching the environment.
fn mirror_root(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches('/');
    if matches!(
        raw.to_ascii_lowercase().as_str(),
        "" | "0" | "off" | "none" | "false"
    ) {
        return None;
    }
    if raw.contains("://") {
        Some(raw.to_string())
    } else {
        Some(format!("https://{raw}"))
    }
}

/// One host a release can be fetched from: a human-readable `label` for the
/// line that says which one was used, and the `base` every asset name hangs
/// off.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseSource {
    label: String,
    base: String,
}

/// The hosts to try for `tag`, in order: the mirror when one is configured,
/// then GitHub Releases.
///
/// GitHub is always last and always present. A mirror can be added, misspelt
/// or unreachable; none of those may remove the source of truth from the list.
fn release_sources(mirror: Option<&str>, repo: &str, tag: &str) -> Vec<ReleaseSource> {
    let mut sources = Vec::new();
    if let Some(root) = mirror.and_then(mirror_root) {
        sources.push(ReleaseSource {
            label: format!("the mirror at {root}"),
            base: format!("{root}/{tag}"),
        });
    }
    sources.push(ReleaseSource {
        label: "GitHub Releases".to_string(),
        base: format!("https://github.com/{repo}/releases/download/{tag}"),
    });
    sources
}

/// Why one source did not produce an installed binary.
///
/// The distinction is the whole fallback policy:
///
/// - [`Self::Unavailable`] is "this host did not serve the release": a
///   connection failure, a 404, a mirror that has not caught up. Nothing is
///   wrong with the release, so the next source is tried.
/// - [`Self::Fatal`] is "stop". Two things produce it, and neither is fixed by
///   asking a different host: bytes that failed the release signature or a
///   digest, and an install that could not be completed locally. A host serving
///   something the release key did not sign is an attack signal, and quietly
///   installing from GitHub instead would both hide it from the user and let
///   whoever controls the mirror choose which host you end up trusting.
#[derive(Debug)]
enum SourceFailure {
    Unavailable(anyhow::Error),
    Fatal(anyhow::Error),
}

/// One source's attempt, boxed so the driver below can take it as a plain
/// `dyn Fn` and stay free of the generics that would otherwise infect every
/// caller. `Send`, because the passive startup check drives it from a spawned
/// task.
type SourceAttempt<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = std::result::Result<(), SourceFailure>> + Send + 'a>,
>;

/// Where a download reports which source it used, and every fallback on the
/// way there. `Sync` for the same spawned-task reason as [`SourceAttempt`].
type Report<'a> = &'a (dyn Fn(&str) + Sync);

/// Walk `sources` in order and stop at the first that installs, reporting which
/// one that was and every fallback along the way through `report`.
///
/// The fallback policy lives here and nowhere else, and `attempt` is a
/// parameter so it can be exercised without a network.
async fn install_from_first_available<'a>(
    sources: &'a [ReleaseSource],
    report: Report<'_>,
    attempt: &(dyn Fn(&'a ReleaseSource) -> SourceAttempt<'a> + Sync),
) -> Result<()> {
    let mut last: Option<anyhow::Error> = None;
    for (index, source) in sources.iter().enumerate() {
        match attempt(source).await {
            Ok(()) => {
                report(&format!("downloaded from {}", source.label));
                return Ok(());
            }
            Err(SourceFailure::Fatal(err)) => return Err(err),
            Err(SourceFailure::Unavailable(err)) => {
                if let Some(next) = sources.get(index + 1) {
                    report(&format!(
                        "{} did not serve the release ({err:#}) — falling back to {}",
                        source.label, next.label
                    ));
                }
                last = Some(err);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("no release download source is configured")))
}

/// Extract the expected sha256 hex for `asset` from a `checksums.txt` body
/// (`sha256sum` format: `<hex>  <name>`, optionally `*`-prefixed in binary
/// mode). `None` when the asset has no entry. Malformed lines are skipped.
fn parse_checksums(text: &str, asset: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hex), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.strip_prefix('*').unwrap_or(name);
        if name == asset {
            return Some(hex.to_ascii_lowercase());
        }
    }
    None
}

/// Lowercase hex encoding of a byte slice (small helper; no `hex` dependency).
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// sha256 of a byte slice, lowercase hex. Shared with `crate::sync`, which
/// hashes bundle payload files the same way.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

/// The running executable, resolved through any symlinks so the rename lands
/// on the real file rather than a link.
fn current_exe_canonical() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the current executable")?;
    exe.canonicalize()
        .with_context(|| format!("canonicalizing {}", exe.display()))
}

/// Suffix of the rollback copy `wizard update` leaves behind (`wizard.bak`,
/// restored by `wizard update --rollback`).
pub(crate) const UPDATE_BACKUP_SUFFIX: &str = "bak";

/// Suffix of the rollback copy deep evolve leaves behind (`wizard.prev`).
/// Distinct from [`UPDATE_BACKUP_SUFFIX`] so an update and a deep evolve never
/// overwrite each other's way back.
pub(crate) const EVOLVE_BACKUP_SUFFIX: &str = "prev";

/// The rollback backup path for an executable (`<name>.bak`).
fn backup_path(exe: &Path) -> Result<PathBuf> {
    exe_swap::backup_path(exe, UPDATE_BACKUP_SUFFIX)
}

/// Whether `dir` is writable, probed by creating (and removing) a temp file —
/// more reliable across ownership/ACL combinations than inspecting metadata.
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".wizard-update-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Both stdin and stdout are a terminal — the only context in which it is safe
/// to escalate with `sudo` (a human is present to answer the prompt).
fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

// ---------------------------------------------------------------------------
// The one atomic executable swap (shared with deep evolve)
// ---------------------------------------------------------------------------
//
// It lives in `crate::platform::exe_swap` now: replacing a binary that may be
// executing right now is the step Windows cannot do the way Unix does (an
// executing image is locked there), so it belongs with the other decisions the
// port has to re-make. What stays here is the caller's vocabulary: which
// suffix each rollback copy carries.

/// Private staging directory for downloads and unpacked binaries,
/// `~/.wizard/update`, created 0700 and re-restricted every time.
///
/// Deliberately not the shared system temp dir: `/tmp` is world-writable, the
/// old staging names were predictable (`.wizard.update.<pid>`), and on the
/// escalation path the staged file is the argument to `sudo install`. Any
/// local user who could win that race had root install a binary of their
/// choosing over `wizard`. That is why [`crate::platform::paths::staging_dir`]
/// is strict about the permissions it cannot set, rather than warning.
fn staging_dir() -> Result<PathBuf> {
    paths::staging_dir("update")
}

/// Query the GitHub releases API for the newest tag (`tag_name`). Network and
/// rate-limit failures return `Err` so callers can degrade gracefully.
async fn fetch_latest_tag(repo: &str, timeout: Duration) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(timeout)
        .build()
        .context("building HTTP client")?;
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let body: serde_json::Value = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("querying {url}"))?
        .json()
        .await
        .context("parsing the GitHub releases API response")?;
    body.get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .context("the GitHub releases API response had no tag_name")
}

/// Tail of every refusal to install something unverified, so the message says
/// what to do next instead of only what went wrong.
const VERIFY_HINT: &str = "wizard installs only release binaries whose sha256 is published in that \
     release's checksums.txt, so this update is refused; retry once the release \
     publishes one, pick another version with `wizard update --to <tag>`, or \
     build from source (`WIZARD_BUILD_FROM_SOURCE=1` with install.sh)";

/// Turn the HTTP status of a `checksums.txt` request into either "read the
/// body" or the refusal that aborts the update.
///
/// Pure, and it distinguishes the two ways the file can be missing, because
/// the fixes differ: a 404 means the release genuinely published no checksums
/// (nothing to wait for, the release has to be re-cut), while any other
/// non-success is a fetch that failed (proxy, rate limit, outage) and is worth
/// retrying. Both refuse.
fn checksums_status_check(status: u16, url: &str) -> Result<()> {
    match status {
        200 => Ok(()),
        404 => bail!(
            "this release published no checksums.txt ({url} is 404), so its binaries cannot be \
             verified; {VERIFY_HINT}"
        ),
        other => bail!("fetching {url} failed with HTTP {other}; {VERIFY_HINT}"),
    }
}

/// Fetch a release's `checksums.txt`. Returns an error rather than an `Option`
/// on purpose: verification is mandatory, and an `Option` here is what let a
/// transient fetch failure silently downgrade the install to unverified.
///
/// The raw bytes, not a `String`: they are what the release signature covers,
/// and decoding before verifying would mean checking a signature over a body
/// that had already been transformed. The text is recovered from these exact
/// bytes once the signature has passed.
async fn fetch_checksums(client: &reqwest::Client, base: &str) -> Result<Vec<u8>> {
    let url = format!("{base}/checksums.txt");
    let response = client
        .get(&url)
        .timeout(METADATA_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("fetching {url}; {VERIFY_HINT}"))?;
    checksums_status_check(response.status().as_u16(), &url)?;
    read_capped(response, &url, VERIFY_HINT).await
}

/// The refusal when a metadata response outgrows [`MAX_METADATA_BYTES`], or
/// `Ok` while it has not. Pure, so the bound is one testable decision rather
/// than a branch inside a stream loop nothing drives in the suite.
fn metadata_cap_check(seen: u64, url: &str, hint: &str) -> Result<()> {
    if seen > MAX_METADATA_BYTES {
        bail!(
            "{url} sent more than {} KB, which no release checksums.txt or signature is; \
             refusing before anything is verified. {hint}",
            MAX_METADATA_BYTES / 1024
        );
    }
    Ok(())
}

/// Read a metadata response body under [`MAX_METADATA_BYTES`].
///
/// `Response::bytes` would buffer whatever the host decides to send. These two
/// files are fetched from a host nothing has authenticated yet — a mirror, on
/// the path this release added — and they are read into memory, so an answer
/// that never ends is an out-of-memory kill on a machine that has not yet
/// agreed to trust a byte of it. The tarball got this bound in [`download_to`];
/// the files that decide the tarball's fate had none.
async fn read_capped(response: reqwest::Response, url: &str, hint: &str) -> Result<Vec<u8>> {
    let mut body: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading {url}; {hint}"))?;
        metadata_cap_check((body.len() + chunk.len()) as u64, url, hint)?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The sha256 `asset` must match, or the refusal that skips it. Total and pure
/// so the "never install what we cannot verify" rule is one testable decision
/// rather than a branch buried in the download loop.
fn required_digest(checksums: &str, asset: &str) -> Result<String> {
    parse_checksums(checksums, asset).ok_or_else(|| {
        anyhow!(
            "{asset} is not listed in the release's checksums.txt, so its download cannot be \
             verified; {VERIFY_HINT}"
        )
    })
}

// ---------------------------------------------------------------------------
// Release signatures (minisign)
// ---------------------------------------------------------------------------
//
// `checksums.txt` is what every download is measured against, so it is the one
// file whose *authenticity* has to be established; the sha256 chain carries
// that authenticity to the tarballs. The release workflow signs it with
// minisign and publishes the detached `checksums.txt.minisig` beside it.
//
// minisign is a file format over ed25519, not a new algorithm, so verification
// reuses the `ed25519-dalek` that `crate::sync` already verifies bundles with,
// down to `verify_strict` (which rejects small-order keys and non-canonical
// signatures) and to having no bypass flag. The one primitive it adds is
// blake2b-512: since 0.10 minisign signs a prehash of the file rather than the
// file itself, and records which it did in the signature's two algorithm bytes
// (`Ed` raw, `ED` prehashed). Both are accepted, nothing else is.
//
// The public key is compiled in from a file checked into the repository root,
// so a reader can compare what their binary trusts against what the repository
// publishes, and so `install.sh` can carry the identical string inline.

/// The minisign public key releases are signed with, compiled in from the copy
/// published at the repository root. A binary trusts exactly this key: there is
/// no config key, environment variable, or flag that adds another one or skips
/// the check.
const RELEASE_PUBLIC_KEY: &str = include_str!("../wizard-release.pub");

/// What `wizard-release.pub` holds in a checkout whose release keypair has not
/// been generated yet. Detected only so the refusal names the real cause
/// instead of reporting the placeholder as corrupt base64. It is still a
/// refusal, and no update installs without a key to check the signature with.
const RELEASE_KEY_PLACEHOLDER: &str = "RELEASE-SIGNING-KEY-NOT-YET-GENERATED";

/// Tail of every refusal to install something whose signature does not check
/// out, mirroring [`VERIFY_HINT`] for the checksum half.
const SIGNATURE_HINT: &str = "wizard installs only release binaries whose checksums.txt carries a \
     valid minisign signature from the release key compiled into this binary, so this update is \
     refused; verify the release by hand (`minisign -Vm checksums.txt -P <key from \
     wizard-release.pub>`), pick another version with `wizard update --to <tag>`, or build from \
     source (`WIZARD_BUILD_FROM_SOURCE=1` with install.sh)";

/// A parsed minisign public key: the 8-byte key id every signature has to name,
/// and the ed25519 key itself.
#[derive(Debug)]
struct ReleaseKey {
    id: [u8; 8],
    key: VerifyingKey,
}

/// A key id as minisign prints it: big-endian over the little-endian bytes the
/// format stores, so an error here can be compared against `minisign -V`.
fn key_id_hex(id: &[u8; 8]) -> String {
    format!("{:016X}", u64::from_le_bytes(*id))
}

/// Decode one base64 line of a minisign file into its two algorithm bytes, its
/// 8-byte key id, and a `body_len`-byte body. `what` names the line for errors.
fn decode_minisign_line(
    line: &str,
    body_len: usize,
    what: &str,
) -> Result<([u8; 2], [u8; 8], Vec<u8>)> {
    let raw = BASE64
        .decode(line.trim())
        .with_context(|| format!("{what} is not valid base64"))?;
    if raw.len() != 10 + body_len {
        bail!("{what} is {} bytes, expected {}", raw.len(), 10 + body_len);
    }
    let algorithm: [u8; 2] = raw[..2].try_into().expect("2 bytes");
    let id: [u8; 8] = raw[2..10].try_into().expect("8 bytes");
    Ok((algorithm, id, raw[10..].to_vec()))
}

/// Parse a minisign public key file: a comment line, then
/// `base64(alg || key_id || 32-byte ed25519 key)`.
fn parse_public_key(text: &str) -> Result<ReleaseKey> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("untrusted comment:"))
        .context("the release public key file holds no key line")?;
    if line.starts_with(RELEASE_KEY_PLACEHOLDER) {
        bail!(
            "this build embeds no release signing key (wizard-release.pub is still the \
             placeholder), so it cannot verify any release; {SIGNATURE_HINT}"
        );
    }
    let (algorithm, id, body) = decode_minisign_line(line, 32, "the release public key")?;
    if &algorithm != b"Ed" {
        bail!(
            "the release public key names algorithm {:?}, not minisign's ed25519 (`Ed`)",
            String::from_utf8_lossy(&algorithm)
        );
    }
    let raw: [u8; 32] = body.as_slice().try_into().expect("32 bytes");
    let key = VerifyingKey::from_bytes(&raw)
        .map_err(|_| anyhow!("the release public key is not a valid ed25519 key"))?;
    Ok(ReleaseKey { id, key })
}

/// A parsed detached minisign signature.
struct ReleaseSignature {
    /// `Ed` (signed the file) or `ED` (signed its blake2b-512 prehash).
    algorithm: [u8; 2],
    key_id: [u8; 8],
    signature: Signature,
    /// The trusted comment, which the global signature covers along with the
    /// signature itself. Wizard reads nothing out of it; verifying it is what
    /// stops it being an unauthenticated field riding along in a signed file.
    trusted_comment: String,
    global_signature: Signature,
}

/// Parse a `.minisig` file: untrusted comment, signature, trusted comment,
/// global signature, in that order and all four required.
fn parse_signature(text: &str) -> Result<ReleaseSignature> {
    let mut lines = text.lines().map(|line| line.trim_end_matches(['\r', '\n']));
    let untrusted = lines.next().unwrap_or_default();
    if !untrusted.starts_with("untrusted comment:") {
        bail!("the release signature does not start with an untrusted comment line");
    }
    let signature_line = lines
        .next()
        .context("the release signature file has no signature line")?;
    let (algorithm, key_id, body) =
        decode_minisign_line(signature_line, 64, "the release signature")?;
    let signature = Signature::from_slice(&body).expect("64 bytes");
    let trusted_comment = lines
        .next()
        .and_then(|line| line.strip_prefix("trusted comment: "))
        .context("the release signature file has no trusted comment line")?
        .to_string();
    let global_line = lines
        .next()
        .context("the release signature file has no global signature line")?;
    let global = BASE64
        .decode(global_line.trim())
        .context("the release global signature is not valid base64")?;
    let global_signature = Signature::from_slice(&global)
        .map_err(|_| anyhow!("the release global signature is not a 64-byte ed25519 signature"))?;
    Ok(ReleaseSignature {
        algorithm,
        key_id,
        signature,
        trusted_comment,
        global_signature,
    })
}

/// blake2b-512 of a byte slice: the prehash minisign's `ED` algorithm signs.
fn blake2b512(data: &[u8]) -> [u8; 64] {
    use blake2::{Blake2b512, Digest};
    let mut hasher = Blake2b512::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Check `signature` against `data` under `key`: the key id, then the file
/// signature, then the global signature over `signature || trusted comment`.
/// Every failure is fatal to the update; none of them has a bypass.
fn verify_signature(key: &ReleaseKey, signature: &ReleaseSignature, data: &[u8]) -> Result<()> {
    if signature.key_id != key.id {
        bail!(
            "the release is signed by key {}, but this binary trusts {}; {SIGNATURE_HINT}",
            key_id_hex(&signature.key_id),
            key_id_hex(&key.id)
        );
    }
    let message = match &signature.algorithm {
        b"Ed" => data.to_vec(),
        b"ED" => blake2b512(data).to_vec(),
        other => bail!(
            "the release signature names an unknown minisign algorithm {:?}; {SIGNATURE_HINT}",
            String::from_utf8_lossy(other)
        ),
    };
    key.key
        .verify_strict(&message, &signature.signature)
        .map_err(|_| {
            anyhow!(
                "release signature verification FAILED: checksums.txt does not match its \
                 signature, so it is corrupt or tampered with; nothing was installed"
            )
        })?;
    // The trusted comment is inside the signed envelope, so a rewritten one has
    // to fail here rather than pass silently.
    let mut global = signature.signature.to_bytes().to_vec();
    global.extend_from_slice(signature.trusted_comment.as_bytes());
    key.key
        .verify_strict(&global, &signature.global_signature)
        .map_err(|_| {
            anyhow!(
                "release signature verification FAILED: the trusted comment does not match the \
                 global signature; nothing was installed"
            )
        })
}

/// The key this binary trusts, parsed from the compiled-in
/// `wizard-release.pub`.
fn release_key() -> Result<ReleaseKey> {
    parse_public_key(RELEASE_PUBLIC_KEY)
}

/// Verify `data` (the release's `checksums.txt` bytes) against the detached
/// signature text. The whole authenticity check, in one pure function.
fn verify_release_signature(data: &[u8], signature_text: &str, tag: &str) -> Result<()> {
    let key = release_key()?;
    let signature = parse_signature(signature_text)
        .with_context(|| format!("reading checksums.txt.minisig; {SIGNATURE_HINT}"))?;
    verify_signature(&key, &signature, data)?;
    binds_to_tag(&signature.trusted_comment, tag)
}

/// The trusted comment names the release it was made for; require it to be the
/// one being installed.
///
/// Without this a signature is only evidence that *some* release was signed by
/// the release key, never which. Asset names carry no version, so a host that
/// serves `<mirror>/v2.0.0/…` can answer with v1.0.0's genuine, key-signed
/// checksums.txt, signature and tarball: the key id matches, `verify_strict`
/// passes, the global signature over the trusted comment verifies, the digest
/// matches its own checksums file, and the binary starts. The user is moved to
/// any earlier release the attacker prefers — one with a known hole, say — and
/// because the version then looks old, `auto = true` re-does it on every check.
///
/// That is a signed downgrade, and it makes SECURITY.md's "a mirror cannot hold
/// you on an old release" false. The release workflow has always written the
/// tag into the trusted comment, and `verify_signature` has always verified the
/// global signature that covers it. Nothing read it.
///
/// Matched loosely on purpose: the requirement is that the tag appears as a
/// whole word, so the comment's wording can change without breaking
/// verification for binaries already in the field. What it cannot do is match a
/// *different* release's comment.
fn binds_to_tag(trusted_comment: &str, tag: &str) -> Result<()> {
    let wanted = tag.trim();
    if wanted.is_empty() {
        bail!("cannot verify a release with no tag to check the signature against");
    }
    let names_tag = trusted_comment
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_'))
        .any(|word| word == wanted);
    if !names_tag {
        bail!(
            "this signature is for a different release: it was made for \
             {trusted_comment:?}, not for {wanted}. A host that serves one release's \
             genuinely signed files in answer to a request for another is trying to \
             move you off the version you asked for"
        );
    }
    Ok(())
}

/// Turn the HTTP status of a `checksums.txt.minisig` request into either "read
/// the body" or the refusal that aborts the update. The 404 case is called out
/// separately for the same reason as in [`checksums_status_check`]: a release
/// that published no signature has to be re-cut, a fetch that failed is worth
/// retrying, and both refuse.
fn signature_status_check(status: u16, url: &str) -> Result<()> {
    match status {
        200 => Ok(()),
        404 => bail!(
            "this release published no checksums.txt.minisig ({url} is 404), so its checksums \
             cannot be authenticated; {SIGNATURE_HINT}"
        ),
        other => bail!("fetching {url} failed with HTTP {other}; {SIGNATURE_HINT}"),
    }
}

/// Fetch a release's detached `checksums.txt.minisig`.
async fn fetch_signature(client: &reqwest::Client, base: &str) -> Result<String> {
    let url = format!("{base}/checksums.txt.minisig");
    let response = client
        .get(&url)
        .timeout(METADATA_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("fetching {url}; {SIGNATURE_HINT}"))?;
    signature_status_check(response.status().as_u16(), &url)?;
    let body = read_capped(response, &url, SIGNATURE_HINT).await?;
    // Lossy rather than strict: a signature that is not UTF-8 is not a
    // signature, and `parse_signature` refuses it with a message about the
    // format the user can act on, where a decode error here would not.
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Stream a URL to `dest`.
async fn download_to(client: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let response = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("downloading {url}"))?;
    let mut out =
        std::fs::File::create(dest).with_context(|| format!("writing {}", dest.display()))?;
    let mut stream = response.bytes_stream();
    // Bounded, because everything that decides whether these bytes are ours
    // happens *after* they have all arrived. Until then a download host — which
    // may be a mirror the user was talked into configuring — can send as much
    // as it likes, and this wrote every byte to disk and then read the whole
    // file back to hash it. An answer that never ends fills the disk and the
    // memory of a machine that has not yet agreed to trust a single byte of it.
    //
    // The cap is far above any real asset (the largest today is about 10 MB
    // and the native GUI build is not much more) and far below anything that
    // hurts. Exceeding it is a refusal naming the host, not a truncation: a
    // partial tarball would fail its digest anyway, and saying "too large" is
    // the more useful failure.
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading {url}"))?;
        written = written.saturating_add(chunk.len() as u64);
        if written > MAX_DOWNLOAD_BYTES {
            let _ = std::fs::remove_file(dest);
            bail!(
                "{url} sent more than {} MB, which no Wizard release is; refusing before                  anything is verified",
                MAX_DOWNLOAD_BYTES / (1024 * 1024)
            );
        }
        std::io::Write::write_all(&mut out, &chunk)
            .with_context(|| format!("writing {}", dest.display()))?;
    }
    std::io::Write::flush(&mut out).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// Extract the single `wizard` file from a gzip+tar `tarball` to `dest`.
fn extract_wizard(tarball: &Path, dest: &Path) -> Result<()> {
    let file =
        std::fs::File::open(tarball).with_context(|| format!("opening {}", tarball.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().context("reading the release tarball")? {
        let mut entry = entry.context("reading a release tarball entry")?;
        let path = entry
            .path()
            .context("a release tarball entry had a bad path")?;
        if path.file_name().and_then(|n| n.to_str()) == Some("wizard") {
            let mut out = std::fs::File::create(dest)
                .with_context(|| format!("writing {}", dest.display()))?;
            std::io::copy(&mut entry, &mut out)
                .with_context(|| format!("unpacking wizard to {}", dest.display()))?;
            return Ok(());
        }
    }
    bail!("the release tarball contained no `wizard` file");
}

/// The `sudo` argv sequence that installs `staged` over `dest_exe`: back the
/// current binary up to `backup` first, then install.
///
/// Split out because the sudo path is the one this program cannot exercise in
/// a test, and it is the path most people take: `/usr/local/bin` is the
/// installer's default on every FHS distro, so `dir_is_writable` says no and
/// the escalation below is what actually runs an update. It used to run
/// `sudo install` alone — no backup — while [`install_over`]'s own doc
/// promised `<name>.bak` and [`rollback_binary`] had a matching sudo branch
/// waiting for it. The result was that `wizard update` succeeded and
/// `wizard update --rollback` then said "no backup … nothing to roll back
/// to", for everyone who was not installed somewhere writable.
///
/// The backup is skipped when there is nothing at `dest_exe` yet, because
/// `install` would fail on the missing source and take the update with it.
fn sudo_install_plan(staged: &Path, dest_exe: &Path, backup: &Path) -> Vec<Vec<OsString>> {
    let mut plan = Vec::new();
    if dest_exe.exists() {
        plan.push(vec![
            OsString::from("install"),
            OsString::from("-m755"),
            dest_exe.into(),
            backup.into(),
        ]);
    }
    plan.push(vec![
        OsString::from("install"),
        OsString::from("-m755"),
        staged.into(),
        dest_exe.into(),
    ]);
    plan
}

/// Move `staged` into place at `dest_exe`, backing the current binary up to
/// `<name>.bak` first. When `writable`, the swap goes through the shared
/// [`exe_swap::install_executable`] (copy, fsync, rename); otherwise (a
/// protected dir like `/usr/local/bin`) escalate via `sudo` when a terminal is
/// present — for the backup as well as the install, see [`sudo_install_plan`]
/// — else print the manual command and error. `staged` is cleaned up on every
/// path except the last one, where it is intentionally left for the printed
/// command.
fn install_over(staged: &Path, dest_exe: &Path, writable: bool) -> Result<()> {
    if writable {
        // `staged` lives in the private staging dir, so the helper copies it
        // next to `dest_exe` before the rename; drop the original either way.
        let installed = exe_swap::install_executable(staged, dest_exe, UPDATE_BACKUP_SUFFIX);
        let _ = std::fs::remove_file(staged);
        return installed.map(|_| ());
    }
    if interactive() {
        let backup = backup_path(dest_exe)?;
        for argv in sudo_install_plan(staged, dest_exe, &backup) {
            let status = std::process::Command::new("sudo")
                .args(&argv)
                .status()
                .with_context(|| format!("running sudo install for {}", dest_exe.display()))?;
            if !status.success() {
                let _ = std::fs::remove_file(staged);
                bail!("sudo install to {} failed", dest_exe.display());
            }
        }
        let _ = std::fs::remove_file(staged);
        Ok(())
    } else {
        // Leave the staged binary in place so the printed command works.
        bail!(
            "cannot write {} and no terminal to escalate — install manually:\n  \
             sudo install -m755 {} {}",
            dest_exe.display(),
            staged.display(),
            dest_exe.display()
        );
    }
}

/// Run `binary --version` as a sanity check: does it actually execute on this
/// system? Catches a libc/dynamic-loader mismatch — e.g. a prebuilt glibc or
/// musl release binary on NixOS, or an old glibc host — before it replaces a
/// working install with a dud. Mirrors the same guard in `install.sh`.
fn binary_runs(binary: &Path) -> bool {
    std::process::Command::new(binary)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Locate `name` on `PATH`.
fn find_command(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// A working `cargo`: `PATH` first, then `~/.cargo/bin` for a rustup install
/// that is not yet on this process's `PATH`.
fn find_cargo() -> Option<PathBuf> {
    if let Some(cargo) = find_command("cargo") {
        return Some(cargo);
    }
    let candidate = dirs::home_dir()?.join(".cargo").join("bin").join("cargo");
    candidate.is_file().then_some(candidate)
}

/// Why a failed download may be rebuilt from source rather than refused.
///
/// Signature and digest failures are never eligible: those are "stop", not
/// "try another path". A placeholder key, a host with no runnable prebuilt,
/// and a platform we publish nothing for are the only three, and they are
/// the same three `install.sh` already builds from source for.
fn source_build_reason(err: &anyhow::Error) -> Option<&'static str> {
    let text = format!("{err:#}");
    // Eligible first: a placeholder-key refusal is wrapped as "not the
    // release key's", and that outer wording is also used for a real
    // signature failure. The inner phrase is what distinguishes them.
    if text.contains("embeds no release signing key") {
        return Some("this build embeds no release signing key");
    }
    if text.contains("no prebuilt wizard binary runs") {
        return Some("no prebuilt binary runs on this system");
    }
    if text.contains("no matching prebuilt")
        || text.contains("Termux has no matching")
        || text.contains("no prebuilt wizard release")
    {
        return Some("this platform has no prebuilt release");
    }
    None
}

/// Clone `tag` and `cargo build --release --locked` it, then swap the result
/// in. Trust is the git tag, the same as `WIZARD_BUILD_FROM_SOURCE=1`.
fn build_from_source(repo: &str, tag: &str, dest_exe: &Path, report: Report<'_>) -> Result<()> {
    let git = find_command("git")
        .context("git is required to build from source but was not found on PATH")?;
    let cargo = find_cargo().context(
        "cargo is required to build from source but was not found; install a Rust toolchain \
         (https://rustup.rs) and retry, or use the Nix flake",
    )?;

    let dest_dir = dest_exe
        .parent()
        .context("the current executable has no parent directory")?;
    let writable = dir_is_writable(dest_dir);
    let scratch = staging_dir()?;
    let src_dir = scratch.join(format!("src-{tag}"));
    let _ = std::fs::remove_dir_all(&src_dir);

    let url = format!("https://github.com/{repo}");
    report(&format!("cloning {url}@{tag}"));
    let clone = std::process::Command::new(&git)
        .args(["clone", "--depth", "1", "--branch", tag])
        .arg(&url)
        .arg(&src_dir)
        .status()
        .context("running git clone")?;
    if !clone.success() {
        let _ = std::fs::remove_dir_all(&src_dir);
        bail!("git clone of {url} at {tag} failed");
    }
    if !src_dir.join("Cargo.toml").is_file() {
        let _ = std::fs::remove_dir_all(&src_dir);
        bail!("cloned {url}@{tag} but there is no Cargo.toml");
    }

    report("running cargo build --release --locked (this can take a few minutes)");
    let mut cmd = std::process::Command::new(&cargo);
    cmd.args(["build", "--release", "--locked"])
        .current_dir(&src_dir);
    if cfg!(feature = "native") {
        cmd.args(["--features", "native"]);
    }
    let status = cmd
        .status()
        .context("running cargo build --release --locked")?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&src_dir);
        bail!("cargo build --release --locked failed for {tag}");
    }

    let built = src_dir.join("target").join("release").join("wizard");
    if !built.is_file() {
        let _ = std::fs::remove_dir_all(&src_dir);
        bail!("build succeeded but {} is missing", built.display());
    }
    if !binary_runs(&built) {
        let _ = std::fs::remove_dir_all(&src_dir);
        bail!("the binary built from {tag} does not run on this system");
    }

    let result = install_over(&built, dest_exe, writable);
    let _ = std::fs::remove_dir_all(&src_dir);
    result
}

/// Install `tag` at `dest_exe`: download when this binary can verify one,
/// otherwise (or when no published asset runs here) build the tag from
/// source. `allow_source` is true for `wizard update` and false for the
/// background auto-update, which must not start a multi-minute compile.
async fn apply_update(
    repo: &str,
    tag: &str,
    dest_exe: &Path,
    report: Report<'_>,
    allow_source: bool,
) -> Result<()> {
    if release_key().is_err() {
        if !allow_source {
            release_key()?;
        }
        report(&format!(
            "this build embeds no release signing key, so it cannot verify a download; \
             building {tag} from source"
        ));
        return build_from_source(repo, tag, dest_exe, report);
    }
    match download_and_install(repo, tag, dest_exe, report).await {
        Ok(()) => Ok(()),
        Err(err) => {
            if allow_source && let Some(why) = source_build_reason(&err) {
                report(&format!("{why}; building {tag} from source"));
                build_from_source(repo, tag, dest_exe, report)
            } else {
                Err(err)
            }
        }
    }
}

/// Download the release for `tag` and swap it in at `dest_exe`, trying each
/// host in `sources` until one of them installs.
///
/// `report` is where the "downloaded from …" and fallback lines go. It is a
/// parameter because the passive startup check must stay silent: a `println!`
/// into the alternate-screen, raw-mode TUI is invisible at best and corrupts
/// the display at worst.
async fn download_and_install(
    repo: &str,
    tag: &str,
    dest_exe: &Path,
    report: Report<'_>,
) -> Result<()> {
    let candidates = asset_candidates()?;
    let dest_dir = dest_exe
        .parent()
        .context("the current executable has no parent directory")?;

    // Everything is staged in `~/.wizard/update` (0700), never the shared
    // system temp dir: on the escalation path below the staged file is the
    // argument to `sudo install`. Staging off `dest_dir`'s filesystem costs one
    // extra copy inside `exe_swap::install_executable`, which does its own scratch-copy
    // beside `dest_exe` so the final swap is still an atomic rename.
    let writable = dir_is_writable(dest_dir);
    let scratch = staging_dir()?;

    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(20))
        .build()
        .context("building HTTP client")?;

    let sources = release_sources(Some(mirror_setting().as_str()), repo, tag);
    // Bind the borrows outside the closure so the per-source future captures
    // copies of the references rather than moving the values into the first
    // attempt.
    let (client, candidates, scratch) = (&client, &candidates, scratch.as_path());
    install_from_first_available(&sources, report, &|source| {
        Box::pin(install_from_source(
            client, source, tag, candidates, scratch, dest_exe, writable,
        ))
    })
    .await
}

/// A source's `checksums.txt` as verified text, or the refusal that stops the
/// update.
///
/// The one place the release signature is checked on the download path, and it
/// takes the host only to name it in the message: there is no parameter, flag
/// or branch by which a mirror could be verified less strictly than GitHub,
/// because there is only one implementation and it never learns who answered.
/// Every failure is [`SourceFailure::Fatal`] — see that type for why a host
/// serving bytes the release key did not sign is not a reason to quietly ask
/// the next host instead.
fn verified_checksums(
    raw: &[u8],
    signature: &str,
    source: &ReleaseSource,
    tag: &str,
) -> std::result::Result<String, SourceFailure> {
    verify_release_signature(raw, signature, tag)
        .with_context(|| {
            format!(
                "{} served a {tag} checksums.txt that is not the release key's",
                source.label
            )
        })
        .map_err(SourceFailure::Fatal)?;
    // Only now, once these bytes are known to be the release key's own, are
    // they read as text.
    String::from_utf8(raw.to_vec())
        .with_context(|| format!("the {tag} release's checksums.txt is not valid UTF-8"))
        .map_err(SourceFailure::Fatal)
}

/// One host's attempt at the whole install: fetch and verify the release's
/// digests, then walk the asset candidates and install the first that
/// downloads, verifies, unpacks, and — critically — actually runs on this
/// machine.
///
/// Verification is mandatory and identical for every source: the release's
/// `checksums.txt` must be fetchable (see [`fetch_checksums`]), its signature
/// must verify under the compiled-in release key, the asset must be listed in
/// it, and the digest must match, or nothing is installed. The live binary is
/// only ever touched once a candidate passes every check, so a platform with no
/// runnable prebuilt (e.g. NixOS, which needs the Nix flake) fails cleanly with
/// the current binary left in place.
async fn install_from_source(
    client: &reqwest::Client,
    source: &ReleaseSource,
    tag: &str,
    candidates: &[String],
    scratch: &Path,
    dest_exe: &Path,
    writable: bool,
) -> std::result::Result<(), SourceFailure> {
    let base = &source.base;
    // Both fetched before anything is downloaded: without the digests there is
    // nothing to check a download against, and without the signature the
    // digests are only whatever the download host served. An unverifiable
    // binary is not installed, and neither is one whose digests are unsigned.
    //
    // A host that cannot produce them is unavailable and the next one is
    // tried; a host that produces digests the release key did not sign is
    // fatal, because that is not a host having a bad day.
    let checksums = fetch_checksums(client, base)
        .await
        .with_context(|| format!("verifying the {tag} release"))
        .map_err(SourceFailure::Unavailable)?;
    let signature = fetch_signature(client, base)
        .await
        .with_context(|| format!("verifying the {tag} release"))
        .map_err(SourceFailure::Unavailable)?;
    let checksums = verified_checksums(&checksums, &signature, source, tag)?;

    let pid = std::process::id();
    let mut unrunnable: Vec<String> = Vec::new();
    let mut last_err = anyhow!("no release asset for {tag} could be downloaded from {base}");

    for asset in candidates {
        // 1. Look the digest up first. `checksums.txt` lists every asset the
        //    release published, so one that is absent from it was never
        //    published (some platforms ship only musl or only gnu): move on to
        //    the next candidate instead of downloading something unverifiable.
        let expected = match required_digest(&checksums, asset) {
            Ok(expected) => expected,
            Err(err) => {
                last_err = err;
                continue;
            }
        };

        // 2. Download, then verify. A mismatch means corruption or tampering —
        //    abort the whole update rather than reaching for a different asset
        //    or a different host: the digest came from a signed file, so the
        //    bytes that do not match it are the problem.
        let tarball = scratch.join(format!(".{asset}.{pid}.part"));
        if let Err(err) = download_to(client, &format!("{base}/{asset}"), &tarball).await {
            let _ = std::fs::remove_file(&tarball);
            last_err = err;
            continue;
        }
        // Scoped so the whole tarball is not held in memory while unpacking.
        let actual = {
            let data = std::fs::read(&tarball)
                .with_context(|| format!("reading {}", tarball.display()))
                .map_err(SourceFailure::Fatal)?;
            sha256_hex(&data)
        };
        if actual != expected {
            let _ = std::fs::remove_file(&tarball);
            return Err(SourceFailure::Fatal(anyhow!(
                "checksum mismatch for {asset} from {base} — expected {expected}, got {actual}; \
                 aborting update"
            )));
        }

        // 3. Unpack + chmod.
        let staged = scratch.join(format!(".wizard.update.{pid}"));
        let _ = std::fs::remove_file(&staged);
        let extracted =
            extract_wizard(&tarball, &staged).and_then(|()| exe_swap::set_executable(&staged));
        let _ = std::fs::remove_file(&tarball);
        if let Err(err) = extracted {
            let _ = std::fs::remove_file(&staged);
            last_err = err;
            continue;
        }

        // 4. Sanity check — the binary must run here before we replace a working
        //    one with it.
        if !binary_runs(&staged) {
            let _ = std::fs::remove_file(&staged);
            unrunnable.push(asset.clone());
            last_err = anyhow!("the binary from {asset} does not run on this system");
            continue;
        }

        // 5. Swap it in (backs the current binary up to `<name>.bak` first).
        //    A swap that fails is a local problem — a read-only directory, no
        //    terminal to escalate on — and no other host fixes it.
        return install_over(&staged, dest_exe, writable).map_err(SourceFailure::Fatal);
    }

    if unrunnable.is_empty() {
        Err(SourceFailure::Unavailable(last_err))
    } else {
        Err(SourceFailure::Unavailable(last_err.context(format!(
            "no prebuilt wizard binary runs on this system (tried {}); the current binary \
             is unchanged. On NixOS, install via the Nix flake (see the README) rather than \
             `wizard update`. On Termux, rebuild from source \
             (`WIZARD_BUILD_FROM_SOURCE=1` with install.sh, or `cargo build --release` \
             in ~/.wizard/src).",
            unrunnable.join(", ")
        ))))
    }
}

/// Restore the pre-update binary from `<name>.bak`.
fn rollback_binary(dest_exe: &Path) -> Result<i32> {
    let backup = backup_path(dest_exe)?;
    if !backup.exists() {
        bail!(
            "no backup at {} — nothing to roll back to",
            backup.display()
        );
    }
    let dest_dir = dest_exe
        .parent()
        .context("the current executable has no parent directory")?;

    if dir_is_writable(dest_dir) {
        std::fs::rename(&backup, dest_exe).with_context(|| {
            format!("restoring {} from {}", dest_exe.display(), backup.display())
        })?;
    } else if interactive() {
        let status = std::process::Command::new("sudo")
            .arg("install")
            .arg("-m755")
            .arg(&backup)
            .arg(dest_exe)
            .status()
            .with_context(|| format!("running sudo install for {}", dest_exe.display()))?;
        if !status.success() {
            bail!("sudo install to {} failed", dest_exe.display());
        }
        // Through sudo as well: the backup was written by `sudo install`, in a
        // directory this user cannot write, so an ordinary `remove_file` here
        // fails silently and leaves a copy that a second `--rollback` would
        // restore all over again.
        let _ = std::process::Command::new("sudo")
            .arg("rm")
            .arg("-f")
            .arg(&backup)
            .status();
    } else {
        bail!(
            "cannot write {} and no terminal to escalate — restore manually:\n  \
             sudo install -m755 {} {}",
            dest_exe.display(),
            backup.display(),
            dest_exe.display()
        );
    }
    println!("rolled back to the previous binary — restart wizard to use it.");
    Ok(0)
}

/// The `wizard update` command handler. Returns the process exit code.
pub async fn run(check: bool, to: Option<String>, force: bool, rollback: bool) -> Result<i32> {
    let dest_exe = current_exe_canonical()?;

    if rollback {
        return rollback_binary(&dest_exe);
    }

    let repo = DEFAULT_REPO;
    let current = current_version();

    let tag = match to {
        Some(tag) => normalize_tag(&tag)?,
        None => fetch_latest_tag(repo, COMMAND_TIMEOUT)
            .await
            .context("could not determine the latest release from GitHub")?,
    };
    let newer = is_newer(&tag, current);

    if check {
        println!("current: v{current}");
        println!("latest:  {tag}");
        if newer {
            println!("update available — run `wizard update`");
        } else {
            println!("up to date");
        }
        return Ok(0);
    }

    if !newer && !force {
        println!("already up to date (v{current})");
        return Ok(0);
    }

    println!("updating to {tag}…");
    apply_update(repo, &tag, &dest_exe, &|line| println!("{line}"), true)
        .await
        .with_context(|| format!("updating to {tag}"))?;
    println!("updated v{current} → {tag} — restart wizard to use it.");
    Ok(0)
}

// ---------------------------------------------------------------------------
// Passive startup check
// ---------------------------------------------------------------------------

/// Cache under `~/.wizard/update-check.json` that throttles the startup check
/// to `interval_hours`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct UpdateCache {
    last_check_unix: u64,
    latest_tag: String,
}

fn cache_path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("update-check.json"))
}

fn read_cache() -> Option<UpdateCache> {
    let raw = std::fs::read_to_string(cache_path().ok()?).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(cache: &UpdateCache) {
    if let Ok(path) = cache_path()
        && let Ok(json) = serde_json::to_string(cache)
    {
        let _ = std::fs::write(path, json);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fire-and-forget passive update check, spawned so it never delays the TUI.
/// Governed entirely by `[update]` config; swallows every error. When `auto`
/// is set and the binary is writable without escalation it installs in the
/// background (taking effect on the next launch); otherwise it prints a single
/// notice line when a newer release exists.
pub async fn maybe_check_on_startup(cfg: &UpdateConfig) {
    if !cfg.notify && !cfg.auto {
        return;
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let _ = check_and_maybe_apply(cfg).await;
    });
}

async fn check_and_maybe_apply(cfg: UpdateConfig) -> Result<()> {
    let interval_secs = cfg.interval_hours.saturating_mul(3600);
    let now = now_unix();
    let cached = read_cache();

    let due = match &cached {
        Some(c) => now.saturating_sub(c.last_check_unix) >= interval_secs,
        None => true,
    };

    let latest = if due {
        match fetch_latest_tag(&cfg.repo, CHECK_TIMEOUT).await {
            Ok(tag) => {
                write_cache(&UpdateCache {
                    last_check_unix: now,
                    latest_tag: tag.clone(),
                });
                tag
            }
            // Network / rate-limit hiccup: stay silent, try again next cadence.
            Err(_) => return Ok(()),
        }
    } else {
        match cached.and_then(|c| (!c.latest_tag.is_empty()).then_some(c.latest_tag)) {
            Some(tag) => tag,
            None => return Ok(()),
        }
    };

    let current = current_version();
    if !is_newer(&latest, current) {
        return Ok(());
    }

    // The `notify` line is surfaced synchronously from the refreshed cache by
    // `print_startup_notice`, *before* the TUI takes the screen — never from
    // this task. A `println!` into the alternate-screen, raw-mode TUI would be
    // invisible or corrupt the display, so the only action left here is the
    // opt-in auto-apply.
    if cfg.auto {
        // A background task must never invoke sudo, so only auto-apply when the
        // binary is writable without escalation. The swapped-in binary takes
        // effect on the next launch; this is intentionally silent for the same
        // alternate-screen reason.
        if let Ok(exe) = current_exe_canonical()
            && let Some(dir) = exe.parent()
            && dir_is_writable(dir)
        {
            // Silent on screen — this task runs while the TUI owns it — but
            // not silent altogether.
            //
            // The result used to be dropped outright, which threw away the one
            // signal this whole path exists to produce. `SourceFailure::Fatal`
            // means a host served bytes the release key did not sign, and it
            // deliberately suppresses the GitHub fallback so a compromised
            // mirror cannot be quietly routed around. Discarded, that becomes:
            // auto-update stops working, permanently, and says nothing. The
            // interactive path prints it; the automatic one owed at least a log
            // line, because it is the path nobody is watching.
            if let Err(err) = apply_update(&cfg.repo, &latest, &exe, &|_| {}, false).await {
                tracing::warn!("automatic update to {latest} did not install: {err:#}");
            }
        }
    }
    Ok(())
}

/// The passive "update available" line for a cached `latest` tag, or `None`
/// when it is empty or not newer than `current`. Pure, so it is unit-testable.
fn notice_line(latest: &str, current: &str) -> Option<String> {
    if !latest.is_empty() && is_newer(latest, current) {
        Some(format!(
            "wizard {latest} available (you have v{current}) — run `wizard update`"
        ))
    } else {
        None
    }
}

/// Print the passive notice synchronously and from the cache only (no network),
/// so it lands cleanly on stdout *before* the TUI enters the alternate screen.
/// The background [`maybe_check_on_startup`] task refreshes that cache, so a
/// freshly published release is announced on the next launch. Gated on a real
/// terminal and on `[update].notify`.
pub fn print_startup_notice(cfg: &UpdateConfig) {
    if !cfg.notify || !std::io::stdout().is_terminal() {
        return;
    }
    if let Some(cache) = read_cache()
        && let Some(line) = notice_line(&cache.latest_tag, current_version())
    {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_semver_and_strips_v() {
        assert!(is_newer("v0.5.1", "0.5.0"));
        assert!(is_newer("0.6.0", "0.5.9"));
        assert!(is_newer("v1.0.0", "v0.9.9"));
        assert!(!is_newer("v0.5.0", "0.5.0"));
        assert!(!is_newer("v0.4.9", "0.5.0"));
        // Unparseable versions degrade to "no update".
        assert!(!is_newer("latest", "0.5.0"));
        assert!(!is_newer("v0.5.1", "not-a-version"));
    }

    #[test]
    fn display_version_drops_a_trailing_zero_patch_only() {
        assert_eq!(short_version("0.7.0"), "0.7");
        assert_eq!(short_version("0.7.1"), "0.7.1");
        assert_eq!(short_version("0.10.0"), "0.10");
        // The compiled version stays a full, parseable semver for comparison.
        assert!(semver::Version::parse(current_version()).is_ok());
        // The display never adds a component the real version lacks.
        assert!(current_version().starts_with(display_version()));
    }

    #[test]
    fn a_placeholder_or_unrunnable_prebuilt_may_build_from_source() {
        // The three cases install.sh already builds from source for: a binary
        // that cannot verify anything, a host no published asset runs on, and
        // a platform we ship nothing for.
        assert_eq!(
            source_build_reason(&anyhow!(
                "this build embeds no release signing key (wizard-release.pub is still the placeholder)"
            )),
            Some("this build embeds no release signing key")
        );
        assert_eq!(
            source_build_reason(&anyhow!(
                "no prebuilt wizard binary runs on this system (tried wizard-x86_64-unknown-linux-musl.tar.gz)"
            )),
            Some("no prebuilt binary runs on this system")
        );
        assert_eq!(
            source_build_reason(&anyhow!(
                "no prebuilt wizard release for this platform (linux/x86_64)"
            )),
            Some("this platform has no prebuilt release")
        );
        assert_eq!(
            source_build_reason(&anyhow!(
                "Termux has no matching prebuilt release asset (Android/Bionic)."
            )),
            Some("this platform has no prebuilt release")
        );
    }

    #[test]
    fn a_bad_signature_or_digest_is_never_a_reason_to_compile() {
        // These are "stop", not "try another path". Building from source
        // around a failed check would make the check optional. The
        // "not the release key's" wrapper alone is not enough: a
        // placeholder-key refusal is wrapped the same way, and that
        // one *is* eligible (asserted above).
        for text in [
            "release signature verification FAILED: checksums.txt does not match its signature",
            "checksum mismatch for wizard.tar.gz from https://example — aborting update",
            "signed by key AABBCCDD, but this binary trusts EEFF0011",
            "GitHub Releases served a v2.0.1 checksums.txt that is not the release key's: \
             release signature verification FAILED",
            "the trusted comment does not match the global signature",
            "this signature is for a different release: it was made for \"wizard v1.0.0\"",
        ] {
            assert!(
                source_build_reason(&anyhow!("{text}")).is_none(),
                "{text} must stay fatal"
            );
        }
    }

    #[test]
    fn a_placeholder_refusal_wrapped_as_not_the_release_key_still_builds() {
        // This is the sentence `wizard update` prints today: verified_checksums
        // wraps release_key()'s placeholder refusal. The outer wording must
        // not hide the inner cause.
        assert_eq!(
            source_build_reason(&anyhow!(
                "updating to v2.0.1: GitHub Releases served a v2.0.1 checksums.txt that \
                 is not the release key's: this build embeds no release signing key \
                 (wizard-release.pub is still the placeholder)"
            )),
            Some("this build embeds no release signing key")
        );
    }

    #[test]
    fn normalize_tag_adds_leading_v() {
        assert_eq!(normalize_tag("0.5.0").unwrap(), "v0.5.0");
        assert_eq!(normalize_tag("v0.5.0").unwrap(), "v0.5.0");
        assert_eq!(normalize_tag("  0.5.0  ").unwrap(), "v0.5.0");

        // The tag is interpolated into the URLs this fetches from, so a tag
        // that is not one is refused rather than turned into a request for
        // somewhere else.
        for bad in [
            "../../etc/passwd",
            "v2.0.0/../../other",
            "v2.0.0?x=1",
            "v 2.0.0",
            "",
            "v",
            "https://elsewhere.example.com/",
        ] {
            assert!(
                normalize_tag(bad).is_err(),
                "{bad:?} must not be accepted as a release tag"
            );
        }
        // Real-world tags keep working.
        for good in ["v2.0.0", "2.0.0-rc.1", "v1.8.0+build.7", "v2_0_0"] {
            assert!(normalize_tag(good).is_ok(), "{good:?} is a usable tag");
        }
    }

    #[test]
    fn normalize_arch_maps_known_and_rejects_unknown() {
        assert_eq!(normalize_arch("x86_64"), Some("x86_64"));
        assert_eq!(normalize_arch("amd64"), Some("x86_64"));
        assert_eq!(normalize_arch("aarch64"), Some("aarch64"));
        assert_eq!(normalize_arch("arm64"), Some("aarch64"));
        assert_eq!(normalize_arch("riscv64"), None);
    }

    #[test]
    fn asset_candidates_macos_is_single_darwin_build() {
        assert_eq!(
            asset_candidates_for("macos", "aarch64", false, false),
            vec!["wizard-aarch64-apple-darwin.tar.gz".to_string()]
        );
        assert_eq!(
            asset_candidates_for("macos", "x86_64", true, false),
            vec!["wizard-x86_64-apple-darwin.tar.gz".to_string()]
        );
    }

    #[test]
    fn asset_candidates_nixos_prefers_musl_then_gnu() {
        assert_eq!(
            asset_candidates_for("linux", "x86_64", true, false),
            vec![
                "wizard-x86_64-unknown-linux-musl.tar.gz".to_string(),
                "wizard-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            ]
        );
    }

    #[test]
    fn asset_candidates_plain_linux_prefers_gnu_then_musl() {
        assert_eq!(
            asset_candidates_for("linux", "aarch64", false, false),
            vec![
                "wizard-aarch64-unknown-linux-gnu.tar.gz".to_string(),
                "wizard-aarch64-unknown-linux-musl.tar.gz".to_string(),
            ]
        );
    }

    #[test]
    fn asset_candidates_termux_is_empty() {
        // No Android/Bionic release asset: update must not try gnu/musl.
        assert!(asset_candidates_for("linux", "aarch64", false, true).is_empty());
        assert!(asset_candidates_for("linux", "x86_64", true, true).is_empty());
    }

    #[test]
    fn native_assets_keep_the_native_build_a_native_build() {
        // A `--features native` binary updates to a native asset, never to the
        // plain one: it is the binary that can open the window.
        assert_eq!(
            native_assets(asset_candidates_for("linux", "x86_64", false, false)),
            vec!["wizard-native-x86_64-unknown-linux-gnu.tar.gz".to_string()]
        );
        assert_eq!(
            native_assets(asset_candidates_for("macos", "aarch64", false, false)),
            vec!["wizard-native-aarch64-apple-darwin.tar.gz".to_string()]
        );
        // musl is dropped rather than rewritten — we publish no static native
        // build, so on NixOS this leaves the gnu one and nothing else.
        assert_eq!(
            native_assets(asset_candidates_for("linux", "x86_64", true, false)),
            vec!["wizard-native-x86_64-unknown-linux-gnu.tar.gz".to_string()]
        );
        // Termux has nothing to rewrite either.
        assert!(native_assets(asset_candidates_for("linux", "aarch64", false, true)).is_empty());
    }

    #[test]
    fn parse_checksums_finds_the_asset() {
        let text = "\
aaaa1111  wizard-x86_64-unknown-linux-gnu.tar.gz
bbbb2222  wizard-x86_64-unknown-linux-musl.tar.gz
cccc3333  wizard-aarch64-apple-darwin.tar.gz
";
        assert_eq!(
            parse_checksums(text, "wizard-x86_64-unknown-linux-musl.tar.gz"),
            Some("bbbb2222".to_string())
        );
        assert_eq!(
            parse_checksums(text, "wizard-aarch64-apple-darwin.tar.gz"),
            Some("cccc3333".to_string())
        );
        assert_eq!(parse_checksums(text, "wizard-missing.tar.gz"), None);
    }

    #[test]
    fn parse_checksums_handles_binary_star_prefix_and_junk_lines() {
        // A blank line, a one-field junk line, then a binary-mode (`*`) entry.
        let text =
            "\n# a comment line with only one field\nDEADBEEF *wizard-x86_64-apple-darwin.tar.gz\n";
        assert_eq!(
            parse_checksums(text, "wizard-x86_64-apple-darwin.tar.gz"),
            Some("deadbeef".to_string())
        );
    }

    #[test]
    fn notice_line_only_when_strictly_newer() {
        assert_eq!(
            notice_line("v0.6.0", "0.5.0"),
            Some("wizard v0.6.0 available (you have v0.5.0) — run `wizard update`".to_string())
        );
        // Same version, older "latest", and an empty cache all stay quiet.
        assert_eq!(notice_line("v0.5.0", "0.5.0"), None);
        assert_eq!(notice_line("v0.4.0", "0.5.0"), None);
        assert_eq!(notice_line("", "0.5.0"), None);
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // sha256("") — the empty-input digest.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // -- the download mirror -------------------------------------------------

    #[test]
    fn the_mirror_is_off_by_default_and_off_is_spellable() {
        // The shipped default is no mirror at all: `dl.<domain>` does not
        // exist yet, and a default that points at a host which does not answer
        // makes every install pay a failed request to gain nothing. This
        // assertion is what makes turning it on a deliberate edit.
        assert_eq!(mirror_root(DEFAULT_MIRROR), None);

        for off in ["", "   ", "0", "off", "OFF", "none", "None", "false"] {
            assert_eq!(mirror_root(off), None, "{off:?} must mean no mirror");
        }
        // A bare host gets https; an explicit scheme is kept; trailing slashes
        // never become a double slash in an asset URL.
        assert_eq!(
            mirror_root("dl.example.com").as_deref(),
            Some("https://dl.example.com")
        );
        assert_eq!(
            mirror_root("https://dl.example.com/").as_deref(),
            Some("https://dl.example.com")
        );
        assert_eq!(
            mirror_root("http://127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080")
        );
    }

    #[test]
    fn github_is_always_a_source_and_the_mirror_only_ever_precedes_it() {
        let github = "https://github.com/o/r/releases/download/v2.0.0";

        // No mirror: exactly one source, and it is the source of truth.
        let plain = release_sources(None, "o/r", "v2.0.0");
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].base, github);
        assert_eq!(plain[0].label, "GitHub Releases");

        // With one: the mirror is tried first, at the *version-pinned* path, so
        // a mirror that stopped updating cannot hold anyone on an old release.
        let mirrored = release_sources(Some("dl.example.com"), "o/r", "v2.0.0");
        assert_eq!(mirrored.len(), 2);
        assert_eq!(mirrored[0].base, "https://dl.example.com/v2.0.0");
        assert!(mirrored[0].label.contains("dl.example.com"), "{mirrored:?}");
        assert_eq!(
            mirrored[1].base, github,
            "GitHub must survive any mirror setting"
        );

        // And a mirror setting that means "off" does not add a source.
        assert_eq!(release_sources(Some("off"), "o/r", "v2.0.0"), plain);
    }

    /// A driver run over `outcomes`, one per source. Returns the result, the
    /// sources actually attempted, and every line reported to the user.
    async fn drive(
        sources: &[ReleaseSource],
        outcomes: Vec<std::result::Result<(), SourceFailure>>,
    ) -> (Result<()>, Vec<String>, Vec<String>) {
        // Mutexes rather than cells: the driver's callbacks are `Sync`, because
        // the passive startup check drives them from a spawned task.
        let tried = std::sync::Mutex::new(Vec::new());
        let said = std::sync::Mutex::new(Vec::new());
        let outcomes = std::sync::Mutex::new(outcomes.into_iter());
        let result = install_from_first_available(
            sources,
            &|line| said.lock().unwrap().push(line.to_string()),
            &|source| {
                tried.lock().unwrap().push(source.label.clone());
                let outcome = outcomes
                    .lock()
                    .unwrap()
                    .next()
                    .expect("the driver attempted more sources than the test planned");
                Box::pin(async move { outcome })
            },
        )
        .await;
        (
            result,
            tried.into_inner().unwrap(),
            said.into_inner().unwrap(),
        )
    }

    #[tokio::test]
    async fn a_mirror_that_cannot_serve_the_release_falls_back_to_github_and_says_so() {
        let sources = release_sources(Some("dl.example.com"), "o/r", "v2.0.0");
        let (result, tried, said) = drive(
            &sources,
            vec![
                Err(SourceFailure::Unavailable(anyhow!("connection refused"))),
                Ok(()),
            ],
        )
        .await;

        assert!(result.is_ok(), "{:?}", result.unwrap_err());
        assert_eq!(
            tried,
            vec!["the mirror at https://dl.example.com", "GitHub Releases"]
        );
        // The user is told both halves: that the mirror was skipped, with the
        // reason, and which host the binary they now run actually came from.
        let transcript = said.join("\n");
        assert!(transcript.contains("connection refused"), "{transcript}");
        assert!(
            transcript.contains("falling back to GitHub Releases"),
            "{transcript}"
        );
        assert!(
            transcript.contains("downloaded from GitHub Releases"),
            "{transcript}"
        );
    }

    #[tokio::test]
    async fn a_mirror_release_that_fails_verification_is_refused_and_never_laundered() {
        // The mirror is the second place a release can be compromised, and
        // signing is the mitigation, so bytes that fail the signature stop the
        // update where they are. Falling back to GitHub here would install a
        // fine binary while hiding from the user that their mirror is serving
        // something the release key never signed.
        let sources = release_sources(Some("dl.example.com"), "o/r", "v2.0.0");
        let (result, tried, said) = drive(
            &sources,
            vec![Err(SourceFailure::Fatal(anyhow!(
                "signature verification FAILED"
            )))],
        )
        .await;

        let err = result.unwrap_err().to_string();
        assert!(err.contains("signature verification FAILED"), "{err}");
        assert_eq!(
            tried,
            vec!["the mirror at https://dl.example.com"],
            "GitHub must not be tried after a verification failure"
        );
        assert!(
            said.is_empty(),
            "nothing was downloaded, so nothing may be announced: {said:?}"
        );
    }

    #[tokio::test]
    async fn a_lone_github_source_reports_itself_and_needs_no_mirror() {
        let sources = release_sources(None, "o/r", "v2.0.0");
        let (result, tried, said) = drive(&sources, vec![Ok(())]).await;
        assert!(result.is_ok());
        assert_eq!(tried, vec!["GitHub Releases"]);
        assert_eq!(said, vec!["downloaded from GitHub Releases".to_string()]);

        // And when the only source is unavailable there is nothing to fall back
        // to: its error is the error, with no fallback line promising a host
        // that does not exist.
        let (result, _, said) = drive(
            &sources,
            vec![Err(SourceFailure::Unavailable(anyhow!("HTTP 503")))],
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("HTTP 503"));
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn a_mirror_and_github_are_verified_by_one_function_and_one_rule() {
        // Same bytes, same tampering, two hosts: the outcome has to be the
        // same refusal, differing only in which host is named. `verified_checksums`
        // takes no flag and no policy — it cannot be told to be lenient.
        let (signing, id, _public) = test_key(31);
        let published = b"aaaa1111  wizard-x86_64-unknown-linux-gnu.tar.gz\n";
        let signature = sign_release(&signing, id, b"ED", published, "wizard release");
        let swapped = b"beef0000  wizard-x86_64-unknown-linux-gnu.tar.gz\n";

        let sources = release_sources(Some("dl.example.com"), "o/r", "v2.0.0");
        let mirror = &sources[0];
        let github = &sources[1];

        // The refusals: fatal for both, and identical once the host's name is
        // taken out of the sentence.
        let mut refusals = Vec::new();
        for source in [mirror, github] {
            match verified_checksums(swapped, &signature, source, "v2.0.0") {
                Err(SourceFailure::Fatal(err)) => {
                    let text = format!("{err:#}");
                    // Whichever world the embedded key is in. There are
                    // three, and this listed two:
                    //
                    //  - no key compiled in at all (the placeholder), which
                    //    is the tree as committed today;
                    //  - a real key, and the signature does not verify under
                    //    it — "verification FAILED";
                    //  - a real key, and the signature is valid but was made
                    //    by a *different* key, which is what a fixture key
                    //    produces once a real one is committed. That refusal
                    //    says "signed by key X, but this binary trusts Y" and
                    //    matched neither of the other two.
                    //
                    // The third is not hypothetical: it is the world the
                    // moment `contrib/seed-release-key.sh` runs, so without
                    // it this test went red inside the commit that seeds the
                    // release key — found by running that script against a
                    // throwaway clone rather than on release day.
                    assert!(
                        text.contains("verification FAILED")
                            || text.contains("embeds no release signing key")
                            || text.contains("but this binary trusts"),
                        "{text}"
                    );
                    refusals.push(text.replace(&source.label, "<host>"));
                }
                other => panic!("a doctored checksums.txt must be fatal, got {other:?}"),
            }
        }
        assert_eq!(refusals[0], refusals[1]);

        // A signature that never arrives is refused the same way for both.
        let mut missing = Vec::new();
        for source in [mirror, github] {
            match verified_checksums(published, "", source, "v2.0.0") {
                Err(SourceFailure::Fatal(err)) => {
                    missing.push(format!("{err:#}").replace(&source.label, "<host>"));
                }
                other => panic!("an absent signature must be fatal, got {other:?}"),
            }
        }
        assert_eq!(missing[0], missing[1]);

        // Both refusals name the host that served the bytes, which is the only
        // thing about them that is allowed to differ — and is also why the
        // equality above is not two empty strings agreeing with each other.
        for source in [mirror, github] {
            let err = verified_checksums(swapped, &signature, source, "v2.0.0")
                .expect_err("a doctored checksums.txt is refused");
            let SourceFailure::Fatal(err) = err else {
                panic!("verification failures are fatal");
            };
            assert!(format!("{err:#}").contains(&source.label));
        }

        // There is deliberately no "and this one installs" control here: while
        // `wizard-release.pub` holds the placeholder, `release_key` refuses
        // every release, and once it holds a real key nothing this test can
        // sign will verify under it. Which of those two worlds we are in is
        // asserted by `the_embedded_release_key_is_a_real_key_or_a_named_refusal`,
        // and that the verifier accepts genuine minisign output by
        // `a_real_minisign_signature_verifies`.
    }

    /// Temp dir removed on drop (the suite has no `tempfile` dependency).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("wizard-update-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_checksums_file_refuses_the_update() {
        // The release published none at all: a 404, and it names the file so
        // the user can check the release page themselves.
        let url = "https://github.com/o/r/releases/download/v1/checksums.txt";
        let err = checksums_status_check(404, url).unwrap_err().to_string();
        assert!(err.contains("published no checksums.txt"), "{err}");
        assert!(err.contains(url), "{err}");

        // The fetch failed rather than the file being absent: a different,
        // retry-shaped message, but still a refusal.
        let err = checksums_status_check(503, url).unwrap_err().to_string();
        assert!(err.contains("HTTP 503"), "{err}");
        assert!(!err.contains("published no checksums.txt"), "{err}");

        // Both point at the same way out.
        assert!(err.contains("build from source"), "{err}");

        assert!(checksums_status_check(200, url).is_ok());
    }

    #[test]
    fn an_asset_absent_from_checksums_is_never_installed() {
        let text = "aaaa1111  wizard-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            required_digest(text, "wizard-x86_64-unknown-linux-gnu.tar.gz").unwrap(),
            "aaaa1111"
        );
        // No entry, and an empty/garbled checksums.txt, both refuse rather
        // than falling back to an unverified install.
        let err = required_digest(text, "wizard-aarch64-apple-darwin.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not listed in the release's checksums.txt"),
            "{err}"
        );
        assert!(required_digest("", "wizard-x86_64-unknown-linux-gnu.tar.gz").is_err());
    }

    #[test]
    fn staging_happens_under_the_state_dir_and_not_a_shared_temp_dir() {
        // The staging *policy* (private, and re-tightened when it was left
        // loose) belongs to `platform::paths` and is asserted there. What the
        // updater owns is which directory it stages into, and the answer must
        // never drift back to the shared temp dir: on the escalation path the
        // staged file is the argument to `sudo install`.
        let dir = staging_dir().expect("staging dir");
        let wizard_dir = Config::wizard_dir().expect("wizard dir");
        // Under the state tree wherever that tree is, rather than "not
        // `/tmp`": the suite redirects `~/.wizard` into a temp dir of its own,
        // so the shape is what can be asserted here, and it is the shape that
        // makes the staged file private in production.
        assert_eq!(dir, wizard_dir.join("update"), "{}", dir.display());
        assert!(dir.is_dir());
        assert!(
            crate::platform::secrets::is_protected(&dir).expect("stat"),
            "staging dir must not be readable or writable by other users"
        );
    }

    /// The swap itself lives in `platform::exe_swap` and is tested there. What
    /// these three cover is the updater's half: that `wizard update` and deep
    /// evolve name *different* rollback copies, and that a swap which cannot
    /// happen never leaves the user without a binary at the path they run.
    #[test]
    fn install_executable_swaps_atomically_and_keeps_a_backup() {
        let tmp = TempDir::new();
        let dest = tmp.0.join("wizard");
        let source = tmp.0.join("wizard-new");
        std::fs::write(&dest, b"old binary").unwrap();
        std::fs::write(&source, b"new binary").unwrap();

        let backup = exe_swap::install_executable(&source, &dest, UPDATE_BACKUP_SUFFIX).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new binary");
        assert_eq!(backup, tmp.0.join("wizard.bak"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"old binary");
        assert!(
            exe_swap::is_executable(&dest),
            "the installed binary is executable"
        );
        // No scratch file survives a successful install.
        assert!(!leftovers(&tmp.0).iter().any(|n| n.contains(".new.")));
    }

    /// The escalated install path makes the backup `--rollback` needs.
    ///
    /// `/usr/local/bin` is the installer's default on every FHS distro, so
    /// `dir_is_writable` says no and this is the path most updates take. It
    /// ran `sudo install <staged> <dest>` and nothing else, while
    /// `install_over`'s doc promised `<name>.bak` and `rollback_binary` had a
    /// sudo branch built to restore one — so `wizard update` worked and
    /// `wizard update --rollback` answered "no backup … nothing to roll back
    /// to" for exactly the majority of installs.
    ///
    /// Asserted on the plan rather than by running it: a test cannot call
    /// `sudo`, and the defect was a *missing command*, which no observation of
    /// the process afterwards distinguishes from a machine where the backup
    /// was never wanted.
    #[test]
    fn the_escalated_install_backs_the_binary_up_before_replacing_it() {
        let tmp = TempDir::new();
        let dest = tmp.0.join("wizard");
        let staged = tmp.0.join("wizard-new");
        let backup = tmp.0.join("wizard.bak");
        std::fs::write(&dest, b"old binary").unwrap();
        std::fs::write(&staged, b"new binary").unwrap();

        let plan = sudo_install_plan(&staged, &dest, &backup);
        let rendered: Vec<Vec<String>> = plan
            .iter()
            .map(|step| {
                step.iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect()
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                vec![
                    "install".to_string(),
                    "-m755".to_string(),
                    dest.display().to_string(),
                    backup.display().to_string(),
                ],
                vec![
                    "install".to_string(),
                    "-m755".to_string(),
                    staged.display().to_string(),
                    dest.display().to_string(),
                ],
            ],
            "the backup has to be taken before the binary it copies is replaced"
        );

        // A first install has nothing to back up, and `install` would fail on
        // the missing source rather than skip it.
        std::fs::remove_file(&dest).unwrap();
        assert_eq!(
            sudo_install_plan(&staged, &dest, &backup).len(),
            1,
            "no current binary means no backup step"
        );
    }

    #[test]
    fn a_failed_install_leaves_the_old_binary_whole() {
        let tmp = TempDir::new();
        let dest = tmp.0.join("wizard");
        std::fs::write(&dest, b"old binary").unwrap();

        // A source that cannot be copied (a directory reads as EISDIR after
        // the scratch file already exists) stands in for a copy that dies
        // halfway: the invariant is that `dest` is never the partial result.
        let source = tmp.0.join("not-a-binary");
        std::fs::create_dir(&source).unwrap();
        let err = exe_swap::install_executable(&source, &dest, UPDATE_BACKUP_SUFFIX).unwrap_err();
        assert!(format!("{err:#}").contains("not-a-binary"), "{err:#}");

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"old binary",
            "the running binary is untouched when the install fails"
        );
        assert!(
            !leftovers(&tmp.0).iter().any(|n| n.contains(".new.")),
            "the scratch copy is cleaned up: {:?}",
            leftovers(&tmp.0)
        );

        // And a source that does not exist at all fails before anything is
        // written, leaving no backup claiming an install happened.
        let err = exe_swap::install_executable(&tmp.0.join("absent"), &dest, UPDATE_BACKUP_SUFFIX);
        assert!(err.is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"old binary");
        assert!(!tmp.0.join("wizard.bak").exists());
    }

    #[test]
    fn install_executable_creates_a_binary_that_was_not_there() {
        let tmp = TempDir::new();
        let dest = tmp.0.join("wizard");
        let source = tmp.0.join("wizard-new");
        std::fs::write(&source, b"new binary").unwrap();

        let backup = exe_swap::install_executable(&source, &dest, EVOLVE_BACKUP_SUFFIX).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new binary");
        assert_eq!(
            backup,
            tmp.0.join("wizard.prev"),
            "a deep evolve's way back is `.prev`, never the updater's `.bak`"
        );
        assert!(
            !backup.exists(),
            "nothing was displaced, so there is no rollback copy to invent"
        );
    }

    // -----------------------------------------------------------------------
    // Release signatures
    // -----------------------------------------------------------------------

    /// A real `minisign 0.12` public key, signature and payload, kept verbatim
    /// so the parser is tested against the format as the tool actually emits it
    /// (prehashed `ED`, which is minisign's default) rather than against this
    /// module's own idea of it. The matching secret key was generated for this
    /// test, used once, and never written anywhere: it signs nothing that
    /// exists, and it is not the release key (which lives in
    /// `wizard-release.pub`).
    const FIXTURE_PUBLIC_KEY: &str = "untrusted comment: minisign public key E56A2757A679780A\n\
         RWQKeHmmVydq5U7jz22zhEYPld2/F3fgK2SGaCq3AdMQtfss0H1OwtNK\n";
    const FIXTURE_CHECKSUMS: &[u8] = b"aaaa1111  wizard-x86_64-unknown-linux-gnu.tar.gz\n";
    const FIXTURE_SIGNATURE: &str = "untrusted comment: wizard release\n\
         RUQKeHmmVydq5XKw7x6bK03bHLFO7v0silFGM13xxyh5UJgrBzk2AFrUZ1H2+p0xGwoCTEq4GXpeusyQM5/QGPEzJwhmW13xUgc=\n\
         trusted comment: wizard release checksums\n\
         aW3skZnhr0CHg7f4PNTod25wrIqC1DMFVOAip9TjdiUB5TqCfRn2DHBIlI8SgzQXKtVmkBgisUhPAp2vRNsVDA==\n";

    /// A deterministic throwaway signing key and the minisign public key file
    /// naming it, so a test can mint both releases the verifier must accept and
    /// releases it must refuse. A fixed seed, because these are fixtures.
    fn test_key(seed: u8) -> (ed25519_dalek::SigningKey, [u8; 8], String) {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let id = [seed, 1, 2, 3, 4, 5, 6, 7];
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&id);
        raw.extend_from_slice(&signing.verifying_key().to_bytes());
        let text = format!(
            "untrusted comment: minisign public key {}\n{}\n",
            key_id_hex(&id),
            BASE64.encode(&raw)
        );
        (signing, id, text)
    }

    /// Write a `.minisig` for `data`: `algorithm` picks minisign's raw (`Ed`) or
    /// prehashed (`ED`) mode, and `id` is stamped as-is so a test can claim a
    /// key id the signature was not made with.
    fn sign_release(
        signing: &ed25519_dalek::SigningKey,
        id: [u8; 8],
        algorithm: &[u8; 2],
        data: &[u8],
        trusted: &str,
    ) -> String {
        use ed25519_dalek::Signer as _;
        let message = if algorithm == b"ED" {
            blake2b512(data).to_vec()
        } else {
            data.to_vec()
        };
        let signature = signing.sign(&message);
        let mut line = algorithm.to_vec();
        line.extend_from_slice(&id);
        line.extend_from_slice(&signature.to_bytes());
        let mut global = signature.to_bytes().to_vec();
        global.extend_from_slice(trusted.as_bytes());
        format!(
            "untrusted comment: wizard release\n{}\ntrusted comment: {trusted}\n{}\n",
            BASE64.encode(&line),
            BASE64.encode(signing.sign(&global).to_bytes())
        )
    }

    #[test]
    fn a_real_minisign_signature_verifies() {
        // The interop test: minisign's own output, parsed and checked here.
        let key = parse_public_key(FIXTURE_PUBLIC_KEY).expect("fixture key");
        assert_eq!(key_id_hex(&key.id), "E56A2757A679780A");
        let signature = parse_signature(FIXTURE_SIGNATURE).expect("fixture signature");
        // minisign has signed a blake2b-512 prehash by default since 0.10.
        assert_eq!(&signature.algorithm, b"ED");
        assert_eq!(signature.trusted_comment, "wizard release checksums");
        verify_signature(&key, &signature, FIXTURE_CHECKSUMS).expect("genuine signature verifies");
    }

    /// The signature `.github/workflows/release.yml` actually produces is one
    /// `wizard update` actually accepts.
    ///
    /// `a_real_minisign_signature_verifies` above proves this verifier reads
    /// genuine minisign output, but against a fixture whose trusted comment is
    /// `wizard release checksums`. The workflow signs with a *different*
    /// string — `-t "wizard <tag> checksums, signed by the wizard release
    /// key"` — and the trusted comment is covered by the global signature, so
    /// it is not decoration this verifier skips past: get the coupling wrong
    /// and every asset of a release fails to verify, discovered by users on a
    /// tag that has already published.
    ///
    /// Nothing else pins the two together. The workflow is a YAML string and
    /// this is Rust; they have never run in the same process, and until this
    /// release they had never run at all.
    #[test]
    fn a_genuinely_signed_older_release_cannot_be_served_for_a_newer_one() {
        // The downgrade: every asset name in a release is version-free, so a
        // host answering `<mirror>/v2.0.0/…` can hand back v1.0.0's real,
        // key-signed files. Everything cryptographic passes — same key, valid
        // global signature, digests matching their own checksums.txt — and
        // before this the client had no way to notice, because it read nothing
        // out of the trusted comment. With `auto = true` the version then looks
        // old again and it repeats on every check.
        let (signing, id, _public) = test_key(11);
        let old_checksums = b"deadbeef  wizard-x86_64-unknown-linux-gnu.tar.gz\n";
        let armored = sign_release(
            &signing,
            id,
            b"ED",
            old_checksums,
            "wizard v1.0.0 checksums, signed by the wizard release key",
        );
        let signature = parse_signature(&armored).expect("parses");

        // The signature itself is genuine. That is the whole point.
        assert!(
            binds_to_tag(&signature.trusted_comment, "v1.0.0").is_ok(),
            "it really is v1.0.0's signature"
        );

        let err = binds_to_tag(&signature.trusted_comment, "v2.0.0")
            .expect_err("v1.0.0's signature must not satisfy a v2.0.0 request");
        let text = format!("{err:#}");
        assert!(text.contains("different release"), "{text}");
        assert!(text.contains("v2.0.0"), "{text}");

        // A tag is matched as a whole word, so a longer tag that merely
        // contains the one being installed cannot stand in for it.
        assert!(binds_to_tag("wizard v2.0.0-rc1 checksums", "v2.0.0").is_err());
        assert!(binds_to_tag("wizard v2.0.0 checksums", "v2.0.0").is_ok());

        // And a comment naming nothing is refused rather than waved through.
        assert!(binds_to_tag("wizard checksums, signed by the release key", "v2.0.0").is_err());

        // The assertions above drive `binds_to_tag` directly, because the real
        // entry point calls `release_key()` first and this tree still carries
        // the placeholder — so it refuses before it could reach any of this.
        // That leaves the wiring itself unproven, and the wiring is the whole
        // fix: verifying the signature and then not asking what it was for is
        // exactly the state this replaces. Pinned from the source.
        let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/update.rs"))
            .expect("this file");
        let body = source
            .split_once("fn verify_release_signature")
            .expect("the entry point exists")
            .1;
        let body = body.split_once("\n}\n").map_or(body, |(body, _)| body);
        assert!(
            body.contains("binds_to_tag(&signature.trusted_comment, tag)"),
            "verify_release_signature must bind the signature to the tag it was asked for"
        );
    }

    #[test]
    fn the_workflow_s_trusted_comment_round_trips_through_this_verifier() {
        let (signing, id, public) = test_key(9);
        let key = parse_public_key(&public).expect("key");
        let checksums = b"abc123  wizard-x86_64-unknown-linux-gnu.tar.gz\n";

        // Verbatim from the `Sign checksums` step, with the tag substituted.
        let trusted = "wizard v2.0.0 checksums, signed by the wizard release key";
        let armored = sign_release(&signing, id, b"ED", checksums, trusted);

        let signature = parse_signature(&armored).expect("the workflow's signature parses");
        assert_eq!(
            signature.trusted_comment, trusted,
            "the comment must survive the round trip intact, since it is signed"
        );
        verify_signature(&key, &signature, checksums)
            .expect("a signature made the way the workflow makes it must verify");

        // And the coupling is real rather than incidental: editing the comment
        // after signing breaks it, which is what makes the assertion above
        // worth having.
        let tampered = armored.replace(trusted, "wizard v2.0.0 checksums, signed by somebody");
        let forged = parse_signature(&tampered).expect("still parses");
        assert!(
            verify_signature(&key, &forged, checksums).is_err(),
            "a rewritten trusted comment must not verify"
        );
    }

    #[test]
    fn a_missing_signature_refuses_the_update() {
        // The release published none: a 404, named so it can be checked on the
        // release page. Any other status is a fetch worth retrying. Both refuse,
        // and both say what to do instead.
        let url = "https://github.com/o/r/releases/download/v1/checksums.txt.minisig";
        let err = signature_status_check(404, url).unwrap_err().to_string();
        assert!(err.contains("published no checksums.txt.minisig"), "{err}");
        assert!(err.contains(url), "{err}");
        let err = signature_status_check(503, url).unwrap_err().to_string();
        assert!(err.contains("HTTP 503"), "{err}");
        assert!(err.contains("build from source"), "{err}");
        assert!(signature_status_check(200, url).is_ok());

        // And an empty or truncated signature file is a refusal, not a skip:
        // there is no shape of `.minisig` that means "install it anyway".
        for text in ["", "untrusted comment: wizard release\n"] {
            assert!(parse_signature(text).is_err(), "{text:?} must not parse");
            assert!(verify_release_signature(FIXTURE_CHECKSUMS, text, "v2.0.0").is_err());
        }
    }

    #[test]
    fn a_corrupted_signature_refuses_the_update() {
        let (signing, id, public) = test_key(11);
        let key = parse_public_key(&public).expect("key");
        let data = b"aaaa1111  wizard-x86_64-unknown-linux-gnu.tar.gz\n";
        let good = sign_release(&signing, id, b"ED", data, "wizard release");
        verify_signature(&key, &parse_signature(&good).expect("parses"), data)
            .expect("the untampered signature verifies");

        // One flipped byte inside the signature: it still parses, and it still
        // must not verify.
        let mut lines: Vec<String> = good.lines().map(str::to_string).collect();
        let flipped = {
            let mut raw = BASE64.decode(lines[1].as_str()).expect("base64");
            raw[20] ^= 0x01;
            BASE64.encode(raw)
        };
        lines[1] = flipped;
        let tampered = format!("{}\n", lines.join("\n"));
        let err = verify_signature(&key, &parse_signature(&tampered).expect("parses"), data)
            .unwrap_err()
            .to_string();
        assert!(err.contains("verification FAILED"), "{err}");

        // Truncated, and outright garbage, are refused at the parse.
        let mut short = BASE64.decode(lines[1].as_str()).expect("base64");
        short.truncate(40);
        let mut broken: Vec<String> = good.lines().map(str::to_string).collect();
        broken[1] = BASE64.encode(short);
        assert!(parse_signature(&broken.join("\n")).is_err());
        broken[1] = "!!! not base64 !!!".to_string();
        assert!(parse_signature(&broken.join("\n")).is_err());
    }

    #[test]
    fn a_swapped_tarball_with_matching_checksums_refuses_the_update() {
        // The attack the signature exists for: whoever serves the download can
        // serve a different tarball *and* a checksums.txt that matches it, so
        // the sha256 chain agrees with itself. The signature is over the
        // release's own checksums.txt, so the rewritten one has nothing that
        // verifies against the key this binary carries.
        let (signing, id, public) = test_key(13);
        let key = parse_public_key(&public).expect("key");
        let published = b"aaaa1111  wizard-x86_64-unknown-linux-gnu.tar.gz\n";
        let signature = parse_signature(&sign_release(
            &signing,
            id,
            b"ED",
            published,
            "wizard release",
        ))
        .expect("parses");
        verify_signature(&key, &signature, published).expect("the published file verifies");

        // The digest of the attacker's binary, in an otherwise identical file.
        let swapped = b"beef0000  wizard-x86_64-unknown-linux-gnu.tar.gz\n";
        let err = verify_signature(&key, &signature, swapped)
            .unwrap_err()
            .to_string();
        assert!(err.contains("verification FAILED"), "{err}");
        assert!(err.contains("nothing was installed"), "{err}");
        // Which is also the state `parse_checksums` would have been happy with:
        // the doctored file is well-formed, and only the signature catches it.
        assert_eq!(
            parse_checksums(
                std::str::from_utf8(swapped).unwrap(),
                "wizard-x86_64-unknown-linux-gnu.tar.gz"
            ),
            Some("beef0000".to_string())
        );
    }

    #[test]
    fn a_signature_from_another_key_refuses_the_update() {
        let (theirs, their_id, _) = test_key(17);
        let (_, our_id, our_public) = test_key(19);
        let key = parse_public_key(&our_public).expect("key");
        let data = b"aaaa1111  wizard.tar.gz\n";

        // Signed by a key this binary does not trust, and saying so.
        let signature =
            parse_signature(&sign_release(&theirs, their_id, b"ED", data, "x")).expect("parses");
        let err = verify_signature(&key, &signature, data)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&key_id_hex(&their_id)), "{err}");
        assert!(err.contains(&key_id_hex(&our_id)), "{err}");

        // And the same signature wearing our key id: the id is a hint, never
        // the check, so this fails on the signature itself.
        let signature =
            parse_signature(&sign_release(&theirs, our_id, b"ED", data, "x")).expect("parses");
        let err = verify_signature(&key, &signature, data)
            .unwrap_err()
            .to_string();
        assert!(err.contains("verification FAILED"), "{err}");
    }

    #[test]
    fn a_rewritten_trusted_comment_refuses_the_update() {
        // The trusted comment rides inside the signed envelope (that is the
        // whole point of the second signature), so editing it has to fail even
        // though nothing here reads it.
        let (signing, id, public) = test_key(23);
        let key = parse_public_key(&public).expect("key");
        let data = b"aaaa1111  wizard.tar.gz\n";
        let signed = sign_release(&signing, id, b"ED", data, "timestamp:1  file:checksums.txt");
        let rewritten = signed.replace("timestamp:1", "timestamp:2");
        let err = verify_signature(&key, &parse_signature(&rewritten).expect("parses"), data)
            .unwrap_err()
            .to_string();
        assert!(err.contains("trusted comment"), "{err}");
    }

    #[test]
    fn only_minisigns_two_algorithms_are_accepted() {
        let (signing, id, public) = test_key(29);
        let key = parse_public_key(&public).expect("key");
        let data = b"aaaa1111  wizard.tar.gz\n";
        // Legacy raw-file signatures still verify: a release cut with an older
        // minisign is not a security failure.
        let raw = parse_signature(&sign_release(&signing, id, b"Ed", data, "x")).expect("parses");
        verify_signature(&key, &raw, data).expect("legacy `Ed` verifies");
        // But a signature over the *file* labelled as prehashed does not, and
        // an algorithm nobody has heard of is refused before any hashing.
        let mislabelled =
            parse_signature(&sign_release(&signing, id, b"ED", data, "x")).expect("parses");
        assert!(verify_signature(&key, &mislabelled, &blake2b512(data)).is_err());
        let unknown =
            parse_signature(&sign_release(&signing, id, b"Xx", data, "x")).expect("parses");
        let err = verify_signature(&key, &unknown, data)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown minisign algorithm"), "{err}");
    }

    #[test]
    fn a_public_key_that_is_not_one_refuses_the_update() {
        // A key file that never got a key: the refusal names the placeholder
        // rather than reporting it as corrupt base64, because the fix differs.
        let err = parse_public_key(
            "untrusted comment: x\nRELEASE-SIGNING-KEY-NOT-YET-GENERATED-see-SECURITY.md\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("embeds no release signing key"), "{err}");
        // A comment and nothing else, a truncated key, and a key naming an
        // algorithm minisign does not use.
        assert!(parse_public_key("untrusted comment: x\n").is_err());
        assert!(parse_public_key("untrusted comment: x\nRWQ=\n").is_err());
        let mut raw = b"Xx".to_vec();
        raw.extend_from_slice(&[0u8; 40]);
        let err = parse_public_key(&format!("untrusted comment: x\n{}\n", BASE64.encode(&raw)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not minisign's ed25519"), "{err}");
    }

    #[test]
    fn the_embedded_release_key_is_a_real_key_or_a_named_refusal() {
        // Whatever `wizard-release.pub` holds, exactly one of these is true: it
        // parses (and is not the throwaway key these tests sign with), or every
        // update refuses with the message that says why. A malformed key that
        // fails some other way fails here.
        match release_key() {
            Ok(key) => {
                let fixture = parse_public_key(FIXTURE_PUBLIC_KEY).expect("fixture key");
                assert_ne!(
                    key.key.to_bytes(),
                    fixture.key.to_bytes(),
                    "the test fixture key must never become the release key"
                );
            }
            Err(err) => assert!(
                err.to_string().contains("embeds no release signing key"),
                "{err}"
            ),
        }
    }

    // -- install.sh, driven for real -----------------------------------------
    //
    // `install.sh` is the other half of every rule in this module, and it is
    // 1,900 lines of bash that no `cargo test` would otherwise touch. Sourcing
    // it with `WIZARD_SELFTEST=1` defines its functions without installing
    // anything, so the download path can be driven against a stub `curl` that
    // decides which host answers.

    /// Absolute path to the installer under test.
    fn install_sh() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
    }

    /// Run `script` under bash with a stub `curl` first on PATH.
    ///
    /// The stub serves any URL containing `serves` by writing `body` to the
    /// request's `-o` path, fails every other URL the way curl fails a 404
    /// (exit 22), and appends every URL it saw to `<dir>/curl.log`.
    fn run_installer_script(
        dir: &Path,
        serves: &str,
        body: &str,
        script: &str,
    ) -> std::process::Output {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("stub bin dir");
        std::fs::write(
            bin.join("curl"),
            format!(
                r#"#!/usr/bin/env bash
# Stub curl: the test decides which host answers.
out=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) out="$2"; shift 2 ;;
        http*) url="$1"; shift ;;
        *) shift ;;
    esac
done
printf '%s\n' "$url" >>"{log}"
case "$url" in
    *{serves}*)
        [ -n "$out" ] && printf '%s' '{body}' >"$out"
        exit 0
        ;;
esac
exit 22
"#,
                log = dir.join("curl.log").display(),
                serves = serves,
                body = body,
            ),
        )
        .expect("write stub curl");
        exe_swap::set_executable(&bin.join("curl")).expect("chmod stub curl");

        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::process::Command::new("bash")
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .env("WIZARD_SELFTEST", "1")
            .env("HOME", dir)
            .current_dir(dir)
            .output()
            .expect("run bash")
    }

    #[test]
    fn install_sh_falls_back_to_github_when_the_mirror_fails() {
        let tmp = TempDir::new();
        // Only GitHub answers. The pinned version keeps the tag resolution off
        // the network, so the only requests are the asset itself.
        let out = run_installer_script(
            &tmp.0,
            "github.com",
            "github-bytes",
            &format!(
                r#"set -euo pipefail
export WIZARD_MIRROR=https://dl.example.invalid
export WIZARD_VERSION=v9.9.9
source '{}'
download_release_asset wizard-x86_64-unknown-linux-gnu.tar.gz "$PWD/asset"
printf 'SOURCE=%s\n' "$DOWNLOAD_SOURCE"
"#,
                install_sh().display()
            ),
        );
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(out.status.success(), "{stdout}\n{stderr}");

        // The mirror was tried first, at the version-pinned path…
        let log = std::fs::read_to_string(tmp.0.join("curl.log")).expect("curl log");
        let urls: Vec<&str> = log.lines().collect();
        assert_eq!(
            urls.first().copied(),
            Some("https://dl.example.invalid/v9.9.9/wizard-x86_64-unknown-linux-gnu.tar.gz"),
            "{log}"
        );
        // …GitHub served it, and the user is told both facts.
        assert!(
            urls.last().is_some_and(|u| u.contains("github.com")),
            "{log}"
        );
        assert!(
            stderr.contains("falling back to GitHub releases"),
            "the fallback must be announced: {stderr}"
        );
        assert!(stdout.contains("SOURCE=GitHub releases"), "{stdout}");
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("asset")).expect("asset"),
            "github-bytes"
        );
    }

    #[test]
    fn install_sh_refuses_a_mirror_release_exactly_as_it_refuses_a_github_one() {
        // The property: a mirror is not a way to be verified less strictly.
        // The same doctored release, served by each host in turn, has to end
        // in the same refusal — and it does, because `download_release_asset`
        // is the only thing in the script that knows where bytes came from and
        // `verify_checksum` is the only thing that decides whether to keep
        // them.
        // A syntactically real release key, so the script gets past "this
        // install.sh carries no signing key" and actually fetches and checks
        // the release it was pointed at. Nothing the stub serves is signed by
        // it, which is the point.
        let (_, _, public) = test_key(37);
        let key_line = public.lines().nth(1).expect("key line").to_string();
        let script = |mirror: &str| {
            format!(
                r#"set -euo pipefail
export WIZARD_MIRROR={mirror}
export WIZARD_VERSION=v9.9.9
source '{installer}'
WIZARD_RELEASE_PUBKEY='{key_line}'
download_release_asset wizard-x86_64-unknown-linux-gnu.tar.gz "$PWD/tarball"
verify_checksum "$PWD/tarball" wizard-x86_64-unknown-linux-gnu.tar.gz
printf 'INSTALLED-ANYWAY\n'
"#,
                installer = install_sh().display()
            )
        };

        let mut refusals = Vec::new();
        for (mirror, serves) in [
            ("https://dl.example.invalid", "dl.example.invalid"),
            ("off", "github.com"),
        ] {
            let tmp = TempDir::new();
            let out = run_installer_script(
                &tmp.0,
                serves,
                "d3adbeef  wizard-x86_64-unknown-linux-gnu.tar.gz",
                &script(mirror),
            );
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            assert!(
                !out.status.success(),
                "a release served by {serves} was accepted: {stdout}\n{stderr}"
            );
            assert!(!stdout.contains("INSTALLED-ANYWAY"), "{stdout}");
            // The host that served it is the one the test aimed at.
            let log = std::fs::read_to_string(tmp.0.join("curl.log")).expect("curl log");
            assert!(log.contains(serves), "{log}");
            refusals.push(
                stderr
                    .lines()
                    .filter(|line| line.starts_with("error:"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        assert!(!refusals[0].is_empty(), "the refusal says something");
        assert_eq!(
            refusals[0], refusals[1],
            "a mirror-served release and a GitHub-served one must be refused identically"
        );
    }

    #[test]
    fn install_sh_aborts_the_whole_install_when_the_native_asset_fails_verification() {
        // `wizard-native` is fetched by `install_native_gui`, the last step of
        // `main`, and it is the one asset whose verification sits under a
        // function with a `|| return 1` caller. When that call was a command
        // substitution, `die`'s `exit 1` ended only the subshell: the installer
        // shrugged, warned "no runnable native build" — naming a cause that had
        // nothing to do with what happened — and exited 0 after refusing an
        // asset it could not verify. Nothing unverified was installed either
        // way, but SECURITY.md's "every failure aborts" was false for this one
        // asset, so the exit status is pinned here.
        //
        // The stub answers every URL with the same bytes, so `checksums.txt`
        // and its `.minisig` are the same garbage and the signature cannot
        // verify — the failure a real tampered release would produce. A host
        // with no signature checker at all reaches the same `die` by the other
        // branch, so this runs everywhere.
        let (_, _, public) = test_key(47);
        let key_line = public.lines().nth(1).expect("key line").to_string();
        let tmp = TempDir::new();
        let out = run_installer_script(
            &tmp.0,
            "github.com",
            "d3adbeef  wizard-native.tar.gz",
            &format!(
                r#"set -euo pipefail
export WIZARD_VERSION=v9.9.9
export WIZARD_NATIVE=1
source '{installer}'
WIZARD_RELEASE_PUBKEY='{key_line}'
install_native_gui
printf 'CONTINUED\n'
"#,
                installer = install_sh().display()
            ),
        );
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            !out.status.success(),
            "an unverifiable wizard-native asset let the install finish 0: {stdout}\n{stderr}"
        );
        assert!(
            !stdout.contains("CONTINUED"),
            "the install carried on past the refusal: {stdout}"
        );
        assert!(
            stderr.lines().any(|line| line.starts_with("error:")),
            "the refusal must say what happened: {stderr}"
        );
        // And it must not be reported as a missing build, which is what sends
        // the reader looking for an unsupported platform instead of a bad
        // download.
        assert!(
            !stderr.contains("could not install the native GUI"),
            "a verification failure was reported as an absent asset: {stderr}"
        );
    }

    /// Run `script` with a stub `curl` that serves a whole release: a request
    /// whose URL ends in the name of one of `assets` is answered with that
    /// file's bytes, anything else fails the way curl fails a 404, and every
    /// URL is appended to `<dir>/curl.log`.
    ///
    /// Separate from [`run_installer_script`] because the signature path fetches
    /// two files that must differ — `checksums.txt` and its `.minisig` — and a
    /// stub that answers every URL with the same body cannot express that.
    fn run_installer_script_serving(
        dir: &Path,
        assets: &[(&str, &str)],
        script: &str,
    ) -> std::process::Output {
        let bin = dir.join("bin");
        let served = dir.join("served");
        std::fs::create_dir_all(&bin).expect("stub bin dir");
        std::fs::create_dir_all(&served).expect("served dir");
        for (name, body) in assets {
            std::fs::write(served.join(name), body).expect("write served asset");
        }
        std::fs::write(
            bin.join("curl"),
            format!(
                r#"#!/usr/bin/env bash
# Stub curl: the release lives in a directory, addressed by asset name.
out=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) out="$2"; shift 2 ;;
        http*) url="$1"; shift ;;
        *) shift ;;
    esac
done
printf '%s\n' "$url" >>"{log}"
name="${{url##*/}}"
if [ -n "$name" ] && [ -f "{served}/$name" ]; then
    [ -n "$out" ] && cp "{served}/$name" "$out"
    exit 0
fi
exit 22
"#,
                log = dir.join("curl.log").display(),
                served = served.display(),
            ),
        )
        .expect("write stub curl");
        exe_swap::set_executable(&bin.join("curl")).expect("chmod stub curl");

        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::process::Command::new("bash")
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .env("WIZARD_SELFTEST", "1")
            .env("HOME", dir)
            .current_dir(dir)
            .output()
            .expect("run bash")
    }

    /// Whether this host can check a minisign signature at all — `minisign`, or
    /// an openssl with ed25519 and blake2b. Asked with the installer's own
    /// probe, so the answer is the one it will act on. macOS CI has neither
    /// (LibreSSL), and there the two signature tests below have nothing to
    /// drive; every other assertion about the binding still runs in
    /// `install_sh_matches_the_tag_as_a_whole_word`.
    fn installer_can_check_signatures() -> bool {
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "source '{}'\ncommand -v minisign >/dev/null 2>&1 || openssl_can_verify",
                install_sh().display()
            ))
            .env("WIZARD_SELFTEST", "1")
            .output()
            .expect("run bash");
        out.status.success()
    }

    /// A release signed for one tag cannot be installed as another — the
    /// installer's half of `binds_to_tag`.
    ///
    /// The same signed downgrade as
    /// `a_genuinely_signed_older_release_cannot_be_served_for_a_newer_one`, but
    /// against `install.sh`, which is the path `curl | bash` uses and the path
    /// that reaches the mirror first. Everything cryptographic here is genuine:
    /// the checksums.txt below really is signed by the key the script is given,
    /// with a valid global signature over its trusted comment. The only thing
    /// wrong with it is which release it was signed for.
    #[test]
    fn install_sh_refuses_a_release_signed_for_another_tag() {
        if !installer_can_check_signatures() {
            return;
        }
        let (signing, id, public) = test_key(41);
        let key_line = public.lines().nth(1).expect("key line").to_string();
        let checksums = b"aaaa1111  wizard-x86_64-unknown-linux-gnu.tar.gz\n";
        // The wording the release workflow signs, for the *older* release.
        let signature = sign_release(
            &signing,
            id,
            b"ED",
            checksums,
            "wizard v1.0.0 checksums, signed by the wizard release key",
        );

        let tmp = TempDir::new();
        let out = run_installer_script_serving(
            &tmp.0,
            &[
                ("checksums.txt", std::str::from_utf8(checksums).unwrap()),
                ("checksums.txt.minisig", &signature),
            ],
            &format!(
                r#"set -euo pipefail
export WIZARD_VERSION=v9.9.9
source '{installer}'
WIZARD_RELEASE_PUBKEY='{key_line}'
verify_release_checksums
printf 'ACCEPTED\n'
"#,
                installer = install_sh().display()
            ),
        );
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            !out.status.success() && !stdout.contains("ACCEPTED"),
            "v1.0.0's signed checksums were accepted as v9.9.9: {stdout}\n{stderr}"
        );
        // And the refusal names both releases, because "signature verification
        // failed" would send the reader looking for a corrupt download.
        assert!(
            stderr.contains("v1.0.0") && stderr.contains("v9.9.9"),
            "the refusal must name the mismatch: {stderr}"
        );
    }

    /// The other half: the signature the release workflow really produces, for
    /// the release really being installed, still verifies. A binding check that
    /// refused everything would pass the test above and break every install.
    #[test]
    fn install_sh_accepts_a_signature_that_names_the_release_being_installed() {
        if !installer_can_check_signatures() {
            return;
        }
        let (signing, id, public) = test_key(43);
        let key_line = public.lines().nth(1).expect("key line").to_string();
        let checksums = b"aaaa1111  wizard-x86_64-unknown-linux-gnu.tar.gz\n";
        let signature = sign_release(
            &signing,
            id,
            b"ED",
            checksums,
            "wizard v9.9.9 checksums, signed by the wizard release key",
        );

        let tmp = TempDir::new();
        let out = run_installer_script_serving(
            &tmp.0,
            &[
                ("checksums.txt", std::str::from_utf8(checksums).unwrap()),
                ("checksums.txt.minisig", &signature),
            ],
            &format!(
                r#"set -euo pipefail
export WIZARD_VERSION=v9.9.9
source '{installer}'
WIZARD_RELEASE_PUBKEY='{key_line}'
verify_release_checksums
printf 'ACCEPTED\n'
"#,
                installer = install_sh().display()
            ),
        );
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            out.status.success() && stdout.contains("ACCEPTED"),
            "a correctly signed release was refused: {stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("signed for v9.9.9"),
            "the confirmation says which release it was signed for: {stdout}"
        );
    }

    /// The comment is matched by whole word, exactly like [`binds_to_tag`]: a
    /// tag that is only a prefix of the signed one is a different release, and
    /// the wording around the tag is free to change.
    ///
    /// Driven directly rather than through a signature, so it runs on hosts
    /// with no way to check one.
    #[test]
    fn install_sh_matches_the_tag_as_a_whole_word() {
        let tmp = TempDir::new();
        let out = run_installer_script_serving(
            &tmp.0,
            &[],
            &format!(
                r#"set -uo pipefail
source '{installer}'
comment='wizard v1.0.0 checksums, signed by the wizard release key'
for tag in v1.0.0 v1.0 v1.0.0.1 v2.0.0 '' wizard; do
    if comment_names_tag "$comment" "$tag"; then
        printf 'MATCH %s\n' "${{tag:-<empty>}}"
    else
        printf 'NO %s\n' "${{tag:-<empty>}}"
    fi
done
"#,
                installer = install_sh().display()
            ),
        );
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(out.status.success(), "{stdout}\n{stderr}");
        let verdicts: Vec<&str> = stdout.lines().collect();
        assert_eq!(
            verdicts,
            vec![
                "MATCH v1.0.0",
                "NO v1.0",
                "NO v1.0.0.1",
                "NO v2.0.0",
                "NO <empty>",
                // Any whole word in the comment matches, which is why the tag
                // it is compared against has to come from the install, never
                // from the comment.
                "MATCH wizard",
            ],
            "{stdout}"
        );
    }

    #[test]
    fn the_metadata_fetches_are_bounded() {
        // checksums.txt and its signature are read into memory from a host
        // nothing has authenticated yet, and they are read *before* the thing
        // that would authenticate it. One byte over the cap is a refusal that
        // names the URL, so the message points at the host that did it.
        assert!(
            metadata_cap_check(
                MAX_METADATA_BYTES,
                "https://mirror.example/checksums.txt",
                VERIFY_HINT
            )
            .is_ok()
        );
        let err = metadata_cap_check(
            MAX_METADATA_BYTES + 1,
            "https://mirror.example/checksums.txt",
            VERIFY_HINT,
        )
        .expect_err("over the cap");
        let err = format!("{err:#}");
        assert!(
            err.contains("https://mirror.example/checksums.txt"),
            "{err}"
        );
        assert!(
            err.contains("refusing before anything is verified"),
            "{err}"
        );

        // And the bound is a bound, not a formality: the largest real
        // checksums.txt is a few hundred bytes, so it sits far below the
        // tarball ceiling rather than inheriting it.
        const { assert!(MAX_METADATA_BYTES < MAX_DOWNLOAD_BYTES) };
    }

    #[test]
    fn security_md_describes_install_sh_at_its_real_length() {
        // SECURITY.md offers "read it first" as a mitigation and then tells you
        // what that costs. A stale number there understates the cost, which is
        // the one thing that sentence exists to be honest about.
        let installer = std::fs::read_to_string(install_sh()).expect("read install.sh");
        let lines = installer.lines().count();
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/SECURITY.md"))
            .expect("read SECURITY.md");
        let stated = format!("{},{:03} lines", lines / 1000, lines % 1000);
        assert!(
            doc.contains(&stated),
            "SECURITY.md must say install.sh is {stated} (it is {lines} lines)"
        );
    }

    #[test]
    fn the_installer_and_the_binary_trust_the_same_key() {
        // `install.sh` cannot include_str!, so it carries the key inline. Two
        // copies of a root of trust that can drift apart is how a signed
        // release ends up installable only one of the two ways.
        let key_line = RELEASE_PUBLIC_KEY
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("untrusted comment:"))
            .expect("wizard-release.pub holds a key line");
        let installer = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
            .expect("read install.sh");
        assert!(
            installer.contains(&format!("WIZARD_RELEASE_PUBKEY=\"{key_line}\"")),
            "install.sh must embed the same key as wizard-release.pub ({key_line})"
        );
    }

    // -- install.sh, on a host that has almost nothing ------------------------
    //
    // The installer has to find *some* way to check a signature, and which ways
    // exist is a property of the host, not of the release. These tests take the
    // machine's PATH away and hand back exactly one tool at a time, which is
    // how a stock Mac (no minisign, and an /usr/bin/openssl that is LibreSSL)
    // gets tested from Linux CI.

    /// The tools install.sh needs before it can do anything at all, whichever
    /// verifier it ends up using. Everything past this list is the test's
    /// subject rather than its scaffolding.
    const HOST_BASELINE: &[&str] = &[
        "bash", "curl", "tar", "mktemp", "sed", "grep", "awk", "tr", "od", "dd", "cat", "cp", "rm",
        "mkdir", "wc", "tail", "head", "uname", "sort",
    ];

    /// The absolute path to `tool` on the current PATH, or None.
    fn which(tool: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(tool))
            .find(|candidate| candidate.is_file())
    }

    /// Build a bin directory holding symlinks to exactly `tools`, so a script
    /// run with it as its whole PATH sees that host and no other.
    fn host_with(dir: &Path, tools: &[&str]) -> PathBuf {
        let bin = dir.join("host-bin");
        std::fs::create_dir_all(&bin).expect("host bin dir");
        for tool in HOST_BASELINE.iter().chain(tools) {
            if let Some(path) = which(tool) {
                let link = bin.join(tool);
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(path, link).expect("symlink tool");
            }
        }
        bin
    }

    /// Source install.sh on a host holding only `tools` and run its
    /// `verify_signature` over `file` and `sig`, returning its three-way status:
    /// 0 verified, 1 the signature is wrong, 2 nothing here can check one.
    ///
    /// `VERIFIER_EXTRA_PATHS` is emptied rather than left alone: it exists to
    /// find a verifier the machine running this test may well have installed,
    /// and a test that says "python3 and nothing else" has to mean it.
    fn verify_signature_status(
        dir: &Path,
        tools: &[&str],
        key_line: &str,
        file: &Path,
        sig: &Path,
    ) -> i32 {
        let bin = host_with(dir, tools);
        let script = format!(
            r#"set -u
source '{installer}'
set +e
VERIFIER_EXTRA_PATHS=''
WIZARD_RELEASE_PUBKEY='{key_line}'
verify_signature '{file}' '{sig}'
printf 'STATUS=%s\n' "$?"
"#,
            installer = install_sh().display(),
            file = file.display(),
            sig = sig.display(),
        );
        let out = std::process::Command::new(bin.join("bash"))
            .arg("-c")
            .arg(&script)
            .env("PATH", &bin)
            .env("HOME", dir)
            .env("WIZARD_SELFTEST", "1")
            .current_dir(dir)
            .output()
            .expect("run bash");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        stdout
            .lines()
            .find_map(|line| line.strip_prefix("STATUS="))
            .unwrap_or_else(|| panic!("verify_signature printed no status:\n{stdout}\n{stderr}"))
            .trim()
            .parse()
            .expect("a numeric status")
    }

    /// A minted release: the public key line install.sh should carry, the
    /// signed `checksums.txt`, and its `.minisig`, written into `dir`.
    fn minted_release(dir: &Path, seed: u8) -> (String, PathBuf, PathBuf) {
        let (signing, id, public) = test_key(seed);
        let key_line = public.lines().nth(1).expect("key line").to_string();
        let checksums = b"d3adbeef  wizard-x86_64-unknown-linux-gnu.tar.gz\n";
        let signature = sign_release(
            &signing,
            id,
            b"ED",
            checksums,
            "wizard v9.9.9 checksums, signed by the wizard release key",
        );
        let file = dir.join("checksums.txt");
        let sig = dir.join("checksums.txt.minisig");
        std::fs::write(&file, checksums).expect("write checksums");
        std::fs::write(&sig, signature).expect("write signature");
        (key_line, file, sig)
    }

    #[test]
    fn install_sh_verifies_a_release_with_python_and_nothing_else() {
        // The macOS case, and the reason this path exists: a host with no
        // minisign and no openssl that can do ed25519 and blake2b used to have
        // no way to install at all — install.sh refused, correctly, and left
        // the user with a Homebrew detour before they could run one command.
        // Nothing about the check is relaxed here: the same two signatures over
        // the same bytes, and every doctored release below is still refused.
        let Some(_) = which("python3") else {
            eprintln!("no python3 on PATH; skipping the python-only verifier test");
            return;
        };
        let tmp = TempDir::new();
        let dir = tmp.0.as_path();
        let (key_line, file, sig) = minted_release(dir, 61);

        assert_eq!(
            verify_signature_status(dir, &["python3"], &key_line, &file, &sig),
            0,
            "a genuine signature must verify with python3 alone"
        );

        // The payload edited after signing.
        let tampered = dir.join("tampered.txt");
        std::fs::write(
            &tampered,
            b"eeeeeeee  wizard-x86_64-unknown-linux-gnu.tar.gz\n",
        )
        .expect("write tampered");
        assert_eq!(
            verify_signature_status(dir, &["python3"], &key_line, &tampered, &sig),
            1
        );

        // A signature by another key, which is what a release signed by anyone
        // else looks like from here.
        let other = dir.join("other");
        std::fs::create_dir_all(&other).expect("scratch dir for a second key");
        let (other_line, _, _) = minted_release(&other, 62);
        assert_eq!(
            verify_signature_status(dir, &["python3"], &other_line, &file, &sig),
            1
        );

        // The trusted comment rewritten after signing: it is inside the signed
        // envelope, and require_signature_names_tag reads it to decide which
        // release these bytes are, so an unchecked one is a signed downgrade.
        let doctored = dir.join("doctored.minisig");
        let original = std::fs::read_to_string(&sig).expect("read signature");
        let mut lines: Vec<&str> = original.lines().collect();
        lines[2] = "trusted comment: wizard v1.0.0 checksums, signed by the wizard release key";
        std::fs::write(&doctored, lines.join("\n") + "\n").expect("write doctored");
        assert_eq!(
            verify_signature_status(dir, &["python3"], &key_line, &file, &doctored),
            1,
            "the trusted comment is signed data and has to be checked"
        );
    }

    #[test]
    fn install_sh_refuses_when_the_host_can_check_nothing() {
        // The remaining honest outcome. A host with no checker must still get
        // status 2 — the one the caller turns into "no way to check the release
        // signature on this host", rather than a silent install.
        let tmp = TempDir::new();
        let dir = tmp.0.as_path();
        let (key_line, file, sig) = minted_release(dir, 63);
        assert_eq!(
            verify_signature_status(dir, &[], &key_line, &file, &sig),
            2,
            "a host with nothing to verify with must refuse, not install"
        );
    }

    #[test]
    fn install_sh_uses_openssl_only_when_that_openssl_can_actually_verify() {
        // Whether this asserts 0 or 2 depends on the machine, and both are the
        // property under test: an openssl that has ed25519 and blake2b verifies
        // the release, and one that does not (LibreSSL, which is what a Mac
        // calls `openssl`) is passed over rather than reported as a bad
        // signature. The probe decides, so this test runs everywhere.
        let Some(openssl) = which("openssl") else {
            eprintln!("no openssl on PATH; skipping the openssl verifier test");
            return;
        };
        let tmp = TempDir::new();
        let dir = tmp.0.as_path();
        let (key_line, file, sig) = minted_release(dir, 64);

        let capable = std::process::Command::new(&openssl)
            .args(["dgst", "-blake2b512"])
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|out| out.status.success())
            && std::process::Command::new(&openssl)
                .args(["pkeyutl", "-help"])
                .output()
                .is_ok_and(|out| {
                    String::from_utf8_lossy(&out.stdout).contains("-rawin")
                        || String::from_utf8_lossy(&out.stderr).contains("-rawin")
                });

        let status = verify_signature_status(dir, &["openssl"], &key_line, &file, &sig);
        if capable {
            assert_eq!(status, 0, "a capable openssl must verify the release");
            let tampered = dir.join("tampered.txt");
            std::fs::write(&tampered, b"eeeeeeee  wizard.tar.gz\n").expect("write tampered");
            assert_eq!(
                verify_signature_status(dir, &["openssl"], &key_line, &tampered, &sig),
                1
            );
        } else {
            assert_eq!(
                status, 2,
                "an openssl that cannot do this must be passed over, not trusted"
            );
        }
    }

    /// File names directly inside `dir`, for leftover-scratch assertions.
    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }
}
