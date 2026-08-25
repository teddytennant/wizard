//! `wizard harness` subcommands: bundle tooling for external
//! harness-evolution loops (e.g. AHE).
//!
//! A *harness bundle* is a directory holding the evolvable surface of the
//! agent as plain files — `system_prompt.md`, `tool_descriptions/<tool>.md`,
//! `skills/<name>/SKILL.md`, `subagents/<name>.toml`. At runtime a bundle is
//! activated with `--harness-dir` / `$WIZARD_HARNESS_DIR` and each present
//! file shadows the corresponding compiled default (missing files fall back,
//! so a partial bundle degrades gracefully). `export` dumps the current
//! compiled defaults as a bundle, which is what makes the evolution loop
//! recursive: improvements merged back into the source become the next
//! export's baseline.

use std::path::Path;

use anyhow::{Context, Result};

use crate::agent::prompts::SOVEREIGN_SYSTEM_PROMPT;
use crate::agent::subagent;
use crate::cli::HarnessCmd;
use crate::tools::registry::ToolRegistry;

/// Dispatch a `wizard harness` subcommand.
pub fn run(cmd: HarnessCmd) -> Result<()> {
    match cmd {
        HarnessCmd::Export { dir } => export(&dir),
    }
}

/// Write the compiled harness defaults into `dir` as a bundle. Existing
/// files are overwritten; unrelated files already in `dir` are left alone.
pub fn export(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    // System prompt: the sovereign personality. Evolution loops drive
    // headless (`wizard -p`) runs, which are sovereign; the charter and
    // skills/instructions/memory sections are appended on top at runtime and
    // are not part of the evolvable base prompt.
    std::fs::write(
        dir.join("system_prompt.md"),
        format!("{}\n", SOVEREIGN_SYSTEM_PROMPT.trim_end()),
    )?;

    // Tool descriptions: one markdown file per compiled-in tool, named after
    // the tool. Scripted and MCP tools are runtime-dependent and excluded;
    // plugin tools are not, because a plugin is compiled in exactly as a
    // native tool is and a bundle that skipped them would leave the model's
    // biggest descriptions unoverridable. Which tools those are is a property
    // of the build: leave `tool-web` out and the bundle has no `web_fetch.md`,
    // because that build has no `web_fetch`.
    let descriptions = dir.join("tool_descriptions");
    std::fs::create_dir_all(&descriptions)?;
    let mut registry = ToolRegistry::with_native_tools();
    crate::plugins::install_tools_into(&mut registry);
    let mut tool_count = 0usize;
    for spec in registry.specs() {
        std::fs::write(
            descriptions.join(format!("{}.md", spec.function.name)),
            format!("{}\n", spec.function.description.trim_end()),
        )?;
        tool_count += 1;
    }

    // Skills: copy the bundled skill directories (repo checkout or
    // exe-adjacent install), first found wins per name. User skills under
    // ~/.wizard/skills are the user's, not compiled defaults.
    let skills_out = dir.join("skills");
    std::fs::create_dir_all(&skills_out)?;
    let mut skill_count = 0usize;
    for root in bundled_skill_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let src = entry.path();
            if !src.join("SKILL.md").is_file() {
                continue;
            }
            let Some(name) = src.file_name() else {
                continue;
            };
            let dst = skills_out.join(name);
            if dst.exists() {
                continue; // earlier root already provided this skill
            }
            copy_dir(&src, &dst)?;
            skill_count += 1;
        }
    }

    // Subagents: the shipped loadout TOMLs when a loadout is discoverable,
    // plus the built-in definitions (serialized) for names the loadout does
    // not cover.
    let subagents_out = dir.join("subagents");
    std::fs::create_dir_all(&subagents_out)?;
    let mut subagent_count = 0usize;
    if let Some(loadout) = loadout_subagents_dir() {
        for entry in std::fs::read_dir(&loadout)?.flatten() {
            let src = entry.path();
            if src.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            if let Some(name) = src.file_name() {
                std::fs::copy(&src, subagents_out.join(name))?;
                subagent_count += 1;
            }
        }
    }
    for config in subagent::builtin_configs() {
        let path = subagents_out.join(format!("{}.toml", config.name));
        if path.exists() {
            continue;
        }
        let toml = toml::to_string_pretty(&config)
            .with_context(|| format!("serializing builtin subagent '{}'", config.name))?;
        std::fs::write(path, toml)?;
        subagent_count += 1;
    }

    std::fs::write(dir.join("HARNESS.md"), harness_doc())?;

    println!(
        "exported harness bundle to {}: system_prompt.md, {tool_count} tool descriptions, \
         {skill_count} skills, {subagent_count} subagents, HARNESS.md",
        dir.display()
    );
    Ok(())
}

