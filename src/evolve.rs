//! Drives AHE's *real* harness-evolution loop by shelling out to its
//! `scripts/evolve.sh` (which wraps `python evolve.py` in a detached tmux
//! session). Wizard owns none of AHE's configuration: the AHE checkout brings
//! its own `.env` (LLM keys), its `configs/`, and its dataset. AHE executes
//! harnesses on the **local Docker daemon** — there is no cloud sandbox, so the
//! only credentials a run needs are LLM keys. We only launch it, locate its
//! live status files, and surface progress.
//!
//! Status files AHE writes under `<ahe_repo>/experiments/<TIMESTAMP>__<name>/`:
//! `iteration_scores.md`, `evolution_history.md`, `iteration_scores.yaml`,
//! `best_ever.json`. We read the markdown directly — no YAML dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::EvolveConfig;

/// How many trailing lines of `iteration_scores.md` to show in a status
/// summary.
const SCORES_TAIL_LINES: usize = 16;

/// Resolve the experiment config path: absolute as-is, otherwise relative to
/// `ahe_repo` (the same place AHE's own `evolve.sh` resolves it).
fn experiment_config_path(cfg: &EvolveConfig) -> PathBuf {
    let raw = Path::new(&cfg.experiment_config);
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cfg.ahe_repo.join(raw)
    }
}

/// The `<ahe_repo>/experiments` directory (may not exist before the first run).
fn experiments_dir(cfg: &EvolveConfig) -> PathBuf {
    cfg.ahe_repo.join("experiments")
}

/// Verify everything AHE needs to be launchable from here. Reports *all*
/// missing pieces at once. AHE runs harnesses on the local Docker daemon — no
/// cloud sandbox — so the requirements are: the AHE scripts, a reachable Docker
/// daemon, the selected experiment config, and LLM keys (from `<repo>/.env` or
/// the inherited environment).
pub fn preflight(cfg: &EvolveConfig) -> Result<()> {
    let repo = &cfg.ahe_repo;
    let mut missing: Vec<String> = Vec::new();

    if !repo.is_dir() {
        bail!(
            "AHE checkout not found at {} — set [evolve] ahe_repo in ~/.wizard/config.toml",
            repo.display()
        );
    }

    let evolve_sh = repo.join("scripts").join("evolve.sh");
    if !evolve_sh.is_file() {
        missing.push(format!(
            "scripts/evolve.sh (looked at {})",
            evolve_sh.display()
        ));
    }

    let evolve_py = repo.join("evolve.py");
    if !evolve_py.is_file() {
        missing.push(format!("evolve.py (looked at {})", evolve_py.display()));
    }

    // Local Docker execution: AHE builds and runs each task's container on the
    // local daemon, so the `docker` CLI must be on PATH.
    if !docker_available() {
        missing.push(
            "`docker` CLI on PATH — AHE runs harnesses on the local Docker daemon \
             (install Docker and ensure `docker ps` works)"
                .to_string(),
        );
    }

    // LLM keys: the code agent needs a reachable LLM. They can live in the AHE
    // checkout's `.env` *or* be inherited from the environment. We require one
    // of those; we do NOT require any cloud-sandbox (E2B) or GitHub credentials.
    let env_file = repo.join(".env");
    if !env_file.is_file() && !llm_env_present() {
        missing.push(format!(
            ".env with LLM keys (looked at {}), or LLM_API_KEY/LLM_BASE_URL/LLM_MODEL \
             exported in the environment",
            env_file.display()
        ));
    }

    let exp_cfg = experiment_config_path(cfg);
    if !exp_cfg.is_file() {
        missing.push(format!(
            "experiment config '{}' (looked at {})",
            cfg.experiment_config,
            exp_cfg.display()
        ));
    }

    if missing.is_empty() {
        return Ok(());
    }

    bail!(
        "evolve preflight failed — missing:\n  - {}\n\nAHE runs fully locally: it needs \
         Docker + LLM keys, no cloud sandbox. Supply LLM keys via {}/.env (or the \
         environment) and pick an experiment under {}/configs/.",
        missing.join("\n  - "),
        repo.display(),
        repo.display(),
    )
}

