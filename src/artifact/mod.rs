//! Local artifacts: the `Artifact` tool and `/artifacts` without claude.ai.
//!
//! Claude Code's `Artifact` publishes an HTML page to claude.ai and hands back
//! a hosted link. Wizard has no such host, so a page is written under
//! `~/.wizard/artifacts/<slug>/` and the link is a `file://` URL (or the
//! localhost one from `wizard artifact serve`). Nothing leaves the machine and
//! nobody else can open the link; every reply says so.
//!
//! The input schema is Claude Code's, so a prompt or skill written for it
//! keeps working. What a local store cannot honour is named in the reply
//! instead of being dropped: runtime capabilities, design systems, artifact
//! types, comments and the shared database all need the hosted service.

mod publish;
pub mod serve;

use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Port `wizard artifact serve` listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 8765;

/// The page itself inside an artifact directory.
pub const PAGE: &str = "index.html";

/// Per-artifact record, kept beside the files and never served.
pub const META: &str = ".artifact.json";

/// Limits Claude Code documents for a published artifact.
pub const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_BINARY_BYTES: u64 = 15 * 1024 * 1024;
pub const MAX_FILES_PER_PUBLISH: usize = 255;
pub const MAX_BYTES_PER_PUBLISH: u64 = 64 * 1024 * 1024;
pub const MAX_FILES_PER_ARTIFACT: usize = 511;
pub const MAX_BYTES_PER_ARTIFACT: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub label: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub version: u32,
    #[serde(default)]
    pub pinned: bool,
}

/// A directory of artifacts.
#[derive(Debug, Clone)]
pub struct Artifacts {
    root: PathBuf,
    /// Whether `open` may start the system browser.
    launch: bool,
}

impl Artifacts {
    /// `~/.wizard/artifacts`.
    pub fn standard() -> Result<Self, String> {
        let dir = crate::config::Config::wizard_dir().map_err(|err| format!("{err:#}"))?;
        Ok(Self {
            root: dir.join("artifacts"),
            launch: true,
        })
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            launch: false,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self, slug: &str) -> PathBuf {
        self.root.join(slug)
    }

    pub fn file_url(&self, slug: &str) -> String {
        format!("file://{}", self.dir(slug).join(PAGE).display())
    }

    pub fn served_url(&self, slug: &str) -> String {
        format!("http://127.0.0.1:{DEFAULT_PORT}/{slug}/")
    }