/// Bundled (non-user) skill roots: the repo checkout and exe-adjacent
/// installs — the same discovery as `skills::default_roots` minus the user
/// and harness roots, since export captures compiled defaults only.
fn bundled_skill_roots() -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
    if manifest.is_dir() {
        roots.push(manifest);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for candidate in [dir.join("skills"), dir.join("../share/wizard/skills")] {
            if candidate.is_dir() && !roots.contains(&candidate) {
                roots.push(candidate);
            }
        }
    }
    roots
}

/// The shipped `loadout/subagents/` directory, when discoverable (repo
/// checkout or exe-adjacent install).
fn loadout_subagents_dir() -> Option<std::path::PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("loadout/subagents");
    if manifest.is_dir() {
        return Some(manifest);
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("loadout/subagents"),
        dir.join("../share/wizard/loadout/subagents"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_dir())
}

/// Recursively copy `src` into `dst` (regular files and directories only).
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else if from.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// The generated bundle guide. Rides inside every exported bundle so a
/// harness-evolution agent working on the bundle knows what each component
/// is and how edits take effect.
fn harness_doc() -> &'static str {
    "\
# Wizard harness bundle

This directory is a *harness bundle*: the evolvable surface of the wizard
agent, externalized as plain files. Wizard loads it when started with
`--harness-dir <this dir>` (or `$WIZARD_HARNESS_DIR`); every component
present here shadows the corresponding compiled default, and a missing or
empty file falls back to that default.

## Components

- `system_prompt.md` — the base personality prompt (sovereign mode). The
  wizard charter, skills index, project instructions, and memory sections
  are appended on top at runtime and cannot be edited from here.
- `tool_descriptions/<tool>.md` — the description advertised to the model
  for the named native tool. Only the description is overridable; tool
  behavior, parameters, and access class are compiled in.
- `skills/<name>/SKILL.md` — skills listed in the prompt's skills index
  (name, description, path). The body is read from disk when the skill
  matches, unless the skill sets `always: true`. Bundle skills shadow
  bundled and user skills by name; new directories add new skills.
- `subagents/<name>.toml` — spawnable subagent definitions (`name`,
  `description`, `system_prompt`, optional `tool_scope`; optional `max_steps`
  only if you want a hard cap — default is unlimited).
  Bundle definitions shadow user-defined and built-in ones by name.

## Editing rules for evolution loops

- Keep names stable: a `tool_descriptions/` file must keep the exact tool
  name as its stem, a subagent TOML must keep its `name` field matching new
  file names you introduce.
- Edits take effect on the next wizard start (or `/reload` in a session);
  no rebuild is required.
- Deleting a file reverts that component to the compiled default, so
  destructive experiments are always recoverable.