/// Whether the `docker` CLI is available and the daemon responds (`docker ps`).
fn docker_available() -> bool {
    Command::new("docker")
        .arg("ps")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether the LLM keys are present in the inherited environment.
fn llm_env_present() -> bool {
    ["LLM_API_KEY", "LLM_BASE_URL", "LLM_MODEL"]
        .iter()
        .all(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// Launch AHE's evolve loop. Runs `bash scripts/evolve.sh <experiment_config>`
/// with the working directory set to `ahe_repo`, inheriting the environment so
/// AHE picks up its own `.env`/PATH. `evolve.sh` detaches into a tmux session
/// named `ahe-<name>-<ts>`; we return that session name.
pub fn start(cfg: &EvolveConfig) -> Result<String> {
    preflight(cfg)?;

    let before = sessions().unwrap_or_default();

    let output = Command::new("bash")
        .arg("scripts/evolve.sh")
        .arg(&cfg.experiment_config)
        .current_dir(&cfg.ahe_repo)
        .output()
        .with_context(|| format!("launching scripts/evolve.sh in {}", cfg.ahe_repo.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        bail!(
            "scripts/evolve.sh exited with {} —\n{}{}",
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }

    // Prefer the session name evolve.sh prints ("  Session:  ahe-…"); fall back
    // to whichever `ahe-*` session is new since we started.
    let session = parse_session_name(&stdout)
        .or_else(|| {
            sessions()
                .ok()
                .and_then(|after| after.into_iter().find(|s| !before.contains(s)))
        })
        .unwrap_or_else(|| "ahe-<unknown>".to_string());

    Ok(session)
}

/// Pull the tmux session name out of `evolve.sh`'s output (the `Session:` line).
fn parse_session_name(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let trimmed = line.trim();
        let rest = trimmed.strip_prefix("Session:")?;
        let name = rest.trim();
        (!name.is_empty()).then(|| name.to_string())
    })
}

/// Newest experiment directory under `<ahe_repo>/experiments/`, if any. The
/// directories are timestamp-prefixed, so newest = lexicographically greatest
/// name. `None` when nothing has run yet (or the directory is absent).
pub fn latest_experiment(cfg: &EvolveConfig) -> Option<PathBuf> {
    let dir = experiments_dir(cfg);
    let mut latest: Option<(String, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // AHE experiment dirs are prefixed `YYYY-MM-DD__HH-MM-SS`.
        if !name.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        if latest.as_ref().is_none_or(|(best, _)| name > *best) {
            latest = Some((name, entry.path()));
        }
    }
    latest.map(|(_, path)| path)
}

/// A human-readable progress summary for the latest experiment: the tail of
/// `iteration_scores.md` plus the last `evolution_history.md` section. Reads
/// the markdown AHE writes; never blocks on the running process.
pub fn status(cfg: &EvolveConfig) -> Result<String> {
    if !cfg.ahe_repo.is_dir() {
        bail!(
            "AHE checkout not found at {} — set [evolve] ahe_repo in ~/.wizard/config.toml",
            cfg.ahe_repo.display()
        );
    }

    let Some(exp) = latest_experiment(cfg) else {
        return Ok(format!(
            "no evolve experiments yet under {} — run `wizard evolve start` to begin",
            experiments_dir(cfg).display()
        ));
    };

    let mut out = String::new();
    out.push_str(&format!("experiment: {}\n", exp.display()));

    let active = sessions().unwrap_or_default();
    if active.is_empty() {
        out.push_str("tmux: no ahe-* sessions running\n");
    } else {
        out.push_str(&format!("tmux: {}\n", active.join(", ")));
    }

    match read_tail(&exp.join("iteration_scores.md"), SCORES_TAIL_LINES) {
        Some(tail) => {
            out.push_str("\n— iteration_scores.md (tail) —\n");
            out.push_str(&tail);
        }
        None => out.push_str("\n(no iteration_scores.md yet)\n"),
    }

    match last_history_section(&exp.join("evolution_history.md")) {
        Some(section) => {
            out.push_str("\n— evolution_history.md (latest) —\n");
            out.push_str(&section);
        }
        None => out.push_str("\n(no evolution_history.md yet)\n"),
    }

    Ok(out.trim_end().to_string())
}

/// Read the last `lines` lines of a file, or `None` if it cannot be read.
fn read_tail(path: &Path, lines: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    Some(all[start..].join("\n"))
}

/// Extract the final `## Iteration …` section of `evolution_history.md`, so
/// status shows the newest iteration's narrative rather than the whole log.
fn last_history_section(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let start = text
        .match_indices("\n## ")
        .last()
        .map(|(index, _)| index + 1)
        .or_else(|| text.starts_with("## ").then_some(0))?;
    Some(text[start..].trim_end().to_string())
}

/// Running tmux sessions whose names start with `ahe-`. An empty list when no
/// tmux server is running (rather than an error).
pub fn sessions() -> Result<Vec<String>> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .context("running `tmux list-sessions`")?;

    // tmux exits non-zero when no server is running — that just means none.
    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| name.starts_with("ahe-"))
        .map(str::to_string)
        .collect())
}

