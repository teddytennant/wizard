//! `Artifact` publish and asset upload on the local store.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::Value;

use super::{
    Artifacts, MAX_BINARY_BYTES, MAX_BYTES_PER_ARTIFACT, MAX_BYTES_PER_PUBLISH,
    MAX_FILES_PER_ARTIFACT, MAX_FILES_PER_PUBLISH, MAX_TEXT_BYTES, Meta, PAGE, clean_rel,
    collect_files,
};

/// One entry of the `files` argument.
enum Source {
    Disk(PathBuf),
    Artifact { slug: String, path: PathBuf },
    Remove,
}

fn str_of<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

fn resolve(path: &str, base: &Path) -> PathBuf {
    let expanded = shellexpand::tilde(path).into_owned();
    let path = Path::new(&expanded);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn limit_of(bytes: &[u8]) -> (u64, &'static str) {
    if std::str::from_utf8(bytes).is_ok() {
        (MAX_TEXT_BYTES, "text file")
    } else {
        (MAX_BINARY_BYTES, "binary file")
    }
}

fn check_size(name: &str, bytes: &[u8]) -> Result<(), String> {
    let (limit, kind) = limit_of(bytes);
    if bytes.len() as u64 > limit {
        return Err(format!(
            "{name} is {} bytes; a {kind} may be at most {limit} bytes",
            bytes.len()
        ));
    }
    Ok(())
}

/// The text between the first `<title>` and `</title>`.
fn html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = lower[start..].find('>')? + start + 1;
    let end = lower[open_end..].find("</title>")? + open_end;
    let title = html[open_end..end].trim();
    (!title.is_empty()).then(|| title.chars().take(120).collect())
}

fn slugify(title: &str) -> String {
    let mut slug = String::new();
    for c in title.to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_end_matches('-').chars().take(40).collect();
    if slug.is_empty() {
        "artifact".to_string()
    } else {
        slug.trim_end_matches('-').to_string()
    }
}

fn fresh_slug(store: &Artifacts, title: &str) -> String {
    let base = slugify(title);
    loop {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let slug = format!("{base}-{}", &suffix[..6]);
        if !store.dir(&slug).exists() {
            return slug;
        }
    }
}

/// Apply `edits` (old_str/new_str pairs) to `text`, all or none.
fn apply_edits(mut text: String, edits: &[Value]) -> Result<String, String> {
    for (i, edit) in edits.iter().enumerate() {
        let (Some(old), Some(new)) = (str_of(edit, "old_str"), str_of(edit, "new_str")) else {
            return Err(format!("edit {} needs old_str and new_str", i + 1));
        };
        match text.matches(old).count() {
            1 => text = text.replacen(old, new, 1),
            0 => {
                return Err(format!(
                    "edit {} failed: old_str was not found in the file. Nothing was changed or published.",
                    i + 1
                ));
            }
            n => {
                return Err(format!(
                    "edit {} failed: old_str occurs {n} times; it must occur exactly once. Nothing was changed or published.",
                    i + 1
                ));
            }
        }
    }
    Ok(text)
}

fn parse_files(
    store: &Artifacts,
    input: &Value,
    base: &Path,
) -> Result<Vec<(PathBuf, Source)>, String> {
    let mut out = Vec::new();
    match input.get("files") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for item in items {
                let path = match item {
                    Value::String(path) => path.as_str(),
                    other => str_of(other, "path").ok_or("each files entry needs a path")?,
                };
                out.push((clean_rel(path)?, Source::Disk(resolve(path, base))));
            }
        }
        Some(Value::Object(map)) => {
            for (published, source) in map {
                let rel = clean_rel(published)?;
                let source = match source {
                    Value::Null => Source::Remove,
                    Value::String(from) => Source::Disk(resolve(from, base)),
                    Value::Object(obj) => {
                        if let Some(from) = obj.get("from").and_then(Value::as_str) {
                            Source::Disk(resolve(from, base))
                        } else if let (Some(url), Some(path)) = (
                            obj.get("artifact").and_then(Value::as_str),
                            obj.get("path").and_then(Value::as_str),
                        ) {
                            Source::Artifact {
                                slug: store.slug_of(url)?,
                                path: clean_rel(path)?,
                            }
                        } else {
                            return Err(format!(
                                "files['{published}'] needs `from`, or `artifact` and `path`"
                            ));
                        }
                    }
                    _ => return Err(format!("files['{published}'] has an unusable source")),
                };
                out.push((rel, source));
            }
        }
        Some(_) => return Err("files must be a list or a {published: source} map".into()),
    }
    Ok(out)
}