"
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::agent::subagent::SubagentConfig;

    #[tokio::test]
    async fn export_writes_a_complete_bundle() {
        // `export` is synchronous and runs from `crate::run`, which has already
        // called `plugins::boot`; a test binary has not, so it says so here.
        // Without this the bundle would be exported and checked against a
        // registry that was missing the same tools, and would agree with
        // itself about a bundle the real export never writes.
        crate::plugins::bundled::ensure().await;
        let dir = tempfile::tempdir().expect("tempdir");
        export(dir.path()).expect("export");

        let prompt =
            std::fs::read_to_string(dir.path().join("system_prompt.md")).expect("system prompt");
        assert!(!prompt.trim().is_empty());
        assert!(prompt.ends_with('\n'));

        // One non-empty description file per compiled-in tool, named after the
        // tool — the "keep names stable" contract HARNESS.md documents. Built
        // the same way the export builds it, plugin tools included, so a build
        // without `tool-web` expects no `web_fetch.md` and finds none.
        let expected: BTreeSet<String> = {
            let mut registry = ToolRegistry::with_native_tools();
            crate::plugins::install_tools_into(&mut registry);
            registry
                .specs()
                .iter()
                .map(|spec| spec.function.name.clone())
                .collect()
        };
        let descriptions = dir.path().join("tool_descriptions");
        let mut exported = BTreeSet::new();
        for entry in std::fs::read_dir(&descriptions).expect("descriptions dir") {
            let path = entry.expect("entry").path();
            assert_eq!(path.extension().and_then(|e| e.to_str()), Some("md"));
            let body = std::fs::read_to_string(&path).expect("description");
            assert!(!body.trim().is_empty(), "empty description: {path:?}");
            exported.insert(
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .expect("stem")
                    .to_string(),
            );
        }
        assert_eq!(exported, expected);

        let doc = std::fs::read_to_string(dir.path().join("HARNESS.md")).expect("HARNESS.md");
        assert!(doc.contains("--harness-dir"));

        // The builtin worker subagent round-trips, and every exported TOML
        // keeps its `name` matching the file stem.
        let subagents = dir.path().join("subagents");
        let worker = std::fs::read_to_string(subagents.join("worker.toml")).expect("worker.toml");
        let worker: SubagentConfig = toml::from_str(&worker).expect("worker parses");
        assert_eq!(worker.name, "worker");
        for entry in std::fs::read_dir(&subagents).expect("subagents dir") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).expect("subagent toml");
            let config: SubagentConfig = toml::from_str(&raw).expect("subagent parses");
            assert_eq!(
                Some(config.name.as_str()),
                path.file_stem().and_then(|s| s.to_str()),
                "subagent name must match its file stem: {path:?}"
            );
        }

        // Every exported skill directory carries its SKILL.md.
        for entry in std::fs::read_dir(dir.path().join("skills")).expect("skills dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                assert!(
                    path.join("SKILL.md").is_file(),
                    "skill without SKILL.md: {path:?}"
                );
            }
        }
    }

    #[test]
    fn export_overwrites_stale_components_and_keeps_unrelated_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("system_prompt.md"), "stale").expect("seed");
        std::fs::write(dir.path().join("keep.txt"), "mine").expect("seed");
        export(dir.path()).expect("export");
        let prompt =
            std::fs::read_to_string(dir.path().join("system_prompt.md")).expect("system prompt");
        assert_ne!(prompt, "stale");
        let kept = std::fs::read_to_string(dir.path().join("keep.txt")).expect("keep.txt");
        assert_eq!(kept, "mine");
    }

    #[test]
    fn copy_dir_recurses_into_nested_directories() {
        let root = tempfile::tempdir().expect("tempdir");
        let src = root.path().join("src");
        std::fs::create_dir_all(src.join("sub/deep")).expect("mkdirs");
        std::fs::write(src.join("a.txt"), "top").expect("write");
        std::fs::write(src.join("sub/deep/b.txt"), "nested").expect("write");
        let dst = root.path().join("dst");
        copy_dir(&src, &dst).expect("copy");
        assert_eq!(
            std::fs::read_to_string(dst.join("a.txt")).expect("a"),
            "top"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/deep/b.txt")).expect("b"),
            "nested"
        );
    }
}
