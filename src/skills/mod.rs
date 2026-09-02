//! Skills loader: markdown files with optional YAML-style frontmatter,
//! discovered as `<dir>/<skill-name>/SKILL.md`. Loaded at startup and on
//! `/reload`.
//!
//! The system prompt carries an *index* (name, description, path), not the
//! body. The model reads the file with `read_file` when the skill matches,
//! the same split the charter uses with `manual`. A skill may opt back into
//! a resident body with `always: true` in its frontmatter; that is the
//! exception, not the default. `when_env` hides a skill from the prompt
//! unless at least one named environment variable is set and non-empty
//! (Buzz room stays off the terminal prompt until Buzz is configured).
//! A long skill (wrangler is ~11 KB) that is pasted into every session is
//! a tax on every turn that is not using it.
//!
//! Two roots are scanned: the bundled `skills/` directory shipped alongside
//! the binary, and `~/.wizard/skills/` where `/evolve` writes new ones.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Frontmatter of a `SKILL.md` (everything between leading `---` fences).
/// All fields optional; missing names fall back to the directory name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillMeta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// When true, `render_for_prompt` inlines the body instead of just the
    /// index. Default is false: the body stays on disk until the skill is
    /// actually needed.
    #[serde(default)]
    pub always: bool,
    /// If non-empty, the skill is omitted from the prompt unless at least
    /// one of these environment variables is set to a non-empty value.
    #[serde(default)]
    pub when_env: Vec<String>,
}

/// One loaded skill.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Effective name (frontmatter `name`, else parent directory name).
    pub name: String,
    /// Path of the `SKILL.md` it was loaded from.
    pub path: PathBuf,
    pub meta: SkillMeta,
    /// Markdown body with frontmatter stripped.
    pub body: String,
}

/// Default skill roots, in shadowing order (later roots win on name
/// collision): an explicit dev root (`WIZARD_DEV_SKILLS`, or the repo
/// checkout's `skills/` in debug builds only), `skills/` next to the
/// installed binary, then the user's `~/.wizard/skills/` where `/evolve`
/// writes new skills, then the active harness bundle's `skills/` (if any).
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    // Dev-checkout skills. `env!("CARGO_MANIFEST_DIR")` bakes the build
    // machine's checkout path into the binary, so release builds must never
    // read it implicitly — an installed binary would silently load skills
    // from whatever repo it happened to be compiled in. Opt in explicitly
    // with WIZARD_DEV_SKILLS=<dir>; debug builds keep the old convenience.
    let dev_root = std::env::var_os("WIZARD_DEV_SKILLS")
        .map(PathBuf::from)
        .or_else(|| {
            cfg!(debug_assertions).then(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("skills"))
        });
    if let Some(dev) = dev_root
        && dev.is_dir()
    {
        roots.push(dev);
    }

    // Bundled alongside the installed binary.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for candidate in [dir.join("skills"), dir.join("../share/wizard/skills")] {
            if candidate.is_dir() && !roots.contains(&candidate) {
                roots.push(candidate);
            }
        }
    }

    // User skills after bundled ones so they shadow them.
    if let Ok(user) = crate::config::Config::skills_dir() {
        roots.push(user);
    }

    // Harness bundle skills very last: the active bundle shadows everything,
    // since it is the surface harness-evolution loops mutate.
    if let Some(harness) = crate::config::Config::harness_dir() {
        let skills = harness.join("skills");
        if skills.is_dir() {
            roots.push(skills);
        }
    }

    roots
}

/// Parse a single `SKILL.md`: split optional `---` frontmatter from the
/// body and derive the effective name.
pub fn parse_skill(path: &Path) -> Result<Skill> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading skill {}", path.display()))?;
    let (meta, body) = split_frontmatter(&raw);

    let name = meta
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| {
            // Fall back to the parent directory name (`<name>/SKILL.md`).
            path.parent()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
        })
        .or_else(|| path.file_stem().map(|n| n.to_string_lossy().into_owned()))
        .with_context(|| format!("could not derive a name for skill {}", path.display()))?;

    Ok(Skill {
        name,
        path: path.to_path_buf(),
        meta,
        body,
    })
}

/// Scan `roots` for `*/SKILL.md` and load each skill. Missing roots are
/// skipped silently; unparseable skills are skipped with a warning. Later
/// roots override earlier ones on name collision (so user skills shadow
/// bundled ones).
pub fn load_skills(roots: &[PathBuf]) -> Result<Vec<Skill>> {
    let mut skills: Vec<Skill> = Vec::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();

    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue; // missing or unreadable root
        };

        // Sort for deterministic prompt order regardless of FS iteration.
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();

        for dir in dirs {
            let manifest = dir.join("SKILL.md");
            if !manifest.is_file() {
                continue;
            }
            match parse_skill(&manifest) {
                Ok(skill) => {
                    if let Some(&existing) = by_name.get(&skill.name) {
                        skills[existing] = skill; // later root shadows earlier
                    } else {
                        by_name.insert(skill.name.clone(), skills.len());
                        skills.push(skill);
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        path = %manifest.display(),
                        error = %format!("{err:#}"),
                        "skipping unparseable skill"
                    );
                }
            }
        }
    }

    Ok(skills)
}

