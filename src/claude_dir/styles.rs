//! Output styles: a named block of prose that changes how the agent answers.
//!
//! Same files as Claude Code (`<dir>/*.md` with `name` and `description`
//! frontmatter, body is the instruction), read from `~/.wizard/output-styles`,
//! `~/.claude/output-styles` and the project's `.wizard` and `.claude`
//! equivalents. The chosen name lives in `~/.wizard/output-style`; failing
//! that, Claude Code's own `outputStyle` setting is honored so a project that
//! pins a style gets it here too.

use std::path::{Path, PathBuf};

pub const DEFAULT: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputStyle {
    pub name: String,
    pub description: String,
    /// Empty for the default style, which adds nothing.
    pub prompt: String,
    /// `built-in`, or the file it was read from.
    pub source: String,
}

fn builtin() -> Vec<OutputStyle> {
    let style = |name: &str, description: &str, prompt: &str| OutputStyle {
        name: name.to_string(),
        description: description.to_string(),
        prompt: prompt.to_string(),
        source: "built-in".to_string(),
    };
    vec![
        style(DEFAULT, "the standard way of answering", ""),
        style(
            "explanatory",
            "say why as well as what: explain choices and codebase patterns as you work",
            "Output style: explanatory. While you work, add short notes on why you chose an approach \
             and on patterns in the codebase that the user would want to know. Keep the notes next to \
             the change they explain, and keep the work itself first.",
        ),
        style(
            "learning",
            "teach by leaving small pieces for the user to write",
            "Output style: learning. Teach while you work. For a change with a real design choice in \
             it, do the scaffolding and leave one small, well-marked piece (a function body of a few \
             lines) for the user to write, say exactly what it should do, and wait for them. Explain \
             the choice briefly before asking.",
        ),
    ]
}

fn parse_style(path: &Path) -> Option<OutputStyle> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (meta, body) = super::split_frontmatter(&raw);
    let name = meta
        .get("name")
        .cloned()
        .or_else(|| path.file_stem()?.to_str().map(str::to_string))?;
    if name.trim().is_empty() {
        return None;
    }
    Some(OutputStyle {
        name,
        description: meta.get("description").cloned().unwrap_or_default(),
        prompt: body,
        source: path.display().to_string(),
    })
}

/// Every style found in `dirs`, built-ins first; a later directory shadows an
/// earlier one, and a file shadows a built-in of the same (case-insensitive)
/// name.
pub fn load_from(dirs: &[PathBuf]) -> Vec<OutputStyle> {
    let mut styles = builtin();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        paths.sort();
        for style in paths.iter().filter_map(|path| parse_style(path)) {
            styles.retain(|have| !have.name.eq_ignore_ascii_case(&style.name));
            styles.push(style);
        }
    }
    styles
}

fn search_dirs(project_root: &Path) -> Vec<PathBuf> {
    let mut dirs = super::plugins::component_dirs(project_root, "output-styles");
    if let Some(user) = super::user_dir() {
        dirs.push(user.join("output-styles"));
    }
    if let Ok(wizard) = crate::config::Config::wizard_dir() {
        dirs.push(wizard.join("output-styles"));
    }
    dirs.push(super::project_dir(project_root).join("output-styles"));
    dirs.push(project_root.join(".wizard").join("output-styles"));
    dirs
}

/// Every style available to `project_root`.
pub fn available(project_root: &Path) -> Vec<OutputStyle> {
    load_from(&search_dirs(project_root))
}

fn pref_path() -> Option<PathBuf> {
    crate::config::Config::wizard_dir()
        .ok()
        .map(|dir| dir.join("output-style"))
}

fn setting_in(dir: &Path, file: &str) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(file)).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    json.get("outputStyle")?.as_str().map(str::to_string)
}

