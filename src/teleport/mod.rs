//! `/teleport`: continue a session on another machine you own.
//!
//! Claude Code's teleport pulls a cloud session into the local terminal.
//! Wizard has no cloud sessions, so the same move goes machine to machine: a
//! session is one JSONL file under `~/.wizard/sessions/`, `pack` wraps it
//! (with the id and where it ran) into a small gzip envelope, and `pull`
//! unwraps it into this machine's sessions so `/resume` continues it.
//!
//! Neither `wizard sync` (config only, sessions are never bundled) nor the
//! mesh (a read-only live view of a running session) carries a session, so the
//! transport here is the one every machine already has: a file you copy, a URL,
//! or `ssh host wizard teleport pack`.
//!
//! A pulled session is conversation history the model will replay, so pull
//! only from machines and links you trust. The envelope is not signed.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};

/// Largest envelope, packed or unpacked, that `pull` will handle.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub teleport: u32,
    pub id: String,
    pub host: String,
    pub cwd: String,
    pub packed_at: String,
    pub session: String,
}

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_string())
        })
        .unwrap_or_else(|| "unknown-host".into())
}

/// The id of the newest session in `dir`, optionally only ones recorded under
/// `cwd`.
pub fn latest(dir: &Path, cwd: Option<&Path>) -> Option<String> {
    let want = cwd.map(|c| c.display().to_string());
    let mut best: Option<String> = None;
    for entry in std::fs::read_dir(dir).ok()?.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if let Some(want) = &want
            && header_cwd(&path).as_deref() != Some(want.as_str())
        {
            continue;
        }
        let id = path.file_stem()?.to_string_lossy().into_owned();
        if best.as_ref().is_none_or(|b| id > *b) {
            best = Some(id);
        }
    }
    best
}

fn header_cwd(path: &Path) -> Option<String> {
    let first = std::fs::read_to_string(path).ok()?.lines().next()?.to_string();
    serde_json::from_str::<serde_json::Value>(&first)
        .ok()?
        .get("cwd")?
        .as_str()
        .map(str::to_string)
}

/// Wrap session `id` from `dir` as gzip bytes.
pub fn pack(dir: &Path, id: &str) -> Result<Vec<u8>> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        bail!("'{id}' is not a session id");
    }
    let path = dir.join(format!("{id}.jsonl"));
    let session = std::fs::read_to_string(&path)
        .with_context(|| format!("no session '{id}' in {}", dir.display()))?;
    if session.len() as u64 > MAX_BYTES {
        bail!("session '{id}' is larger than {MAX_BYTES} bytes");
    }
    let envelope = Envelope {
        teleport: 1,
        id: id.to_string(),
        host: host_name(),
        cwd: header_cwd(&path).unwrap_or_default(),
        packed_at: chrono::Utc::now().to_rfc3339(),
        session,
    };
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&serde_json::to_vec(&envelope)?)?;
    Ok(gz.finish()?)
}

/// Open an envelope and check that what it carries is a session.
pub fn unpack(bytes: &[u8]) -> Result<Envelope> {
    if bytes.len() as u64 > MAX_BYTES {
        bail!("the envelope is larger than {MAX_BYTES} bytes");
    }
    let mut raw = Vec::new();
    GzDecoder::new(bytes)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut raw)
        .context("this is not a Wizard teleport envelope (gzip)")?;
    if raw.len() as u64 > MAX_BYTES {
        bail!("the envelope unpacks to more than {MAX_BYTES} bytes");
    }
    let envelope: Envelope =
        serde_json::from_slice(&raw).context("this is not a Wizard teleport envelope (json)")?;
    if envelope.teleport != 1 {
        bail!("unknown teleport envelope version {}", envelope.teleport);
    }
    if envelope.id.is_empty()
        || !envelope
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("the envelope's session id is not usable");
    }
    for (n, line) in envelope.session.lines().enumerate() {
        if !line.trim().is_empty() && serde_json::from_str::<serde_json::Value>(line).is_err() {
            bail!("line {} of the session is not JSON; refusing to import it", n + 1);
        }
    }
    Ok(envelope)
}