/// True when this skill should appear in the system prompt.
pub fn skill_visible(skill: &Skill) -> bool {
    if skill.meta.when_env.is_empty() {
        return true;
    }
    skill
        .meta
        .when_env
        .iter()
        .any(|name| std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false))
}

/// Render loaded skills as a system-prompt section: a `## Skills` header
/// followed by each skill's name, description, and path. The body stays on
/// disk unless the skill opted in with `always: true`. Skills with
/// `when_env` are omitted unless at least one named variable is set.
/// Returns an empty string when no skills are loaded (or all are gated).
pub fn render_for_prompt(skills: &[Skill]) -> String {
    let skills: Vec<&Skill> = skills.iter().filter(|s| skill_visible(s)).collect();
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "## Skills\n\n\
         Name and description only. Read a skill's file before acting on it; \
         do not guess its body from the description. A skill with `always: true` \
         has its body inlined below.\n",
    );
    for skill in skills {
        out.push_str("\n### ");
        out.push_str(&skill.name);
        out.push('\n');
        if let Some(description) = skill.meta.description.as_deref() {
            let description = description.trim();
            if !description.is_empty() {
                out.push_str(description);
                out.push('\n');
            }
        }
        out.push_str("File: `");
        out.push_str(&skill.path.display().to_string());
        out.push_str("`\n");
        if skill.meta.always {
            let body = skill.body.trim();
            if !body.is_empty() {
                out.push('\n');
                out.push_str(body);
                out.push('\n');
            }
        }
    }
    out
}

/// Split optional `---`-fenced frontmatter from the markdown body. When the
/// file does not start with a `---` line (or the closing fence is missing),
/// the whole content is the body. Shared with the custom-command loader
/// (`crate::commands`), which uses the same convention.
pub(crate) fn split_frontmatter(raw: &str) -> (SkillMeta, String) {
    let lines: Vec<&str> = raw.lines().collect();
    if lines.first().map(|l| l.trim_end()) == Some("---")
        && let Some(end) = lines[1..].iter().position(|l| l.trim_end() == "---")
    {
        let meta = parse_meta(&lines[1..1 + end]);
        let body = lines[2 + end..].join("\n").trim().to_string();
        return (meta, body);
    }
    (SkillMeta::default(), raw.trim().to_string())
}

/// Parse the simple `key: value` frontmatter lines we support (`name`,
/// `description`, `always`, `when_env`). Unknown keys and malformed lines
/// are ignored. `when_env` is a comma-separated list of env var names.
fn parse_meta(lines: &[&str]) -> SkillMeta {
    let mut meta = SkillMeta::default();
    for line in lines {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = strip_quotes(value.trim());
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "name" => meta.name = Some(value.to_string()),
            "description" => meta.description = Some(value.to_string()),
            "always" => meta.always = parse_bool(value),
            "when_env" => {
                meta.when_env = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            _ => {}
        }
    }
    meta
}

/// YAML-ish truthy values. Anything else is false, including the empty
/// string (already filtered above) and unknown tokens.
fn parse_bool(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1")
}