pub(super) fn publish(store: &Artifacts, input: &Value, cwd: &Path) -> Result<String, String> {
    if input.get("type_url").is_some() {
        return Err(
            "type_url names a claude.ai Artifact type. Local artifacts have no types; \
                    publish a page with file_path instead."
                .into(),
        );
    }
    if input.get("asset").and_then(Value::as_bool) == Some(true) {
        return upload_assets(store, input, cwd);
    }
    let Some(file_path) = str_of(input, "file_path") else {
        return Err("file_path is required: the .html file to publish".into());
    };
    let page_path = resolve(file_path, cwd);
    let base = match str_of(input, "root") {
        Some(root) => resolve(root, cwd),
        None => cwd.to_path_buf(),
    };

    let existing = match str_of(input, "url") {
        Some(url) => Some(store.existing(url)?),
        None => None,
    };

    let mut html =
        std::fs::read_to_string(&page_path).map_err(|e| format!("{}: {e}", page_path.display()))?;
    let edited = match input.get("edits").and_then(Value::as_array) {
        Some(edits) if !edits.is_empty() => {
            html = apply_edits(html, edits)?;
            true
        }
        _ => false,
    };
    check_size(file_path, html.as_bytes())?;

    let mut staged: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    let mut added_bytes = html.len() as u64;
    for (rel, source) in parse_files(store, input, &base)? {
        if rel == Path::new(PAGE) {
            return Err("index.html is the page itself; pass it as file_path, not in files".into());
        }
        let bytes = match source {
            Source::Remove => {
                staged.push((rel, None));
                continue;
            }
            Source::Disk(from) => {
                std::fs::read(&from).map_err(|e| format!("{}: {e}", from.display()))?
            }
            Source::Artifact { slug, path } => {
                let from = store.dir(&slug).join(&path);
                std::fs::read(&from).map_err(|e| format!("{}: {e}", from.display()))?
            }
        };
        check_size(&rel.display().to_string(), &bytes)?;
        if rel == Path::new("preflight.js") && bytes.len() > 8 * 1024 {
            return Err("preflight.js may be at most 8 KiB".into());
        }
        added_bytes += bytes.len() as u64;
        staged.push((rel, Some(bytes)));
    }
    let publishing = 1 + staged.iter().filter(|(_, b)| b.is_some()).count();
    if publishing > MAX_FILES_PER_PUBLISH {
        return Err(format!(
            "one publish may send at most {MAX_FILES_PER_PUBLISH} files, this sends {publishing}"
        ));
    }
    if added_bytes > MAX_BYTES_PER_PUBLISH {
        return Err(format!(
            "one publish may send at most {MAX_BYTES_PER_PUBLISH} bytes, this sends {added_bytes}"
        ));
    }

    let title = html_title(&html)
        .or_else(|| str_of(input, "title").map(str::to_string))
        .or_else(|| existing.as_ref().map(|m| m.title.clone()))
        .unwrap_or_else(|| {
            page_path
                .file_stem()
                .map_or("Artifact".into(), |s| s.to_string_lossy().into_owned())
        });
    let slug = existing
        .as_ref()
        .map_or_else(|| fresh_slug(store, &title), |m| m.slug.clone());
    let dir = store.dir(&slug);

    // Totals after this publish: what is there, minus removals, plus additions.
    let mut have: Vec<(String, u64)> = Vec::new();
    collect_files(&dir, &dir, &mut have);
    for (rel, bytes) in &staged {
        let name = rel.display().to_string();
        have.retain(|(n, _)| *n != name);
        if let Some(bytes) = bytes {
            have.push((name, bytes.len() as u64));
        }
    }
    have.retain(|(n, _)| n != PAGE);
    have.push((PAGE.to_string(), html.len() as u64));
    let total: u64 = have.iter().map(|(_, b)| b).sum();
    if have.len() > MAX_FILES_PER_ARTIFACT || total > MAX_BYTES_PER_ARTIFACT {
        return Err(format!(
            "an artifact may hold at most {MAX_FILES_PER_ARTIFACT} files and {MAX_BYTES_PER_ARTIFACT} bytes; this would make {} files, {total} bytes",
            have.len()
        ));
    }

    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    write_file(&dir.join(PAGE), html.as_bytes())?;
    for (rel, bytes) in &staged {
        let target = dir.join(rel);
        match bytes {
            Some(bytes) => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                write_file(&target, bytes)?;
            }
            None => {
                let _ = std::fs::remove_file(&target);
            }
        }
    }
    if edited {
        let _ = std::fs::write(&page_path, &html);
    }

    let now = Utc::now();
    let mut meta = existing.unwrap_or_else(|| Meta {
        slug: slug.clone(),
        title: title.clone(),
        description: String::new(),
        icon: String::new(),
        label: String::new(),
        created: now,
        updated: now,
        version: 0,
        pinned: false,
    });
    meta.title = title;
    meta.updated = now;
    meta.version += 1;
    if let Some(d) = str_of(input, "description") {
        meta.description = d.to_string();
    }
    if let Some(i) = str_of(input, "icon") {
        meta.icon = i.to_string();
    }
    if let Some(l) = str_of(input, "label") {
        meta.label = l.to_string();
    }
    if input.get("pin").and_then(Value::as_bool) == Some(true) {
        meta.pinned = true;
    }
    store.write_meta(&meta)?;

    let mut out = format!(
        "Published locally, not hosted: {}\n\"{}\" is version {} of {} ({} files, {} bytes), stored in {}.\n\
         To view it over HTTP run `wizard artifact serve` and open {}. Only this machine can open these links.",
        store.file_url(&slug),
        meta.title,
        meta.version,
        slug,
        have.len(),
        total,
        dir.display(),
        store.served_url(&slug),
    );
    let ignored: Vec<&str> = [
        (
            "capabilities",
            input.get("capabilities").is_some_and(non_empty),
        ),
        ("contract", input.get("contract").is_some()),
        ("auto_open", input.get("auto_open").is_some()),
    ]
    .iter()
    .filter(|(_, set)| *set)
    .map(|(name, _)| *name)
    .collect();
    if !ignored.is_empty() {
        out.push_str(&format!(
            "\nNot applied locally: {}. Runtime capabilities (live data, shared state, viewer identity, Claude calls) need the hosted service; this page is static.",
            ignored.join(", ")
        ));
    }
    Ok(out)
}