/// Write a session into `dir`, pointing its recorded directory at `cwd` so
/// `/resume` here finds it, under a free id. Returns that id.
pub fn import(envelope: &Envelope, dir: &Path, cwd: &Path) -> Result<String> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut id = envelope.id.clone();
    let mut n = 1;
    while dir.join(format!("{id}.jsonl")).exists() {
        n += 1;
        id = format!("{}-tp{n}", envelope.id);
    }
    let mut lines: Vec<String> = envelope.session.lines().map(str::to_string).collect();
    if let Some(first) = lines.first_mut()
        && let Ok(mut header) = serde_json::from_str::<serde_json::Value>(first)
        && header.get("cwd").is_some()
    {
        header["cwd"] = serde_json::Value::String(cwd.display().to_string());
        *first = header.to_string();
    }
    let path: PathBuf = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, format!("{}\n", lines.join("\n")))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

/// Where a pull reads from.
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Url(String),
    /// `[user@]host` and the session id (`latest` when none).
    Ssh { host: String, id: String },
}

pub fn parse_source(text: &str) -> Source {
    let text = text.trim();
    if text.starts_with("http://") || text.starts_with("https://") {
        return Source::Url(text.to_string());
    }
    let expanded = PathBuf::from(shellexpand::tilde(text).into_owned());
    if expanded.exists() || text.contains('/') || text.ends_with(".gz") {
        return Source::File(expanded);
    }
    let (host, id) = text.split_once(':').unwrap_or((text, "latest"));
    Source::Ssh {
        host: host.to_string(),
        id: if id.is_empty() { "latest".into() } else { id.to_string() },
    }
}

/// Fetch the envelope bytes for `source`.
pub async fn fetch(source: &Source) -> Result<Vec<u8>> {
    match source {
        Source::File(path) => {
            let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
            if meta.len() > MAX_BYTES {
                bail!("{} is larger than {MAX_BYTES} bytes", path.display());
            }
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))
        }
        Source::Url(url) => {
            let response = reqwest::get(url).await.with_context(|| format!("fetching {url}"))?;
            if !response.status().is_success() {
                bail!("{url} answered HTTP {}", response.status());
            }
            if response.content_length().is_some_and(|n| n > MAX_BYTES) {
                bail!("{url} is larger than {MAX_BYTES} bytes");
            }
            Ok(response.bytes().await?.to_vec())
        }
        Source::Ssh { host, id } => {
            if host.starts_with('-') || id.starts_with('-') {
                bail!("'{host}' is not a host");
            }
            let output = tokio::process::Command::new("ssh")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--"])
                .arg(host)
                .args(["wizard", "teleport", "pack", id, "--out", "-"])
                .stdin(std::process::Stdio::null())
                .output()
                .await
                .context("running ssh (is an ssh client installed?)")?;
            if !output.status.success() {
                bail!(
                    "ssh {host} wizard teleport pack failed: {}. The other machine needs Wizard \
                     3.12 or later on its PATH for non-interactive ssh, and key-based login.",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(output.stdout)
        }
    }
}

/// `wizard teleport`.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum TeleportCmd {
    /// Write a session as a file (or `-` for stdout) to carry to another
    /// machine. With no id, the newest session recorded under this directory.
    Pack {
        id: Option<String>,
        #[arg(long, default_value = "-")]
        out: String,
    },
    /// Bring a session here: a file, an http(s) URL, or `[user@]host[:id]`
    /// over ssh. Then `wizard --resume <id>` continues it.
    Pull { source: String },
}

pub async fn run(cmd: TeleportCmd) -> Result<i32> {
    let sessions = crate::config::Config::sessions_dir()?;
    let cwd = std::env::current_dir()?;
    match cmd {
        TeleportCmd::Pack { id, out } => {
            let id = match id.as_deref() {
                None | Some("latest") => latest(&sessions, Some(&cwd))
                    .or_else(|| latest(&sessions, None))
                    .context("no sessions to pack")?,
                Some(id) => id.to_string(),
            };
            let bytes = pack(&sessions, &id)?;
            if out == "-" {
                std::io::stdout().write_all(&bytes)?;
            } else {
                std::fs::write(&out, &bytes).with_context(|| format!("writing {out}"))?;
                eprintln!("packed session {id} into {out} ({} bytes)", bytes.len());
            }
            Ok(0)
        }
        TeleportCmd::Pull { source } => {
            let bytes = fetch(&parse_source(&source)).await?;
            let envelope = unpack(&bytes)?;
            let id = import(&envelope, &sessions, &cwd)?;
            println!(
                "imported session {id} from {} (it ran in {}). Continue it with: wizard --resume {id}",
                envelope.host,
                if envelope.cwd.is_empty() { "an unknown directory" } else { &envelope.cwd }
            );
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests;