    pub fn meta(&self, slug: &str) -> Option<Meta> {
        let raw = std::fs::read_to_string(self.dir(slug).join(META)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn write_meta(&self, meta: &Meta) -> Result<(), String> {
        let path = self.dir(&meta.slug).join(META);
        let text = serde_json::to_string_pretty(meta).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Every artifact, pinned first, then newest.
    pub fn all(&self) -> Vec<Meta> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut metas: Vec<Meta> = entries
            .filter_map(Result::ok)
            .filter_map(|e| self.meta(&e.file_name().to_string_lossy()))
            .collect();
        metas.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.updated.cmp(&a.updated))
                .then(a.slug.cmp(&b.slug))
        });
        metas
    }

    /// The slug a `url` argument names: a `file://` or served URL, a bare
    /// slug, or a path inside the store.
    pub fn slug_of(&self, url: &str) -> Result<String, String> {
        let url = url.trim();
        if url.contains("claude.ai") {
            return Err(format!(
                "{url} is a hosted claude.ai link. Wizard's Artifact tool is local: it only knows \
                 artifacts under {}. Use action \"list\" for their URLs.",
                self.root.display()
            ));
        }
        let path = if let Some(rest) = url.strip_prefix("file://") {
            let rest = crate::minihttp::decode_path(rest).unwrap_or_else(|| rest.to_string());
            match Path::new(&rest).strip_prefix(&self.root) {
                Ok(inside) => inside.to_string_lossy().into_owned(),
                Err(_) => return Err(format!("{url} is not inside {}", self.root.display())),
            }
        } else if let Some(rest) = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
        {
            let path = rest.split_once('/').map_or("", |(_, p)| p);
            crate::minihttp::decode_path(path).unwrap_or_else(|| path.to_string())
        } else {
            url.to_string()
        };
        let slug = path
            .trim_start_matches('/')
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        if !valid_slug(slug) {
            return Err(format!(
                "'{url}' does not name a local artifact; use action \"list\" for the URLs"
            ));
        }
        Ok(slug.to_string())
    }

    fn existing(&self, url: &str) -> Result<Meta, String> {
        let slug = self.slug_of(url)?;
        self.meta(&slug).ok_or_else(|| {
            format!("no local artifact '{slug}' under {}; use action \"list\"", self.root.display())
        })
    }

    /// Run one `Artifact` call.
    pub fn call(&self, input: &Value, cwd: &Path) -> Result<String, String> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("publish");
        // Claude Code 2.1.314 also spells some calls as their own actions.
        // They mean what the scope and `asset` forms mean.
        let mapped;
        let (action, input) = match action {
            "list_files" | "list_assets" | "list_types" | "describe_type" => {
                let scope = match action {
                    "list_files" => "files",
                    "list_assets" => "assets",
                    _ => "types",
                };
                mapped = with(input, &[("action", "list"), ("scope", scope)]);
                ("list", &mapped)
            }
            "read_file" | "read_asset" => {
                mapped = with(input, &[("action", "read")]);
                ("read", &mapped)
            }
            "delete_asset" => {
                mapped = with(input, &[("action", "delete")]);
                ("delete", &mapped)
            }
            "upload_asset" | "copy_from" => {
                mapped = with(input, &[("action", "publish")]);
                let mut mapped = mapped;
                mapped["asset"] = Value::Bool(true);
                return publish::publish(self, &mapped, cwd);
            }
            "watch" | "unwatch" | "status" => {
                return Ok("Nothing to watch: a local artifact is a file on this machine. Read it again \
                           with action \"read\" to see changes."
                    .to_string());
            }
            other => (other, input),
        };
        match action {
            "publish" => publish::publish(self, input, cwd),
            "list" => self.list(input),
            "read" => self.read(input),
            "delete" => self.delete(input),
            "open" => self.open(input),
            "pin" => self.pin(input, true),
            "unpin" => self.pin(input, false),
            "quickstart" => Ok(quickstart()),
            other => Err(format!(
                "Unknown action '{other}'. Expected publish, list, read, delete, open, pin, unpin or quickstart."
            )),
        }
    }

    fn list(&self, input: &Value) -> Result<String, String> {
        let scope = input.get("scope").and_then(Value::as_str).unwrap_or("mine");
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(25, |n| n.clamp(1, 200) as usize);
        match scope {
            "mine" | "all" => {
                let metas = self.all();
                if metas.is_empty() {
                    return Ok(format!(
                        "No local artifacts yet (they are kept under {}).",
                        self.root.display()
                    ));
                }
                let mut out = format!(
                    "{} local artifact(s) under {} (local files, not hosted):\n",
                    metas.len(),
                    self.root.display()
                );
                for meta in metas.iter().take(limit) {
                    out.push_str(&format!(
                        "- {} (mine{}) v{} updated {}\n  {}\n  {}\n",
                        meta.title,
                        if meta.pinned { ", pinned" } else { "" },
                        meta.version,
                        meta.updated.format("%Y-%m-%d %H:%M UTC"),
                        self.file_url(&meta.slug),
                        self.served_url(&meta.slug),
                    ));
                }
                if metas.len() > limit {
                    out.push_str(&format!("... {} more\n", metas.len() - limit));
                }
                out.push_str("The second URL works while `wizard artifact serve` is running.");
                Ok(out)
            }
            "shared" => Ok("Nothing is shared with you: local artifacts have no sharing. \
                 Copy the artifact's directory to give someone else a copy."
                .to_string()),
            "types" => Ok("No artifact types: they are claude.ai templates. A local artifact is \
                 one HTML page plus its files."
                .to_string()),
            "files" => {
                let meta = self.existing(url_of(input)?)?;
                Ok(self.files_listing(&meta.slug))
            }
            "assets" => {
                let meta = self.existing(url_of(input)?)?;
                let assets = publish::list_assets(self, &meta.slug);
                if assets.is_empty() {
                    return Ok(format!("No assets uploaded to {}.", meta.slug));
                }
                Ok(assets
                    .iter()
                    .map(|(id, name, bytes)| format!("{id}  {bytes} bytes  {name}"))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            other => Err(format!(
                "scope must be mine, shared, all, types, files or assets, got '{other}'"
            )),
        }
    }

    fn files_listing(&self, slug: &str) -> String {
        let mut rows: Vec<(String, u64)> = Vec::new();
        collect_files(&self.dir(slug), &self.dir(slug), &mut rows);
        rows.sort();
        let mut out = format!("Files of {slug}:\n");
        for (path, bytes) in rows {
            out.push_str(&format!("{path}  {bytes} bytes\n"));
        }
        out
    }

    fn read(&self, input: &Value) -> Result<String, String> {
        let meta = self.existing(url_of(input)?)?;
        let dir = self.dir(&meta.slug);
        let mut wanted: Vec<String> = Vec::new();
        if let Some(path) = input.get("path").and_then(Value::as_str) {
            wanted.push(path.to_string());
        }
        if let Some(paths) = input.get("paths").and_then(Value::as_array) {
            wanted.extend(paths.iter().filter_map(Value::as_str).map(str::to_string));
        }
        if wanted.is_empty() {
            let page = std::fs::read_to_string(dir.join(PAGE))
                .map_err(|e| format!("{}: {e}", dir.join(PAGE).display()))?;
            return Ok(format!(
                "Local artifact {} (\"{}\"), version {}, yours to edit; the page is {}.\n\n{page}",
                meta.slug,
                meta.title,
                meta.version,
                dir.join(PAGE).display()
            ));
        }
        let mut out = String::new();
        for path in wanted {
            let file = match publish::asset_path(&dir, &path) {
                Some(asset) => asset,
                None => dir.join(clean_rel(&path)?),
            };
            match std::fs::read(&file) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(text) if text.len() <= 64 * 1024 => {
                        out.push_str(&format!("{} ({}):\n{text}\n", path, file.display()));
                    }
                    Ok(text) => out.push_str(&format!(
                        "{path}: {} bytes, saved at {}\n",
                        text.len(),
                        file.display()
                    )),
                    Err(err) => out.push_str(&format!(
                        "{path}: {} bytes (binary), saved at {}\n",
                        err.into_bytes().len(),
                        file.display()
                    )),
                },
                Err(err) => out.push_str(&format!("{path}: {err}\n")),
            }
        }
        Ok(out)
    }

    fn delete(&self, input: &Value) -> Result<String, String> {
        let meta = self.existing(url_of(input)?)?;
        let dir = self.dir(&meta.slug);
        if let Some(path) = input.get("path").and_then(Value::as_str) {
            let file = match publish::asset_path(&dir, path) {
                Some(asset) => asset,
                None => dir.join(clean_rel(path)?),
            };
            if path == PAGE {
                return Err("index.html is the page; delete the whole artifact instead".into());
            }
            std::fs::remove_file(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            return Ok(format!("Removed {path} from {}.", meta.slug));
        }
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(format!(
            "Deleted local artifact {} ({}). The file:// link no longer works.",
            meta.slug,
            dir.display()
        ))
    }

    fn open(&self, input: &Value) -> Result<String, String> {
        let meta = self.existing(url_of(input)?)?;
        let url = self.file_url(&meta.slug);
        let started = self.launch && launch_browser(&url);
        Ok(format!(
            "{} {url}\nServed copy (needs `wizard artifact serve`): {}",
            if started {
                "Opened in the default browser:"
            } else {
                "Open this in a browser:"
            },
            self.served_url(&meta.slug)
        ))
    }

    fn pin(&self, input: &Value, pinned: bool) -> Result<String, String> {
        let mut meta = self.existing(url_of(input)?)?;
        meta.pinned = pinned;
        self.write_meta(&meta)?;
        Ok(format!(
            "{} {} in the local list (/artifacts shows pinned ones first).",
            if pinned { "Pinned" } else { "Unpinned" },
            meta.slug
        ))
    }
}

