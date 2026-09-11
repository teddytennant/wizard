//! Starter prompts: what an empty session suggests, read off the working
//! directory with no model call.

use std::path::Path;

/// What the starter prompts are read off. Gathered once at startup, from the
/// directory alone: no model call.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CwdFacts {
    pub git_repo: bool,
    pub readme: bool,
    /// Tracked files with uncommitted changes.
    pub dirty: bool,
    pub ci_config: bool,
    /// A package manifest (Cargo.toml, package.json, pyproject.toml, …).
    pub manifest: bool,
    /// The source file changed most recently: an uncommitted one first, else
    /// one from the last commit.
    pub recent_source: Option<String>,
}

const CI_FILES: &[&str] = &[
    ".github/workflows",
    ".gitlab-ci.yml",
    ".circleci/config.yml",
    "Jenkinsfile",
    ".travis.yml",
    "azure-pipelines.yml",
    "bitbucket-pipelines.yml",
];

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
    /// Read the directory and ask git twice at most (status, then the last
    /// commit's file list when nothing is uncommitted).
    pub fn gather(cwd: &Path) -> Self {
        let readme = std::fs::read_dir(cwd).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.to_ascii_uppercase().starts_with("README"))
            })
        });
        let ci_config = CI_FILES.iter().any(|file| cwd.join(file).exists());
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
            recent_source = git(&["log", "-1", "--name-only", "--format="]).and_then(|out| {
                out.lines()
                    .map(str::trim)
                    .find(|path| is_source_file(path))
                    .map(str::to_string)
            });
        }
        Self {
            git_repo,
            readme,
            dirty,
            ci_config,
            manifest,
            recent_source,
        }
    }
}

/// Up to three prompts for `facts`, most specific first, filled from the
/// generic ones. Pure.
pub fn starter_prompts(facts: &CwdFacts) -> Vec<String> {
    let mut prompts = Vec::new();
    if facts.git_repo && facts.readme {
        prompts.push("Explain how this project is put together".to_string());
    }
    if facts.dirty {
        prompts.push("Review my uncommitted changes".to_string());
    }
    if facts.ci_config {
        prompts.push("Why is CI failing".to_string());
    }
    if facts.manifest
        && let Some(file) = &facts.recent_source
    {
        prompts.push(format!("Add a test for {file}"));
    }
    let generic = if facts.git_repo && facts.readme {
        &[
            "What can you do?",
            "Find the biggest source of complexity here",
        ][..]
    } else {
        &[
            "Explain what is in this directory",
            "What can you do?",
            "Start a new project here",
        ][..]
    };
    for prompt in generic {
        if prompts.len() >= 3 {
            break;
        }
        prompts.push((*prompt).to_string());
    }
    prompts.truncate(3);
    prompts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_prompts_follow_the_directory() {
        let empty = starter_prompts(&CwdFacts::default());
        assert_eq!(
            empty,
            [
                "Explain what is in this directory",
                "What can you do?",
                "Start a new project here",
            ]
        );

        let project = CwdFacts {
            git_repo: true,
            readme: true,
            dirty: true,
            ci_config: true,
            manifest: true,
            recent_source: Some("src/onboarding.rs".to_string()),
        };
        assert_eq!(
            starter_prompts(&project),
            [
                "Explain how this project is put together",
                "Review my uncommitted changes",
                "Why is CI failing",
            ]
        );

        let clean = CwdFacts {
            dirty: false,
            ci_config: false,
            ..project.clone()
        };
        assert_eq!(
            starter_prompts(&clean),
            [
                "Explain how this project is put together",
                "Add a test for src/onboarding.rs",
                "What can you do?",
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
            [
                "Explain what is in this directory",
                "What can you do?",
                "Start a new project here",
            ]
        );
    }

    #[test]
    fn cwd_facts_read_this_checkout() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let facts = CwdFacts::gather(root);
        assert!(facts.git_repo);
        assert!(facts.readme);
        assert!(facts.ci_config);
        assert!(facts.manifest);
        assert!(
            facts.recent_source.as_deref().is_some_and(is_source_file),
            "{facts:?}"
        );
        let nowhere = CwdFacts::gather(Path::new("/nonexistent/wizard-cwd"));
        assert_eq!(nowhere, CwdFacts::default());
    }
}