fn non_empty(value: &Value) -> bool {
    match value {
        Value::Object(map) => !map.is_empty(),
        Value::Null => false,
        _ => true,
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/* ---------------------------------------------------------------------- */
/* Assets                                                                 */
/* ---------------------------------------------------------------------- */

fn assets_dir(dir: &Path) -> PathBuf {
    dir.join("assets")
}

/// `(id, file name, bytes)` for each uploaded asset.
pub(super) fn list_assets(store: &Artifacts, slug: &str) -> Vec<(String, String, u64)> {
    let Ok(entries) = std::fs::read_dir(assets_dir(&store.dir(slug))) else {
        return Vec::new();
    };
    let mut rows: Vec<(String, String, u64)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.split('.').next()?.to_string();
            is_asset_id(&id).then(|| (id, name, e.metadata().map_or(0, |m| m.len())))
        })
        .collect();
    rows.sort();
    rows
}

fn is_asset_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// The file an asset id names, if `path` is an id.
pub(super) fn asset_path(dir: &Path, path: &str) -> Option<PathBuf> {
    if !is_asset_id(path) {
        return None;
    }
    std::fs::read_dir(assets_dir(dir))
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(path))
        })
}

fn store_asset(dir: &Path, bytes: &[u8], ext: &str) -> Result<(String, String), String> {
    check_size("asset", bytes)?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let ext = ext
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    let name = if ext.is_empty() {
        id.clone()
    } else {
        format!("{id}.{ext}")
    };
    let folder = assets_dir(dir);
    std::fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    write_file(&folder.join(&name), bytes)?;
    Ok((id, format!("assets/{name}")))
}

fn upload_assets(store: &Artifacts, input: &Value, cwd: &Path) -> Result<String, String> {
    let meta = store.existing(super::url_of(input)?)?;
    let dir = store.dir(&meta.slug);
    let mut lines = Vec::new();

    if let Some(from) = str_of(input, "from_url") {
        let source = store.existing(from)?;
        let ids: Vec<&str> = input
            .get("asset_ids")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if ids.is_empty() || ids.len() > 10 {
            return Err("asset_ids must name 1 to 10 asset ids from the source".into());
        }
        for id in ids {
            let file = asset_path(&store.dir(&source.slug), id)
                .ok_or_else(|| format!("{} has no asset {id}", source.slug))?;
            let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let ext = file
                .extension()
                .map_or(String::new(), |e| e.to_string_lossy().into_owned());
            let (new_id, url) = store_asset(&dir, &bytes, &ext)?;
            lines.push(format!("{id} copied as {new_id}: url {url}"));
        }
    } else {
        let mut paths: Vec<String> = Vec::new();
        if let Some(p) = str_of(input, "file_path") {
            paths.push(p.to_string());
        }
        if let Some(list) = input.get("file_paths").and_then(Value::as_array) {
            paths.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
        }
        if paths.is_empty() || paths.len() > 25 {
            return Err("give file_path, or file_paths with 1 to 25 files".into());
        }
        for path in paths {
            let file = resolve(&path, cwd);
            let result = std::fs::read(&file)
                .map_err(|e| format!("{}: {e}", file.display()))
                .and_then(|bytes| {
                    let ext = file
                        .extension()
                        .map_or(String::new(), |e| e.to_string_lossy().into_owned());
                    store_asset(&dir, &bytes, &ext)
                });
            match result {
                Ok((id, url)) => lines.push(format!("{path}: id {id}, url {url}")),
                Err(err) => lines.push(format!("{path}: not uploaded: {err}")),
            }
        }
    }
    Ok(format!(
        "Stored in {} (local, not hosted). Reference each from the page by its url exactly as given; it is relative to the page, so it works over file:// and over `wizard artifact serve`.\n{}",
        assets_dir(&dir).display(),
        lines.join("\n")
    ))
}