fn with(input: &Value, set: &[(&str, &str)]) -> Value {
    let mut out = input.clone();
    if let Some(map) = out.as_object_mut() {
        for (k, v) in set {
            map.insert((*k).to_string(), Value::String((*v).to_string()));
        }
    }
    out
}

fn url_of(input: &Value) -> Result<&str, String> {
    input
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "url is required for this action".to_string())
}

pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 80
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A published path made safe: relative, no `..`, no reserved names.
pub fn clean_rel(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() || path.len() > 512 || path.contains('\0') {
        return Err(format!("'{path}' is not a usable published path"));
    }
    let mut out = PathBuf::new();
    for part in Path::new(path).components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "'{path}' must be relative with no leading slash and no '..'"
                ));
            }
        }
    }
    if out.as_os_str().is_empty() || out.file_name().is_some_and(|n| n == META) {
        return Err(format!("'{path}' is reserved"));
    }
    Ok(out)
}

fn collect_files(base: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, out);
        } else if entry.file_name() != META
            && let Ok(meta) = entry.metadata()
        {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            out.push((rel.display().to_string(), meta.len()));
        }
    }
}

/// Best-effort: start the system opener. Only where a display exists.
fn launch_browser(url: &str) -> bool {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        "xdg-open"
    } else {
        return false;
    };
    std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

fn quickstart() -> String {
    "Wizard's Artifact tool is the local counterpart of Claude Code's: it writes your HTML page \
     and its files under ~/.wizard/artifacts/<slug>/ and returns a file:// URL. Nothing is \
     uploaded, there are no artifact types, design systems, runtime capabilities or comments, \
     and no one else can open the link.\n\
     \n\
     Write a single self-contained .html file (inline CSS and JS, a <title> of two to four \
     words, light and dark colors, readable at phone width), then call Artifact with \
     file_path. To change it later, call again with the same file_path and url. Put images, \
     styles and scripts in `files`, or upload them with asset: true. Scripts and styles may \
     load from a CDN only if the viewer is online. Keep each page at 16MB or less."
        .to_string()
}

#[cfg(test)]
mod tests;
