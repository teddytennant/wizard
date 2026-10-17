//! Skills wizard can load, for the `$` and `@` popups and `/skills`. Wizard keeps each one as
//! `<root>/<name>/SKILL.md` with a YAML front matter (`name`, `description`); ACP does not list
//! them, so codexw reads the same two roots wizard does for a user: `~/.wizard/skills` and the
//! project's `.wizard/skills`. A project skill shadows a user skill of the same name.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    /// One line: the front matter text with runs of whitespace collapsed.
    pub description: String,
    pub path: PathBuf,
}

/// Every skill under the two roots, sorted by name (case-insensitive).
pub fn load(home: Option<&str>, cwd: &Path) -> Vec<Skill> {
    let mut roots = Vec::new();
    if let Some(h) = home.filter(|h| !h.is_empty()) {
        roots.push(Path::new(h).join(".wizard/skills"));
    }
    roots.push(cwd.join(".wizard/skills"));
    let mut out: Vec<Skill> = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in rd.flatten() {
            let file = e.path().join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let fallback = e.file_name().to_string_lossy().to_string();
            let skill = parse(&text, &fallback, file);
            match out.iter_mut().find(|s| s.name == skill.name) {
                Some(old) => *old = skill,
                None => out.push(skill),
            }
        }
    }
    out.sort_by_cached_key(|s| s.name.to_lowercase());
    out
}

/// Read `name` and `description` from the front matter. A missing name falls back to the
/// directory name; folded and literal block scalars (`>`, `|`, with `-` or `+`) are joined.
pub fn parse(text: &str, fallback_name: &str, path: PathBuf) -> Skill {
    let mut name = None;
    let mut description = String::new();
    let mut lines = text.lines();
    if lines.next().map(str::trim) == Some("---") {
        let mut key: Option<&str> = None;
        let mut block = false;
        let mut parts: Vec<String> = Vec::new();
        let mut flush = |key: Option<&str>, parts: &mut Vec<String>| {
            let v = parts.join(" ");
            match key {
                Some("name") => name = Some(unquote(&v)),
                Some("description") => description = unquote(&v),
                _ => {}
            }
            parts.clear();
        };
        for line in lines {
            if line.trim() == "---" {
                break;
            }
            let top = !line.starts_with(char::is_whitespace) && !line.is_empty();
            if top {
                flush(key, &mut parts);
                key = None;
                block = false;
                if let Some((k, v)) = line.split_once(':') {
                    key = Some(match k.trim() {
                        "name" => "name",
                        "description" => "description",
                        _ => "other",
                    });
                    let v = v.trim();
                    if v.starts_with(['>', '|']) {
                        block = true;
                    } else if !v.is_empty() {
                        parts.push(v.to_string());
                    }
                }
            } else if block || key.is_some() {
                let t = line.trim();
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        }
        flush(key, &mut parts);
    }
    Skill {
        name: name
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| fallback_name.to_string()),
        description: description.split_whitespace().collect::<Vec<_>>().join(" "),
        path,
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(t: &str) -> Skill {
        parse(t, "dir-name", PathBuf::from("x/SKILL.md"))
    }

    #[test]
    fn folded_description_joins_into_one_line() {
        let s = p(
            "---\nname: common-sense\ndescription: >\n  Give the agent\n  judgment.\n  Use it.\nlicense: MIT\nmetadata:\n  version: \"1\"\n---\n# body\n",
        );
        assert_eq!(s.name, "common-sense");
        assert_eq!(s.description, "Give the agent judgment. Use it.");
    }

    #[test]
    fn inline_quoted_values_and_missing_name() {
        let s = p("---\ndescription: \"Scaffold plugins\"\n---\n");
        assert_eq!(s.name, "dir-name");
        assert_eq!(s.description, "Scaffold plugins");
    }

    #[test]
    fn no_front_matter_still_gives_a_skill() {
        let s = p("# just a body\n");
        assert_eq!((s.name.as_str(), s.description.as_str()), ("dir-name", ""));
    }

    #[test]
    fn project_skills_shadow_user_skills_and_the_list_is_sorted() {
        let root = std::env::temp_dir().join(format!("cxw-skills-{}", std::process::id()));
        let home = root.join("home");
        let proj = root.join("proj");
        for (dir, name, desc) in [
            (home.join(".wizard/skills/zeta"), "zeta", "user zeta"),
            (home.join(".wizard/skills/Alpha"), "Alpha", "user alpha"),
            (proj.join(".wizard/skills/zeta"), "zeta", "project zeta"),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {desc}\n---\n"),
            )
            .unwrap();
        }
        let got = load(home.to_str(), &proj);
        let _ = std::fs::remove_dir_all(&root);
        let view: Vec<_> = got
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str()))
            .collect();
        assert_eq!(view, [("Alpha", "user alpha"), ("zeta", "project zeta")]);
    }
}