/// The name of the style in force: Wizard's own choice, else the project's
/// Claude Code setting, else the user's, else `default`.
pub fn active_name(project_root: &Path) -> String {
    if let Some(name) = pref_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
    {
        return name;
    }
    let project = super::project_dir(project_root);
    [
        setting_in(&project, "settings.local.json"),
        setting_in(&project, "settings.json"),
        super::user_dir().and_then(|dir| setting_in(&dir, "settings.json")),
    ]
    .into_iter()
    .flatten()
    .next()
    .unwrap_or_else(|| DEFAULT.to_string())
}

/// Make `name` the style, or say why it cannot be. Matches case-insensitively
/// and stores the style's own spelling.
pub fn set_active(project_root: &Path, name: &str) -> Result<String, String> {
    let styles = available(project_root);
    let Some(style) = styles
        .iter()
        .find(|style| style.name.eq_ignore_ascii_case(name.trim()))
    else {
        let names: Vec<&str> = styles.iter().map(|style| style.name.as_str()).collect();
        return Err(format!(
            "no output style '{}' (available: {})",
            name.trim(),
            names.join(", ")
        ));
    };
    let path = pref_path().ok_or("cannot find ~/.wizard")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    std::fs::write(&path, &style.name).map_err(|err| err.to_string())?;
    Ok(style.name.clone())
}

/// The system-prompt section for the style in force, or `None` for `default`
/// and for a name that matches nothing.
pub fn active_prompt(project_root: &Path) -> Option<String> {
    let name = active_name(project_root);
    let style = available(project_root)
        .into_iter()
        .find(|style| style.name.eq_ignore_ascii_case(&name))?;
    (!style.prompt.trim().is_empty())
        .then(|| format!("## Output style: {}\n\n{}", style.name, style.prompt.trim()))
}

/// `/output-style` with no argument.
pub fn report(project_root: &Path) -> String {
    let active = active_name(project_root);
    let mut out = format!("output style: {active}\n");
    for style in available(project_root) {
        let mark = if style.name.eq_ignore_ascii_case(&active) {
            "*"
        } else {
            " "
        };
        out.push_str(&format!(
            "{mark} {:<14} {}\n",
            style.name, style.description
        ));
    }
    out.push_str(
        "\n/output-style <name> switches; it applies from the next message after /reload.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::super::testing::Tmp;
    use super::*;

    #[test]
    fn built_ins_come_first_and_files_shadow_them_by_name() {
        let tmp = Tmp::new("styles");
        tmp.write(
            "s/terse.md",
            "---\nname: Terse\ndescription: short answers\n---\nBe terse.",
        );
        tmp.write(
            "s/explain.md",
            "---\nname: Explanatory\ndescription: mine\n---\nMy version.",
        );
        let styles = load_from(&[tmp.path().join("s")]);
        let names: Vec<&str> = styles.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["default", "learning", "Explanatory", "Terse"]);
        let explanatory = styles.iter().find(|s| s.name == "Explanatory").unwrap();
        assert_eq!(explanatory.prompt, "My version.");
    }

    #[test]
    fn the_file_stem_names_a_style_without_a_name_key() {
        let tmp = Tmp::new("styles2");
        tmp.write("s/pirate.md", "Talk like a pirate.");
        let styles = load_from(&[tmp.path().join("s")]);
        assert!(styles.iter().any(|s| s.name == "pirate"));
    }

    #[test]
    fn a_project_setting_names_the_active_style() {
        let tmp = Tmp::new("styles3");
        tmp.write(".claude/settings.json", r#"{"outputStyle":"learning"}"#);
        // The preference file under ~/.wizard wins when present; this test only
        // holds when it is absent, so it asserts the settings reader directly.
        assert_eq!(
            setting_in(&tmp.path().join(".claude"), "settings.json").as_deref(),
            Some("learning")
        );
    }

    #[test]
    fn the_default_style_adds_no_prompt() {
        let styles = builtin();
        assert!(styles[0].prompt.is_empty());
        assert!(styles[1..].iter().all(|s| !s.prompt.is_empty()));
    }
}