/// Kill a tmux session by name (`tmux kill-session -t <name>`).
pub fn stop(name: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .output()
        .context("running `tmux kill-session`")?;

    if !output.status.success() {
        bail!(
            "could not stop tmux session '{}': {}",
            name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(repo: &Path) -> EvolveConfig {
        EvolveConfig {
            ahe_repo: repo.to_path_buf(),
            experiment_config: "configs/experiments/exp.yaml".to_string(),
        }
    }

    #[test]
    fn experiment_config_resolves_relative_to_repo() {
        let cfg = cfg(Path::new("/srv/ahe"));
        assert_eq!(
            experiment_config_path(&cfg),
            PathBuf::from("/srv/ahe/configs/experiments/exp.yaml")
        );
    }

    #[test]
    fn experiment_config_keeps_absolute_paths() {
        let mut cfg = cfg(Path::new("/srv/ahe"));
        cfg.experiment_config = "/etc/ahe/exp.yaml".to_string();
        assert_eq!(
            experiment_config_path(&cfg),
            PathBuf::from("/etc/ahe/exp.yaml")
        );
    }

    #[test]
    fn preflight_reports_missing_repo() {
        let cfg = cfg(Path::new("/nonexistent/ahe-checkout"));
        let err = preflight(&cfg).unwrap_err().to_string();
        assert!(err.contains("AHE checkout not found"), "got: {err}");
    }

    #[test]
    fn preflight_lists_missing_pieces_in_a_real_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let err = preflight(&cfg(tmp.path())).unwrap_err().to_string();
        // These pieces are always absent in an empty temp dir, regardless of the
        // ambient environment (Docker presence / LLM_* vars vary by host).
        assert!(err.contains("scripts/evolve.sh"), "got: {err}");
        assert!(err.contains("evolve.py"), "got: {err}");
        assert!(err.contains("experiment config"), "got: {err}");
        // Local-Docker framing — no cloud sandbox / E2B requirement.
        assert!(err.contains("no cloud sandbox"), "got: {err}");
        assert!(!err.contains("E2B account"), "got: {err}");
    }

    #[test]
    fn parses_session_name_from_evolve_output() {
        let stdout = "  Config:   exp.yaml\n  Session:  ahe-simple-code-20260614-2031\n";
        assert_eq!(
            parse_session_name(stdout).as_deref(),
            Some("ahe-simple-code-20260614-2031")
        );
        assert_eq!(parse_session_name("nothing here"), None);
    }

    #[test]
    fn latest_experiment_picks_newest_timestamp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let exps = tmp.path().join("experiments");
        std::fs::create_dir_all(exps.join("2026-06-13__09-00-00__a")).unwrap();
        std::fs::create_dir_all(exps.join("2026-06-14__18-02-54__b")).unwrap();
        std::fs::create_dir_all(exps.join("not-an-experiment")).unwrap();
        let latest = latest_experiment(&cfg(tmp.path())).unwrap();
        assert!(latest.ends_with("2026-06-14__18-02-54__b"));
    }

    #[test]
    fn latest_experiment_is_none_without_runs() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(latest_experiment(&cfg(tmp.path())).is_none());
    }

    #[test]
    fn status_reports_no_experiments_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let summary = status(&cfg(tmp.path())).unwrap();
        assert!(
            summary.contains("no evolve experiments yet"),
            "got: {summary}"
        );
    }

    #[test]
    fn status_summarizes_latest_experiment_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        let exp = tmp
            .path()
            .join("experiments")
            .join("2026-06-14__18-02-54__run");
        std::fs::create_dir_all(&exp).unwrap();
        std::fs::write(
            exp.join("iteration_scores.md"),
            "# Iteration Scores\n\n| Iter | Pass |\n| 1 | 50% |\n",
        )
        .unwrap();
        std::fs::write(
            exp.join("evolution_history.md"),
            "# History\n\n## Iteration 1 — 2026-06-14 18:02\n- Pass rate: 50%\n\
             \n## Iteration 2 — 2026-06-14 18:30\n- Pass rate: 62%\n",
        )
        .unwrap();
        let summary = status(&cfg(tmp.path())).unwrap();
        assert!(summary.contains("Iteration Scores"), "got: {summary}");
        assert!(summary.contains("## Iteration 2"), "got: {summary}");
        assert!(!summary.contains("## Iteration 1"), "got: {summary}");
    }

    #[test]
    fn last_history_section_handles_single_section() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evolution_history.md");
        std::fs::write(&path, "## Iteration 1 — now\n- only one\n").unwrap();
        let section = last_history_section(&path).unwrap();
        assert!(section.starts_with("## Iteration 1"));
    }
}
