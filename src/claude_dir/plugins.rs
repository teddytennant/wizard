//! Claude Code plugins and marketplaces.
//!
//! A plugin is a directory with `.claude-plugin/plugin.json` and any of
//! `commands/`, `skills/`, `agents/`, `output-styles/` and `.mcp.json`. A
//! marketplace is a directory (usually a git repository) with
//! `.claude-plugin/marketplace.json` listing plugins and where to fetch each.
//!
//! Three things make a plugin active, lowest priority first:
//!
//! 1. one Claude Code installed and enabled (`~/.claude/plugins/installed_plugins.json`
//!    plus `enabledPlugins` in its settings), read in place;
//! 2. one installed by `/plugin install`, kept under `~/.wizard/claude-plugins/installed/`;
//! 3. one given for this run with `--plugin-dir`.
//!
//! A plugin's hooks (`hooks/hooks.json`, or the manifest's `hooks` key) go to
//! the hooks engine like any other Claude-format hook; see [`hook_specs`].

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Plugin directories given with `--plugin-dir`, published by the flag as
/// `WIZARD_PLUGIN_DIRS` so subagent processes see them too.
fn session_dirs() -> Vec<PathBuf> {
    std::env::var_os(crate::cli_claude::env::PLUGIN_DIRS)
        .map(|joined| std::env::split_paths(&joined).collect())
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    pub name: String,
    pub root: PathBuf,
    pub version: Option<String>,
    pub description: Option<String>,
    /// `claude-code`, `installed` or `session`.
    pub origin: &'static str,
}

/// `~/.wizard/claude-plugins`.
pub fn store_dir() -> Option<PathBuf> {
    crate::config::Config::wizard_dir()
        .ok()
        .map(|dir| dir.join("claude-plugins"))
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join(".claude-plugin").join("plugin.json")
}