/// Strip one matching pair of surrounding quotes, if present.
fn strip_quotes(value: &str) -> &str {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp dir per test; never reused across runs.
    fn temp_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wizard-skills-test-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn write_skill(root: &Path, dir_name: &str, content: &str) {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(&dir).expect("create skill dir");
        std::fs::write(dir.join("SKILL.md"), content).expect("write SKILL.md");
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let root = temp_root("frontmatter");
        write_skill(
            &root,
            "commits",
            "---\nname: conventional-commits\ndescription: \"How to write commits\"\nalways: true\n---\n\nUse `type(scope): subject`.\n",
        );
        let skill = parse_skill(&root.join("commits/SKILL.md")).expect("parse");
        assert_eq!(skill.name, "conventional-commits");
        assert_eq!(
            skill.meta.description.as_deref(),
            Some("How to write commits")
        );
        assert!(skill.meta.always);
        assert_eq!(skill.body, "Use `type(scope): subject`.");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn missing_frontmatter_falls_back_to_directory_name() {
        let root = temp_root("noformat");
        write_skill(&root, "plain", "Just a body, no fences.\n");
        let skill = parse_skill(&root.join("plain/SKILL.md")).expect("parse");
        assert_eq!(skill.name, "plain");
        assert!(skill.meta.name.is_none());
        assert_eq!(skill.body, "Just a body, no fences.");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn unclosed_frontmatter_is_treated_as_body() {
        let root = temp_root("unclosed");
        write_skill(&root, "broken", "---\nname: nope\nno closing fence\n");
        let skill = parse_skill(&root.join("broken/SKILL.md")).expect("parse");
        assert_eq!(skill.name, "broken");
        assert!(skill.body.contains("no closing fence"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn later_roots_shadow_earlier_by_name() {
        let bundled = temp_root("bundled");
        let user = temp_root("user");
        write_skill(&bundled, "coding", "---\nname: coding\n---\nbundled body\n");
        write_skill(&user, "coding", "---\nname: coding\n---\nuser body\n");
        write_skill(&bundled, "extra", "---\nname: extra\n---\nextra body\n");

        let skills = load_skills(&[bundled.clone(), user.clone()]).expect("load");
        assert_eq!(skills.len(), 2);
        let coding = skills.iter().find(|s| s.name == "coding").expect("coding");
        assert_eq!(coding.body, "user body");

        std::fs::remove_dir_all(bundled).ok();
        std::fs::remove_dir_all(user).ok();
    }

    #[test]
    fn missing_roots_are_skipped() {
        let skills = load_skills(&[PathBuf::from("/nonexistent/wizard-skills")]).expect("load");
        assert!(skills.is_empty());
    }

    #[test]
    fn render_is_empty_without_skills() {
        assert_eq!(render_for_prompt(&[]), "");
    }

    #[test]
    fn render_includes_header_name_description_and_path() {
        let skills = vec![Skill {
            name: "demo".to_string(),
            path: PathBuf::from("demo/SKILL.md"),
            meta: SkillMeta {
                name: Some("demo".to_string()),
                description: Some("A demo skill".to_string()),
                always: false,
                when_env: Vec::new(),
            },
            body: "Body text.".to_string(),
        }];
        let rendered = render_for_prompt(&skills);
        assert!(rendered.starts_with("## Skills\n"));
        assert!(rendered.contains("### demo\n"));
        assert!(rendered.contains("A demo skill\n"));
        assert!(rendered.contains("File: `demo/SKILL.md`\n"));
        assert!(
            !rendered.contains("Body text."),
            "default skills keep their body off the prompt: {rendered}"
        );
    }

    #[test]
    fn render_inlines_body_when_always_is_set() {
        let skills = vec![Skill {
            name: "demo".to_string(),
            path: PathBuf::from("demo/SKILL.md"),
            meta: SkillMeta {
                name: Some("demo".to_string()),
                description: Some("A demo skill".to_string()),
                always: true,
                when_env: Vec::new(),
            },
            body: "Body text.".to_string(),
        }];
        let rendered = render_for_prompt(&skills);
        assert!(rendered.contains("Body text.\n"));
        assert!(rendered.contains("File: `demo/SKILL.md`\n"));
    }

    #[test]
    fn render_omits_skill_when_env_unset() {
        let var = format!("WIZARD_SKILL_TEST_{}", uuid::Uuid::new_v4().simple());
        unsafe { std::env::remove_var(&var) };
        let skills = vec![Skill {
            name: "buzz-room".to_string(),
            path: PathBuf::from("buzz-room/SKILL.md"),
            meta: SkillMeta {
                name: Some("buzz-room".to_string()),
                description: Some("Buzz workspace".to_string()),
                always: true,
                when_env: vec![var.clone()],
            },
            body: "Prefer buzz messages send.".to_string(),
        }];
        assert_eq!(render_for_prompt(&skills), "");
        unsafe { std::env::set_var(&var, "1") };
        let rendered = render_for_prompt(&skills);
        assert!(rendered.contains("### buzz-room\n"));
        assert!(rendered.contains("Prefer buzz messages send."));
        unsafe { std::env::remove_var(&var) };
    }

    #[test]
    fn parses_when_env_list() {
        let root = temp_root("whenenv");
        write_skill(
            &root,
            "buzz-room",
            "---\nname: buzz-room\nwhen_env: BUZZ_PRIVATE_KEY, BUZZ_RELAY_URL\nalways: true\n---\nbody\n",
        );
        let skill = parse_skill(&root.join("buzz-room/SKILL.md")).expect("parse");
        assert_eq!(
            skill.meta.when_env,
            vec!["BUZZ_PRIVATE_KEY", "BUZZ_RELAY_URL"]
        );
        std::fs::remove_dir_all(root).ok();
    }
}
