//! Starter prompts: what an empty session suggests, read off the working
//! directory with no model call.

use std::collections::HashMap;
use std::path::Path;

/// What the starter prompts are read off. Gathered once at startup, from the
/// directory alone.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CwdFacts {
    pub git_repo: bool,
    pub readme: bool,
    /// Tracked files with uncommitted changes.
    pub dirty: bool,
    /// A package manifest (Cargo.toml, package.json, pyproject.toml, …).
    pub manifest: bool,
    /// The source file to name: an uncommitted one first, else the one the
    /// last twenty commits touched most.
    pub recent_source: Option<String>,
}

const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "mix.exs",
    "composer.json",
    "Package.swift",
    "CMakeLists.txt",
];

const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "rb", "java", "kt", "swift", "c", "cc", "cpp", "h",
    "hpp", "cs", "ex", "exs", "php", "scala", "lua", "zig", "ml", "hs", "m", "mm",
];

fn is_source_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

impl CwdFacts {
    /// Read the directory and ask git twice at most: status, then the recent
    /// log when nothing uncommitted names a source file.
    pub fn gather(cwd: &Path) -> Self {
        let readme = std::fs::read_dir(cwd).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.to_ascii_uppercase().starts_with("README"))
            })
        });
        let manifest = MANIFESTS.iter().any(|file| cwd.join(file).exists());

        let git = |args: &[&str]| -> Option<String> {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
        };
        let status = git(&["status", "--porcelain", "--untracked-files=no"]);
        let git_repo = status.is_some();
        let dirty = status.as_deref().is_some_and(|out| !out.trim().is_empty());
        let mut recent_source = status.as_deref().and_then(|out| {
            out.lines()
                .filter_map(|line| line.get(3..))
                .map(|path| {
                    path.rsplit(" -> ")
                        .next()
                        .unwrap_or(path)
                        .trim()
                        .to_string()
                })
                .find(|path| is_source_file(path))
        });
        if recent_source.is_none() && git_repo {
            recent_source =
                git(&["log", "-20", "--name-only", "--format="]).and_then(|out| most_changed(&out));
        }
        Self {
            git_repo,
            readme,
            dirty,
            manifest,
            recent_source,
        }
    }
}

/// The source file named most often in a `git log --name-only` listing;
/// ties go to the one seen first.
fn most_changed(log: &str) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for path in log
        .lines()
        .map(str::trim)
        .filter(|path| is_source_file(path))
    {
        let count = counts.entry(path).or_insert(0);
        if *count == 0 {
            order.push(path);
        }
        *count += 1;
    }
    order
        .iter()
        .min_by_key(|path| std::cmp::Reverse(counts[*path]))
        .map(|path| (*path).to_string())
}

/// Up to three prompts for `facts`, most specific first. Nothing outside a
/// git repo: there is nothing to say about a bare directory. Pure.
pub fn starter_prompts(facts: &CwdFacts) -> Vec<String> {
    if !facts.git_repo {
        return Vec::new();
    }
    let mut prompts = Vec::new();
    if facts.readme {
        prompts.push("Explain how this project is put together".to_string());
    }
    if facts.dirty {
        prompts.push("Review my uncommitted changes".to_string());
    }
    if facts.manifest
        && let Some(file) = &facts.recent_source
    {
        prompts.push(format!("Add a test for {file}"));
    }
    if prompts.len() < 3 {
        prompts.push("Find the biggest source of complexity here".to_string());
    }
    prompts.truncate(3);
    prompts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_prompts_follow_the_directory() {
        // Outside a repo, nothing: a bare directory has nothing to ask about.
        assert!(starter_prompts(&CwdFacts::default()).is_empty());

        let project = CwdFacts {
            git_repo: true,
            readme: true,
            dirty: true,
            manifest: true,
            recent_source: Some("src/onboarding.rs".to_string()),
        };
        assert_eq!(
            starter_prompts(&project),
            [
                "Explain how this project is put together",
                "Review my uncommitted changes",
                "Add a test for src/onboarding.rs",
            ]
        );

        let clean = CwdFacts {
            dirty: false,
            ..project.clone()
        };
        assert_eq!(
            starter_prompts(&clean),
            [
                "Explain how this project is put together",
                "Add a test for src/onboarding.rs",
                "Find the biggest source of complexity here",
            ]
        );

        // A manifest with no source file to name gets no test prompt.
        let bare = CwdFacts {
            recent_source: None,
            readme: false,
            ..clean
        };
        assert_eq!(
            starter_prompts(&bare),
            ["Find the biggest source of complexity here"]
        );
    }

    #[test]
    fn the_most_changed_source_file_wins_and_ties_go_to_the_first_seen() {
        let log = "docs/a.md\nsrc/b.rs\n\nsrc/a.rs\nsrc/b.rs\n\nsrc/a.rs\nREADME.md\n";
        assert_eq!(most_changed(log).as_deref(), Some("src/b.rs"));
        assert_eq!(most_changed("docs/a.md\n"), None);
    }

    /// A repo built here, so the answer does not depend on what HEAD of this
    /// checkout happens to touch.
    #[test]
    fn cwd_facts_read_a_fixture_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@x")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@x")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("README.md"), "# demo\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);

        // Clean tree: the file from history.
        let facts = CwdFacts::gather(root);
        assert_eq!(
            facts,
            CwdFacts {
                git_repo: true,
                readme: true,
                dirty: false,
                manifest: true,
                recent_source: Some("src/lib.rs".to_string()),
            }
        );

        // A docs-only commit on top changes nothing about which source file
        // is named.
        std::fs::write(root.join("NOTES.md"), "notes\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "docs"]);
        assert_eq!(
            CwdFacts::gather(root).recent_source.as_deref(),
            Some("src/lib.rs")
        );

        // A dirty source file is named ahead of history.
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        git(&["add", "src/main.rs"]);
        git(&["commit", "-q", "-m", "main"]);
        std::fs::write(root.join("src/main.rs"), "fn main() { }\n").unwrap();
        let facts = CwdFacts::gather(root);
        assert!(facts.dirty);
        assert_eq!(facts.recent_source.as_deref(), Some("src/main.rs"));

        let nowhere = CwdFacts::gather(Path::new("/nonexistent/wizard-cwd"));
        assert_eq!(nowhere, CwdFacts::default());
    }

    #[test]
    fn this_checkout_is_a_repo_with_a_manifest() {
        let facts = CwdFacts::gather(Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(facts.git_repo);
        assert!(facts.manifest);
    }
}