/// Read a plugin directory. The manifest is optional in Claude Code (the
/// directory name then names the plugin), so a directory with a recognised
/// component folder also counts.
pub fn read_plugin(root: &Path, origin: &'static str) -> Option<Plugin> {
    let manifest: Option<Value> = std::fs::read_to_string(manifest_path(root))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let has_component = ["commands", "skills", "agents", "output-styles"]
        .iter()
        .any(|leaf| root.join(leaf).is_dir())
        || root.join(".mcp.json").is_file()
        || root.join("hooks").join("hooks.json").is_file();
    if manifest.is_none() && !has_component {
        return None;
    }
    let field = |key: &str| {
        manifest
            .as_ref()
            .and_then(|m| m.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let name = field("name").or_else(|| root.file_name()?.to_str().map(str::to_string))?;
    Some(Plugin {
        name,
        root: root.to_path_buf(),
        version: field("version"),
        description: field("description"),
        origin,
    })
}

fn enabled_in_settings(project_root: &Path) -> std::collections::HashMap<String, bool> {
    let mut out = std::collections::HashMap::new();
    let mut files = Vec::new();
    if let Some(user) = super::user_dir() {
        files.push(user.join("settings.json"));
    }
    let project = super::project_dir(project_root);
    files.push(project.join("settings.json"));
    files.push(project.join("settings.local.json"));
    for file in files {
        let Some(json) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        else {
            continue;
        };
        if let Some(map) = json.get("enabledPlugins").and_then(Value::as_object) {
            for (key, value) in map {
                if let Some(on) = value.as_bool() {
                    out.insert(key.clone(), on);
                }
            }
        }
    }
    out
}

/// Install paths in a Claude Code `installed_plugins.json`, v1 and v2 shapes.
fn claude_installed_paths(json: &Value) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Some(plugins) = json.get("plugins").and_then(Value::as_object) else {
        return out;
    };
    for (key, entry) in plugins {
        let installs: Vec<&Value> = match entry {
            Value::Array(list) => list.iter().collect(),
            single => vec![single],
        };
        for install in installs {
            if let Some(path) = install.get("installPath").and_then(Value::as_str) {
                out.push((key.clone(), PathBuf::from(path)));
            }
        }
    }
    out
}

fn from_claude_code(project_root: &Path) -> Vec<Plugin> {
    let Some(user) = super::user_dir() else {
        return Vec::new();
    };
    let Some(json) = std::fs::read_to_string(user.join("plugins").join("installed_plugins.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    else {
        return Vec::new();
    };
    let enabled = enabled_in_settings(project_root);
    claude_installed_paths(&json)
        .into_iter()
        .filter(|(key, _)| enabled.get(key).copied().unwrap_or(false))
        .filter_map(|(_, path)| read_plugin(&path, "claude-code"))
        .collect()
}

fn from_store() -> Vec<Plugin> {
    let Some(dir) = store_dir().map(|dir| dir.join("installed")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut roots: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !p.join(".disabled").exists())
        .collect();
    roots.sort();
    roots
        .iter()
        .filter_map(|root| read_plugin(root, "installed"))
        .collect()
}

fn from_session() -> Vec<Plugin> {
    from_dirs(&session_dirs())
}

/// Plugins named directly: each directory is a plugin, or a folder of them.
fn from_dirs(dirs: &[PathBuf]) -> Vec<Plugin> {
    let mut out = Vec::new();
    for dir in dirs {
        if let Some(plugin) = read_plugin(dir, "session")
            && (manifest_path(dir).is_file() || has_components(dir))
        {
            out.push(plugin);
            continue;
        }
        // A folder of plugins loads each child.
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        out.extend(children.iter().filter_map(|c| read_plugin(c, "session")));
    }
    out
}

fn has_components(root: &Path) -> bool {
    ["commands", "skills", "agents", "output-styles"]
        .iter()
        .any(|leaf| root.join(leaf).is_dir())
}

/// Every active plugin; a later source replaces an earlier plugin of the same
/// name.
pub fn enabled(project_root: &Path) -> Vec<Plugin> {
    let mut plugins: Vec<Plugin> = Vec::new();
    // `--bare` loads only what the command line names.
    let installed = if super::bare() {
        Vec::new()
    } else {
        from_claude_code(project_root)
            .into_iter()
            .chain(from_store())
            .collect()
    };
    for plugin in installed.into_iter().chain(from_session()) {
        plugins.retain(|have| have.name != plugin.name);
        plugins.push(plugin);
    }
    plugins
}

/// `<plugin>/<leaf>` for every active plugin that has one.
pub fn component_dirs(project_root: &Path, leaf: &str) -> Vec<PathBuf> {
    enabled(project_root)
        .into_iter()
        .map(|plugin| plugin.root.join(leaf))
        .filter(|dir| dir.is_dir())
        .collect()
}

/// Plugin commands as `(plugin:name, file)`.
pub fn commands(project_root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for plugin in enabled(project_root) {
        for (name, path) in super::namespaced_markdown(&plugin.root.join("commands")) {
            out.push((format!("{}:{name}", plugin.name), path));
        }
    }
    out
}

/// The hooks one plugin declares: `hooks/hooks.json`, or the manifest's
/// `hooks` key (a path inside the plugin, or the hooks object itself).
/// `${CLAUDE_PLUGIN_ROOT}` in a command, its arguments or a URL is the plugin
/// directory.
pub fn plugin_hooks(plugin: &Plugin) -> Vec<crate::hooks::HookSpec> {
    let manifest: Option<Value> = std::fs::read_to_string(manifest_path(&plugin.root))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let declared = manifest.as_ref().and_then(|m| m.get("hooks")).cloned();
    let document = match declared {
        Some(Value::String(rel)) => read_json(&plugin.root.join(rel.trim_start_matches("./"))),
        Some(inline @ Value::Object(_)) => Some(inline),
        _ => None,
    }
    .or_else(|| read_json(&plugin.root.join("hooks").join("hooks.json")));
    let Some(document) = document else {
        return Vec::new();
    };
    // `hooks.json` wraps the events in a `hooks` key; an inline manifest
    // object may not.
    let events = document.get("hooks").unwrap_or(&document);
    let label = format!("plugin {}", plugin.name);
    let root = plugin.root.display().to_string();
    let swap = |text: &str| text.replace("${CLAUDE_PLUGIN_ROOT}", &root);
    crate::hooks::spec::from_claude_settings(events, &label)
        .into_iter()
        .map(|mut spec| {
            match &mut spec.handler {
                crate::hooks::Handler::Command { command, args } => {
                    *command = swap(command);
                    if let Some(args) = args {
                        for arg in args.iter_mut() {
                            *arg = swap(arg);
                        }
                    }
                }
                crate::hooks::Handler::Http { url, .. } => *url = swap(url),
                _ => {}
            }
            spec
        })
        .collect()
}

fn read_json(path: &Path) -> Option<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

/// The hooks of every active plugin, in plugin order.
pub fn hook_specs(project_root: &Path) -> Vec<crate::hooks::HookSpec> {
    enabled(project_root)
        .iter()
        .flat_map(plugin_hooks)
        .collect()
}

/// MCP servers declared by active plugins, as the JSON `mcpServers` shape.
pub fn mcp_json(project_root: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    for plugin in enabled(project_root) {
        let file = plugin.root.join(".mcp.json");
        if let Some(json) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        {
            out.push(json);
        }
    }
    out
}

/* ---------------------------------------------------------------------- */
/* Marketplaces                                                           */
/* ---------------------------------------------------------------------- */

fn marketplaces_dir() -> Result<PathBuf, String> {
    store_dir()
        .map(|dir| dir.join("marketplaces"))
        .ok_or_else(|| "cannot find ~/.wizard".to_string())
}

fn installed_dir() -> Result<PathBuf, String> {
    store_dir()
        .map(|dir| dir.join("installed"))
        .ok_or_else(|| "cannot find ~/.wizard".to_string())
}

fn is_git_source(source: &str) -> bool {
    source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.starts_with("ssh://")
        || (source.split('/').count() == 2
            && !source.starts_with('.')
            && !source.starts_with('/')
            && !source.starts_with('~')
            && !source.contains(' '))
}

fn git_url(source: &str) -> String {
    if source.contains("://") || source.starts_with("git@") {
        source.to_string()
    } else {
        format!("https://github.com/{source}.git")
    }
}

fn expand_home(source: &str) -> PathBuf {
    match source.strip_prefix("~/").zip(dirs::home_dir()) {
        Some((rest, home)) => home.join(rest),
        None => PathBuf::from(source),
    }
}

fn git_clone(url: &str, into: &Path) -> Result<(), String> {
    let out = std::process::Command::new("git")
        .args(["clone", "--depth", "1", "--quiet", url])
        .arg(into)
        .output()
        .map_err(|err| format!("could not run git: {err}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git clone {url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let target = to.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplacePlugin {
    pub name: String,
    pub description: String,
    /// Where the plugin's files are: a path inside the marketplace, or a git
    /// source to clone.
    pub source: PluginSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    Relative(String),
    Git(String),
    Unsupported(String),
}

/// The plugins a marketplace directory lists.
pub fn read_marketplace(dir: &Path) -> Result<(String, Vec<MarketplacePlugin>), String> {
    let path = dir.join(".claude-plugin").join("marketplace.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|_| format!("{} has no .claude-plugin/marketplace.json", dir.display()))?;
    let json: Value =
        serde_json::from_str(&raw).map_err(|err| format!("{}: {err}", path.display()))?;
    let name = json
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{} has no name", path.display()))?
        .to_string();
    let mut plugins = Vec::new();
    for entry in json
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(plugin_name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        let source = match entry.get("source") {
            Some(Value::String(path)) => PluginSource::Relative(path.clone()),
            Some(obj) => match obj.get("source").and_then(Value::as_str) {
                Some("github") => match obj.get("repo").and_then(Value::as_str) {
                    Some(repo) => PluginSource::Git(git_url(repo)),
                    None => PluginSource::Unsupported("github source without repo".into()),
                },
                Some("url") => match obj.get("url").and_then(Value::as_str) {
                    Some(url) => PluginSource::Git(url.to_string()),
                    None => PluginSource::Unsupported("url source without url".into()),
                },
                other => PluginSource::Unsupported(other.unwrap_or("unknown").to_string()),
            },
            None => PluginSource::Relative(format!("./{plugin_name}")),
        };
        plugins.push(MarketplacePlugin {
            name: plugin_name.to_string(),
            description: entry
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            source,
        });
    }
    Ok((name, plugins))
}

fn marketplace_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(dir) = marketplaces_dir()
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            // A directory source is a one-line file holding the path.
            if path.is_file()
                && path.extension().is_some_and(|ext| ext == "path")
                && let Ok(target) = std::fs::read_to_string(&path)
            {
                roots.push(PathBuf::from(target.trim()));
            } else if path.is_dir() {
                roots.push(path);
            }
        }
    }
    // Marketplaces Claude Code already knows.
    if let Some(user) = super::user_dir()
        && let Some(json) = std::fs::read_to_string(user.join("plugins/known_marketplaces.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        && let Some(map) = json.as_object()
    {
        for entry in map.values() {
            if let Some(loc) = entry.get("installLocation").and_then(Value::as_str) {
                roots.push(PathBuf::from(loc));
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn find_marketplace(name: &str) -> Option<PathBuf> {
    marketplace_roots()
        .into_iter()
        .find(|root| read_marketplace(root).is_ok_and(|(have, _)| have == name))
}

/// `/plugin marketplace add <source>`: a git repo (`owner/repo` or URL) is
/// cloned; a local directory is remembered where it is.
pub fn marketplace_add(source: &str) -> Result<String, String> {
    marketplace_add_named(source).map(|(_, message)| message)
}

/// [`marketplace_add`], also saying what the marketplace is called.
pub fn marketplace_add_named(source: &str) -> Result<(String, String), String> {
    let dir = marketplaces_dir()?;
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    if is_git_source(source) {
        let staging = dir.join(format!(".staging-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        git_clone(&git_url(source), &staging)?;
        let (name, plugins) = match read_marketplace(&staging) {
            Ok(found) => found,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(err);
            }
        };
        let target = dir.join(&name);
        let _ = std::fs::remove_dir_all(&target);
        std::fs::rename(&staging, &target).map_err(|err| err.to_string())?;
        let message = format!("added marketplace {name} ({} plugins)", plugins.len());
        return Ok((name, message));
    }
    let path = expand_home(source);
    let (name, plugins) = read_marketplace(&path)?;
    let canonical = path.canonicalize().map_err(|err| err.to_string())?;
    std::fs::write(
        dir.join(format!("{name}.path")),
        canonical.display().to_string(),
    )
    .map_err(|err| err.to_string())?;
    let message = format!(
        "added marketplace {name} from {} ({} plugins)",
        canonical.display(),
        plugins.len()
    );
    Ok((name, message))
}

pub fn marketplace_remove(name: &str) -> Result<String, String> {
    let dir = marketplaces_dir()?;
    let pointer = dir.join(format!("{name}.path"));
    if pointer.is_file() {
        std::fs::remove_file(pointer).map_err(|err| err.to_string())?;
        return Ok(format!("removed marketplace {name}"));
    }
    let clone = dir.join(name);
    if clone.is_dir() {
        std::fs::remove_dir_all(clone).map_err(|err| err.to_string())?;
        return Ok(format!("removed marketplace {name}"));
    }
    Err(format!(
        "no marketplace named {name} was added with /plugin"
    ))
}

pub fn marketplace_update() -> Result<String, String> {
    let dir = marketplaces_dir()?;
    let mut lines = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join(".git").exists() {
                let out = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&path)
                    .args(["pull", "--ff-only", "--quiet"])
                    .output()
                    .map_err(|err| err.to_string())?;
                lines.push(format!(
                    "{}: {}",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                    if out.status.success() {
                        "up to date".to_string()
                    } else {
                        String::from_utf8_lossy(&out.stderr).trim().to_string()
                    }
                ));
            }
        }
    }
    Ok(if lines.is_empty() {
        "no cloned marketplaces to update".to_string()
    } else {
        lines.join("\n")
    })
}

pub fn marketplace_list() -> String {
    let mut lines = Vec::new();
    for root in marketplace_roots() {
        match read_marketplace(&root) {
            Ok((name, plugins)) => lines.push(format!(
                "{name}  {} plugins  {}",
                plugins.len(),
                root.display()
            )),
            Err(err) => lines.push(format!("(unreadable) {err}")),
        }
    }
    if lines.is_empty() {
        "no marketplaces; /plugin marketplace add <owner/repo|url|dir>".to_string()
    } else {
        lines.join("\n")
    }
}

/* ---------------------------------------------------------------------- */
/* Install / uninstall / enable                                           */
/* ---------------------------------------------------------------------- */

fn install_dir(src: &Path, origin: &str) -> Result<String, String> {
    let plugin = read_plugin(src, "installed").ok_or_else(|| {
        format!(
            "{} is not a plugin (no .claude-plugin/plugin.json)",
            src.display()
        )
    })?;
    let target = installed_dir()?.join(&plugin.name);
    let _ = std::fs::remove_dir_all(&target);
    copy_tree(src, &target).map_err(|err| err.to_string())?;
    let _ = std::fs::write(target.join(".wizard-origin"), origin);
    Ok(format!(
        "installed {}{} ({})",
        plugin.name,
        plugin.version.map(|v| format!(" {v}")).unwrap_or_default(),
        summary(&target)
    ))
}

fn summary(root: &Path) -> String {
    let count = |leaf: &str| super::namespaced_markdown(&root.join(leaf)).len();
    let skills = std::fs::read_dir(root.join("skills"))
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().join("SKILL.md").is_file())
                .count()
        })
        .unwrap_or(0);
    format!(
        "{} commands, {skills} skills, {} agents",
        count("commands"),
        count("agents")
    )
}

/// `/plugin install <spec>`.
///
/// `spec` is `name@marketplace`, a git source (`owner/repo`, a URL), or a
/// local plugin directory.
pub fn install(spec: &str) -> Result<String, String> {
    if let Some((name, market)) = spec.split_once('@')
        && !name.is_empty()
        && !market.is_empty()
        && !name.contains('/')
    {
        let root = find_marketplace(market).ok_or_else(|| {
            format!("no marketplace '{market}'; /plugin marketplace add <source> first")
        })?;
        let (_, plugins) = read_marketplace(&root)?;
        let entry = plugins
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("{market} lists no plugin '{name}'"))?;
        return match entry.source {
            PluginSource::Relative(rel) => {
                let src = root.join(rel.trim_start_matches("./"));
                install_dir(&src, spec)
            }
            PluginSource::Git(url) => install_git(&url, spec),
            PluginSource::Unsupported(kind) => Err(format!(
                "{name} uses a '{kind}' source, which Wizard cannot fetch yet"
            )),
        };
    }
    if is_git_source(spec) {
        return install_git(&git_url(spec), spec);
    }
    install_dir(&expand_home(spec), spec)
}

fn install_git(url: &str, origin: &str) -> Result<String, String> {
    let staging = std::env::temp_dir().join(format!(
        "wizard-plugin-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    git_clone(url, &staging)?;
    let result = install_dir(&staging, origin);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

pub fn uninstall(name: &str) -> Result<String, String> {
    let dir = installed_dir()?.join(name);
    if !dir.is_dir() {
        return Err(format!(
            "'{name}' was not installed with /plugin (Claude Code's own installs are managed by claude)"
        ));
    }
    std::fs::remove_dir_all(dir).map_err(|err| err.to_string())?;
    Ok(format!("uninstalled {name}"))
}

pub fn set_enabled(name: &str, on: bool) -> Result<String, String> {
    let dir = installed_dir()?.join(name);
    if !dir.is_dir() {
        return Err(format!("'{name}' was not installed with /plugin"));
    }
    let marker = dir.join(".disabled");
    if on {
        let _ = std::fs::remove_file(marker);
    } else {
        std::fs::write(marker, "").map_err(|err| err.to_string())?;
    }
    Ok(format!(
        "{} {name}",
        if on { "enabled" } else { "disabled" }
    ))
}

/// `wizard plugin update <name>`: refresh the marketplaces and install again
/// from wherever the plugin came from.
pub fn update(name: &str) -> Result<String, String> {
    let dir = installed_dir()?.join(name);
    let origin = std::fs::read_to_string(dir.join(".wizard-origin"))
        .map_err(|_| format!("'{name}' was not installed with `wizard plugin install`"))?;
    let origin = origin.trim();
    if origin.contains('@') {
        let _ = marketplace_update();
    }
    let was_disabled = dir.join(".disabled").exists();
    let message = install(origin)?;
    if was_disabled {
        set_enabled(name, false)?;
    }
    Ok(message)
}

/// Every plugin `wizard plugin install` put in place, disabled ones included:
/// `(name, enabled)`.
pub fn installed_names() -> Vec<(String, bool)> {
    let Ok(dir) = installed_dir() else {
        return Vec::new();
    };
    let mut out: Vec<(String, bool)> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .map(|entry| {
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        !entry.path().join(".disabled").exists(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// `wizard plugin details <name>`: the manifest's own fields and what the
/// plugin holds.
pub fn details(name: &str, project_root: &Path) -> Result<String, String> {
    let plugin = enabled(project_root)
        .into_iter()
        .find(|plugin| plugin.name == name)
        .or_else(|| read_plugin(&installed_dir().ok()?.join(name), "installed"))
        .ok_or_else(|| format!("no plugin named '{name}'"))?;
    let mut out = plugin.name.clone();
    if let Some(version) = &plugin.version {
        out.push_str(&format!(" {version}"));
    }
    out.push_str(&format!(
        "\n  from: {} ({})",
        plugin.root.display(),
        plugin.origin
    ));
    if let Some(description) = &plugin.description {
        out.push_str(&format!("\n  {description}"));
    }
    out.push_str(&format!("\n  holds: {}", summary(&plugin.root)));
    Ok(out)
}

/// `/plugin validate <dir>`.
pub fn validate(dir: &Path) -> Result<String, String> {
    let manifest = manifest_path(dir);
    let raw = std::fs::read_to_string(&manifest)
        .map_err(|_| format!("{} is missing", manifest.display()))?;
    let json: Value =
        serde_json::from_str(&raw).map_err(|err| format!("{}: {err}", manifest.display()))?;
    let name = json
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty() && !n.contains(char::is_whitespace))
        .ok_or("plugin.json needs a `name` without spaces")?;
    Ok(format!("{name}: valid ({})", summary(dir)))
}

/// `/plugin list` and the bare `/plugin`.
pub fn report(project_root: &Path) -> String {
    let plugins = enabled(project_root);
    let mut out = String::new();
    if plugins.is_empty() {
        out.push_str("no Claude-format plugins active\n");
    }
    for plugin in &plugins {
        out.push_str(&format!(
            "{:<22} {:<12} {:<11} {}\n",
            plugin.name,
            plugin.version.as_deref().unwrap_or("-"),
            plugin.origin,
            summary(&plugin.root)
        ));
    }
    out.push_str(
        "\n/plugin install <name@marketplace|owner/repo|dir>, uninstall <name>, enable|disable <name>,\n\
         marketplace add|list|remove|update, validate <dir>, list",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::super::testing::Tmp;
    use super::*;

    fn plugin_tree(tmp: &Tmp, rel: &str, name: &str) -> PathBuf {
        tmp.write(
            &format!("{rel}/.claude-plugin/plugin.json"),
            &format!(r#"{{"name":"{name}","version":"1.2.0","description":"d"}}"#),
        );
        tmp.write(
            &format!("{rel}/commands/hi.md"),
            "---\ndescription: x\n---\nhi",
        );
        tmp.write(&format!("{rel}/commands/sub/go.md"), "go");
        tmp.write(&format!("{rel}/agents/a.md"), "---\nname: a\n---\nA.");
        tmp.0.join(rel)
    }

    #[test]
    fn a_plugins_hooks_json_becomes_hook_specs_with_the_plugin_root_filled_in() {
        let tmp = Tmp::new("plh");
        let root = plugin_tree(&tmp, "p", "guard");
        tmp.write(
            "p/hooks/hooks.json",
            r#"{"description":"x","hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[
                {"type":"command","command":"${CLAUDE_PLUGIN_ROOT}/check.sh","timeout":5},
                {"type":"http","url":"https://h.test/${CLAUDE_PLUGIN_ROOT}"}]}],
               "SessionStart":[{"hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        );
        let plugin = read_plugin(&root, "session").expect("plugin");
        let specs = plugin_hooks(&plugin);
        assert_eq!(specs.len(), 3);
        let pre: Vec<_> = specs
            .iter()
            .filter(|s| s.event == crate::hooks::HookEvent::PreToolUse)
            .collect();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0].matcher.as_deref(), Some("Bash"));
        assert_eq!(pre[0].source, "plugin guard");
        assert_eq!(pre[0].timeout_secs, Some(5));
        let root_text = root.display().to_string();
        match &pre[0].handler {
            crate::hooks::Handler::Command { command, .. } => {
                assert_eq!(command, &format!("{root_text}/check.sh"));
            }
            other => panic!("not a command: {other:?}"),
        }
        match &pre[1].handler {
            crate::hooks::Handler::Http { url, .. } => {
                assert_eq!(url, &format!("https://h.test/{root_text}"));
            }
            other => panic!("not http: {other:?}"),
        }
    }

    #[test]
    fn a_manifest_can_point_at_its_hooks_file_or_carry_them_inline() {
        let tmp = Tmp::new("plh2");
        tmp.write(
            "a/.claude-plugin/plugin.json",
            r#"{"name":"a","hooks":"./config/h.json"}"#,
        );
        tmp.write(
            "a/config/h.json",
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"one"}]}]}}"#,
        );
        tmp.write(
            "b/.claude-plugin/plugin.json",
            r#"{"name":"b","hooks":{"Stop":[{"hooks":[{"type":"command","command":"two"}]}]}}"#,
        );
        let a = read_plugin(&tmp.0.join("a"), "session").expect("a");
        let b = read_plugin(&tmp.0.join("b"), "session").expect("b");
        assert_eq!(plugin_hooks(&a).len(), 1);
        assert_eq!(plugin_hooks(&b).len(), 1);
        assert!(
            plugin_hooks(&read_plugin(&tmp.0.join("a"), "session").unwrap())
                .iter()
                .all(|s| s.source == "plugin a")
        );
    }

    #[test]
    fn a_directory_with_a_manifest_is_a_plugin_named_by_it() {
        let tmp = Tmp::new("pl");
        let root = plugin_tree(&tmp, "p", "pretty");
        let plugin = read_plugin(&root, "session").expect("plugin");
        assert_eq!(plugin.name, "pretty");
        assert_eq!(plugin.version.as_deref(), Some("1.2.0"));
        assert!(read_plugin(&tmp.0.join("nothing"), "session").is_none());
    }

    #[test]
    fn session_dirs_load_one_plugin_or_a_folder_of_them() {
        let tmp = Tmp::new("pl2");
        let one = plugin_tree(&tmp, "one", "one");
        plugin_tree(&tmp, "many/a", "a");
        plugin_tree(&tmp, "many/b", "b");
        let names: Vec<String> = from_dirs(&[one, tmp.0.join("many")])
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["one", "a", "b"]);
    }

    #[test]
    fn plugin_commands_are_namespaced_by_plugin_and_folder() {
        let tmp = Tmp::new("pl3");
        let root = plugin_tree(&tmp, "p", "pretty");
        let plugin = read_plugin(&root, "session").unwrap();
        let names: Vec<String> = super::super::namespaced_markdown(&plugin.root.join("commands"))
            .into_iter()
            .map(|(n, _)| format!("{}:{n}", plugin.name))
            .collect();
        assert_eq!(names, ["pretty:hi", "pretty:sub:go"]);
    }

    #[test]
    fn claude_code_install_records_read_in_both_shapes() {
        let v2: Value = serde_json::from_str(
            r#"{"version":2,"plugins":{"a@m":[{"scope":"user","installPath":"/x/a"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            claude_installed_paths(&v2),
            [("a@m".to_string(), PathBuf::from("/x/a"))]
        );
        let v1: Value =
            serde_json::from_str(r#"{"plugins":{"b@m":{"installPath":"/x/b"}}}"#).unwrap();
        assert_eq!(
            claude_installed_paths(&v1),
            [("b@m".to_string(), PathBuf::from("/x/b"))]
        );
    }

    #[test]
    fn a_marketplace_lists_relative_and_git_sources() {
        let tmp = Tmp::new("mk");
        tmp.write(
            "m/.claude-plugin/marketplace.json",
            r#"{"name":"mk","plugins":[
                {"name":"rel","source":"./plugins/rel","description":"r"},
                {"name":"gh","source":{"source":"github","repo":"o/r"}},
                {"name":"odd","source":{"source":"npm","package":"x"}}]}"#,
        );
        let (name, plugins) = read_marketplace(&tmp.0.join("m")).expect("reads");
        assert_eq!(name, "mk");
        assert_eq!(
            plugins[0].source,
            PluginSource::Relative("./plugins/rel".into())
        );
        assert_eq!(
            plugins[1].source,
            PluginSource::Git("https://github.com/o/r.git".into())
        );
        assert!(matches!(plugins[2].source, PluginSource::Unsupported(_)));
    }

    #[test]
    fn git_sources_are_told_from_paths() {
        assert!(is_git_source("owner/repo"));
        assert!(is_git_source("https://example.com/x.git"));
        assert!(!is_git_source("./local/dir"));
        assert!(!is_git_source("/abs/dir"));
        assert!(!is_git_source("~/dir"));
        assert!(!is_git_source("a/b/c"));
    }

    #[test]
    fn validate_wants_a_name_without_spaces() {
        let tmp = Tmp::new("val");
        tmp.write("ok/.claude-plugin/plugin.json", r#"{"name":"fine"}"#);
        tmp.write("bad/.claude-plugin/plugin.json", r#"{"name":"not fine"}"#);
        assert!(
            validate(&tmp.0.join("ok"))
                .unwrap()
                .starts_with("fine: valid")
        );
        assert!(validate(&tmp.0.join("bad")).is_err());
        assert!(validate(&tmp.0.join("missing")).is_err());
    }
}
