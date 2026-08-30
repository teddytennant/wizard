//! Integration tests for the compiled `wizard` binary.
//!
//! Every invocation runs with `HOME` pointed at a throwaway directory so the
//! binary's `~/.wizard` tree is created there and the real one is never
//! touched (`dirs::home_dir()` honors `$HOME` on Linux).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Temp dir removed on drop. Serves as both fake `$HOME` and project root.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        // A per-process counter so one test can hold several dirs at once
        // (e.g. two fake homes for a sync round trip).
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wizard-itest-{}-{:?}-{seq}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run the compiled binary with `args`, an isolated `$HOME`, and the wizard
/// env overrides cleared (unless re-set via `envs`).
fn run_wizard(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wizard"));
    command
        .args(args)
        .env("HOME", home)
        .env_remove("WIZARD_MODEL")
        .env_remove("WIZARD_OLLAMA_HOST")
        .env_remove("WIZARD_LLAMACPP_HOST")
        .env_remove("WIZARD_GGUF_PATH")
        .env_remove("WIZARD_SYSTEM_PROMPT")
        .env_remove("WIZARD_HARNESS_DIR")
        .current_dir(home);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("binary runs")
}

#[test]
fn help_prints_usage_and_documented_flags() {
    let home = TempDir::new();
    let output = run_wizard(&home.0, &["--help"], &[]);

    assert!(output.status.success(), "--help must exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage:"), "help shows usage:\n{stdout}");
    for flag in [
        "--mode",
        "--prompt",
        "--evolve",
        "--deep",
        "--max-hours",
        "--loop",
        "--cwd",
        "--resume",
    ] {
        assert!(
            stdout.contains(flag),
            "help must document {flag}:\n{stdout}"
        );
    }
    assert!(
        !home.0.join(".wizard").exists(),
        "--help must not create state"
    );
}

#[test]
fn version_prints_name_and_version() {
    let home = TempDir::new();
    let output = run_wizard(&home.0, &["--version"], &[]);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("wizard "), "got: {stdout}");
}

#[cfg(feature = "provider-ollama")]
#[test]
fn unreachable_ollama_provider_fails_with_actionable_error() {
    let home = TempDir::new();
    // Port 1 on localhost: connection refused immediately, no server needed.
    let bogus = "http://127.0.0.1:1";
    // Ollama is opt-in: only an explicit provider entry selects it.
    write_config(
        &home.0,
        "[[providers]]\n\
         name = \"local\"\n\
         kind = \"ollama\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         model = \"qwen3.5:9b\"\n",
    );
    let output = run_wizard(&home.0, &["--mode", "sovereign", "-p", "do nothing"], &[]);

    assert!(
        !output.status.success(),
        "an unreachable host must be a failure"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "clean exit code, not a crash"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"), "stderr: {stderr}");
    assert!(
        stderr.contains(bogus),
        "error must name the configured host:\n{stderr}"
    );
    assert!(
        stderr.contains("ollama serve"),
        "error must tell the user how to fix it:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "must fail gracefully, not panic:\n{stderr}"
    );
}

/// The three tests below drive a real binary against a local backend, so
/// each needs its plugin compiled in: without it the config resolves to a kind
/// nothing answers to and the error is the registry's, not the transport's.
/// That degrade is asserted in `plugins::a_kind_is_installed_exactly_when_its_plugin_is_compiled_in`.
#[cfg(feature = "provider-llamacpp")]
#[test]
fn unreachable_llamacpp_host_fails_with_actionable_error() {
    let home = TempDir::new();
    // Port 1 on localhost: connection refused immediately, no server needed.
    let bogus = "http://127.0.0.1:1";
    // An empty PATH guarantees auto-spawn is impossible even on machines
    // that have llama-server installed, so the failure is deterministic.
    let output = run_wizard(
        &home.0,
        &["--mode", "sovereign", "-p", "do nothing"],
        &[("WIZARD_LLAMACPP_HOST", bogus), ("PATH", "/nonexistent")],
    );

    assert!(
        !output.status.success(),
        "an unreachable host must be a failure"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "clean exit code, not a crash"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"), "stderr: {stderr}");
    assert!(
        stderr.contains(bogus),
        "error must name the configured host:\n{stderr}"
    );
    assert!(
        stderr.contains("llama-server"),
        "error must tell the user how to fix it:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "must fail gracefully, not panic:\n{stderr}"
    );
}

/// Write `~/.wizard/config.toml` under the fake home.
fn write_config(home: &Path, contents: &str) {
    let dir = home.join(".wizard");
    std::fs::create_dir_all(&dir).expect("create .wizard dir");
    std::fs::write(dir.join("config.toml"), contents).expect("write config.toml");
}

#[cfg(feature = "provider-llamacpp")]
#[test]
fn fresh_config_resolves_to_the_llamacpp_provider() {
    let home = TempDir::new();
    // A config written by current versions always carries `llamacpp_host`;
    // point it at port 1 so the probe fails instantly instead of touching
    // whatever might really be listening on the default port.
    write_config(&home.0, "llamacpp_host = \"http://127.0.0.1:1\"\n");
    let output = run_wizard(
        &home.0,
        &["--mode", "sovereign", "-p", "do nothing"],
        &[("PATH", "/nonexistent")],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("llama-server") && stderr.contains("http://127.0.0.1:1"),
        "the synthesized provider must be llama.cpp at the configured host:\n{stderr}"
    );
    assert!(
        !stderr.contains("ollama serve"),
        "a fresh config must not resolve to Ollama:\n{stderr}"
    );
}

#[cfg(feature = "provider-llamacpp")]
#[test]
fn legacy_ollama_config_resolves_to_llamacpp() {
    let home = TempDir::new();
    // A pre-llama.cpp config: legacy top-level keys, none of the new ones.
    // The synthesized local provider is llama.cpp regardless — Ollama is
    // opt-in via an explicit [[providers]] entry.
    write_config(
        &home.0,
        "model = \"qwen3.5:9b\"\nollama_host = \"http://127.0.0.1:1\"\n",
    );
    // Point llama.cpp at port 1 (instant refusal) and empty the PATH so
    // auto-spawn is impossible: the failure is deterministic even on
    // machines with a real llama-server on the default port.
    let output = run_wizard(
        &home.0,
        &["--mode", "sovereign", "-p", "do nothing"],
        &[
            ("WIZARD_LLAMACPP_HOST", "http://127.0.0.1:1"),
            ("PATH", "/nonexistent"),
        ],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("llama-server") && stderr.contains("http://127.0.0.1:1"),
        "a legacy config must resolve to llama.cpp:\n{stderr}"
    );
    assert!(
        !stderr.contains("ollama serve"),
        "a legacy config must not resolve to Ollama:\n{stderr}"
    );
}

#[test]
fn missing_config_without_a_tty_points_at_onboarding() {
    let home = TempDir::new();
    // `Command::output` pipes stdout/stderr, so this is non-interactive: no
    // config must not fall through to a local provider probe (Ollama or
    // llama.cpp).
    let output = run_wizard(&home.0, &[], &[]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("onboard") || stderr.contains("config"),
        "error must point at setup:\n{stderr}"
    );
    assert!(
        !stderr.contains("ollama serve"),
        "a fresh install must not require Ollama:\n{stderr}"
    );
    assert!(
        !stderr.contains("llama-server"),
        "a fresh install must not probe llama-server before setup:\n{stderr}"
    );
}

#[test]
fn headless_mode_without_a_prompt_is_an_actionable_error() {
    let home = TempDir::new();
    write_config(&home.0, "llamacpp_host = \"http://127.0.0.1:1\"\n");
    let output = run_wizard(
        &home.0,
        &["--mode", "sovereign"],
        &[("PATH", "/nonexistent")],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("-p"),
        "error must point at the missing -p flag:\n{stderr}"
    );
}

#[test]
fn schedule_add_list_remove_round_trip() {
    let home = TempDir::new();
    let cwd = home.0.to_string_lossy().to_string();

    let output = run_wizard(
        &home.0,
        &[
            "schedule",
            "add",
            "nightly",
            "--cron",
            "0 3 * * *",
            "--prompt",
            "tidy the repo",
            "--cwd",
            &cwd,
        ],
        &[],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "add must succeed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("next fire"),
        "add must print the next fire time:\n{stdout}"
    );
    assert!(
        home.0.join(".wizard").join("schedule.toml").exists(),
        "add must persist schedule.toml"
    );

    let output = run_wizard(&home.0, &["schedule", "list"], &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("nightly") && stdout.contains("0 3 * * *"),
        "list must show the entry:\n{stdout}"
    );

    let output = run_wizard(&home.0, &["schedule", "remove", "nightly"], &[]);
    assert!(output.status.success(), "remove must succeed");

    let output = run_wizard(&home.0, &["schedule", "list"], &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no entries"),
        "list after remove must be empty:\n{stdout}"
    );

    let output = run_wizard(&home.0, &["schedule", "remove", "nightly"], &[]);
    assert!(
        !output.status.success(),
        "removing a missing entry must fail"
    );
}

#[test]
fn schedule_enable_disable_round_trip() {
    let home = TempDir::new();
    let cwd = home.0.to_string_lossy().to_string();

    let output = run_wizard(
        &home.0,
        &[
            "schedule",
            "add",
            "nightly",
            "--cron",
            "0 3 * * *",
            "--prompt",
            "tidy",
            "--cwd",
            &cwd,
        ],
        &[],
    );
    assert!(output.status.success(), "add must succeed");

    let output = run_wizard(&home.0, &["schedule", "disable", "nightly"], &[]);
    assert!(output.status.success(), "disable must succeed");
    let output = run_wizard(&home.0, &["schedule", "list"], &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no "),
        "list must show the entry disabled:\n{stdout}"
    );

    let output = run_wizard(&home.0, &["schedule", "enable", "nightly"], &[]);
    assert!(output.status.success(), "enable must succeed");
    let output = run_wizard(&home.0, &["schedule", "list"], &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("yes"),
        "list must show the entry enabled:\n{stdout}"
    );

    let output = run_wizard(&home.0, &["schedule", "enable", "missing"], &[]);
    assert!(
        !output.status.success(),
        "enabling a missing entry must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no entry"), "stderr: {stderr}");
}

#[test]
fn top_level_flags_with_a_subcommand_are_rejected_loudly() {
    let home = TempDir::new();
    let output = run_wizard(&home.0, &["--plan", "fleet", "status"], &[]);
    assert!(
        !output.status.success(),
        "--plan with a subcommand must be an error, not silently dropped"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--plan"), "error names the flag:\n{stderr}");

    // --cwd is honored everywhere and stays allowed.
    let cwd = home.0.to_string_lossy().to_string();
    let output = run_wizard(&home.0, &["--cwd", &cwd, "schedule", "list"], &[]);
    assert!(
        output.status.success(),
        "--cwd with a subcommand stays allowed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn usage_reports_empty_and_rolls_up_records() {
    let home = TempDir::new();
    let output = run_wizard(&home.0, &["usage"], &[]);
    assert!(output.status.success(), "usage with no log must exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no usage recorded"),
        "empty message:\n{stdout}"
    );

    let dir = home.0.join(".wizard");
    std::fs::create_dir_all(&dir).expect("create .wizard");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        dir.join("usage.jsonl"),
        format!(
            "{{\"ts\":{now},\"project\":\"/proj/a\",\"model\":\"m\",\"provider\":\"local\",\"prompt_tokens\":100,\"completion_tokens\":10,\"mode\":\"genie\"}}\n\
             {{\"ts\":{now},\"project\":\"/proj/b\",\"model\":\"m\",\"provider\":\"claude\",\"prompt_tokens\":200,\"completion_tokens\":20,\"mode\":\"sovereign\"}}\n\
             {{\"ts\":10,\"project\":\"/proj/old\",\"model\":\"m\",\"provider\":\"old\",\"prompt_tokens\":1,\"completion_tokens\":1,\"mode\":\"genie\"}}\n"
        ),
    )
    .expect("write usage log");

    let output = run_wizard(&home.0, &["usage"], &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("3 turn(s)"), "all time:\n{stdout}");
    assert!(stdout.contains("/proj/a"), "per-project rows:\n{stdout}");
    assert!(stdout.contains("claude"), "per-provider rows:\n{stdout}");

    let output = run_wizard(&home.0, &["usage", "--since", "7d"], &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("2 turn(s)"), "windowed:\n{stdout}");
    assert!(
        !stdout.contains("/proj/old"),
        "ancient record filtered out:\n{stdout}"
    );

    let output = run_wizard(&home.0, &["usage", "--since", "soon"], &[]);
    assert!(!output.status.success(), "bad --since must be rejected");
}

#[test]
fn evolve_list_and_undo_work_against_the_history_log() {
    let home = TempDir::new();
    let output = run_wizard(&home.0, &["evolve", "list"], &[]);
    assert!(output.status.success(), "empty history exits 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no evolutions recorded"),
        "empty message:\n{stdout}"
    );

    // Two recorded evolutions: an old skill and a newer one.
    let dir = home.0.join(".wizard");
    let skill_dir = dir.join("skills").join("newer");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(&skill_path, "body").expect("write skill");
    let event = |name: &str, path: &std::path::Path, ts: &str| {
        format!(
            "{{\"timestamp\":\"{ts}\",\"tier\":\"runtime\",\"description\":\"add {name}\",\
             \"outcome\":{{\"kind\":\"skill_added\",\"name\":\"{name}\",\"path\":{}}}}}",
            serde_json::to_string(path).unwrap()
        )
    };
    std::fs::write(
        dir.join("evolution.jsonl"),
        format!(
            "{}\n{}\n",
            event(
                "older",
                &dir.join("skills/older/SKILL.md"),
                "2026-01-01T00:00:00Z"
            ),
            event("newer", &skill_path, "2026-02-01T00:00:00Z"),
        ),
    )
    .expect("write history");

    let output = run_wizard(&home.0, &["evolve", "list"], &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("add newer"), "history listed:\n{stdout}");
    let newer_pos = stdout.find("add newer").unwrap();
    let older_pos = stdout.find("add older").unwrap();
    assert!(newer_pos < older_pos, "most recent first:\n{stdout}");

    // Undo #1 (the newer skill): removes its file.
    let output = run_wizard(&home.0, &["evolve", "undo", "1"], &[]);
    assert!(
        output.status.success(),
        "undo must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!skill_path.exists(), "undo deletes the skill file");

    // Undo #2: its artifacts never existed — refuse clearly.
    let output = run_wizard(&home.0, &["evolve", "undo", "2"], &[]);
    assert!(!output.status.success(), "undo of missing artifacts fails");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already gone"), "stderr: {stderr}");

    // Out-of-range index.
    let output = run_wizard(&home.0, &["evolve", "undo", "9"], &[]);
    assert!(!output.status.success(), "out-of-range undo fails");
}

#[test]
fn schedule_add_rejects_a_bad_cron_expression() {
    let home = TempDir::new();
    let cwd = home.0.to_string_lossy().to_string();
    let output = run_wizard(
        &home.0,
        &[
            "schedule", "add", "broken", "--cron", "whenever", "--prompt", "x", "--cwd", &cwd,
        ],
        &[],
    );
    assert!(!output.status.success(), "a bad cron must be rejected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cron"),
        "error must mention the cron expression:\n{stderr}"
    );
    assert!(
        !home.0.join(".wizard").join("schedule.toml").exists(),
        "nothing may be persisted on a failed add"
    );
}

/// Real inference end to end: auto-spawn llama-server, load a GGUF, run one
/// sovereign turn. Opt-in only — set `WIZARD_E2E_GGUF` to a local GGUF path
/// (small models recommended); skipped otherwise so `cargo test` never loads
/// a model.
#[test]
fn e2e_inference_with_auto_spawned_llama_server() {
    let Some(gguf) = std::env::var("WIZARD_E2E_GGUF")
        .ok()
        .filter(|path| !path.trim().is_empty())
    else {
        eprintln!("skipping: set WIZARD_E2E_GGUF=/path/to/model.gguf to run");
        return;
    };
    assert!(
        Path::new(&gguf).exists(),
        "WIZARD_E2E_GGUF points at a missing file: {gguf}"
    );
    assert!(
        Command::new("llama-server")
            .arg("--version")
            .output()
            .is_ok(),
        "WIZARD_E2E_GGUF is set but llama-server is not on PATH"
    );

    let home = TempDir::new();
    // An uncommon port so the test never collides with a llama-server the
    // developer is actually running on the default 11435.
    let host = "http://127.0.0.1:18434";
    let output = run_wizard(
        &home.0,
        &[
            "--mode",
            "sovereign",
            "--loop",
            "1",
            "--max-hours",
            "0.2",
            "-p",
            "Reply with the single word DONE. Do not use any tools.",
        ],
        &[("WIZARD_LLAMACPP_HOST", host), ("WIZARD_GGUF_PATH", &gguf)],
    );

    // The spawned server deliberately outlives wizard; stop it before any
    // assertion can bail out of the test.
    let pid_file = home.0.join(".wizard").join("llama-server.pid");
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        let _ = Command::new("kill").arg(pid.trim()).status();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "sovereign run must succeed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("started llama-server"),
        "wizard must report the server it spawned:\n{stdout}"
    );
    assert!(!stderr.contains("panicked"), "must not panic:\n{stderr}");
}

#[test]
fn sync_key_prints_public_key_and_fingerprint() {
    let home = TempDir::new();
    let output = run_wizard(&home.0, &["sync", "key"], &[]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "sync key must exit 0.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("public key:"), "{stdout}");
    assert!(stdout.contains("SHA256:"), "fingerprint printed:\n{stdout}");
    assert!(
        home.0.join(".wizard/sync/key").is_file(),
        "the keypair is generated on first use"
    );

    // A second invocation reuses the key: identical output.
    let again = run_wizard(&home.0, &["sync", "key"], &[]);
    assert!(again.status.success());
    assert_eq!(output.stdout, again.stdout, "the key is stable");
}

#[test]
fn sync_pack_and_pull_round_trip_across_homes() {
    let home_a = TempDir::new();
    let home_b = TempDir::new();

    // Seed machine A with portable state.
    write_config(&home_a.0, "model = \"qwen3.6:27b\"\n");
    let skill = home_a.0.join(".wizard/skills/demo/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).expect("create skill dir");
    std::fs::write(&skill, "# demo skill\n").expect("write skill");

    // Pack on A.
    let bundle = home_a.0.join("bundle.tar.gz");
    let bundle_str = bundle.to_string_lossy().to_string();
    let output = run_wizard(&home_a.0, &["sync", "pack", "--out", &bundle_str], &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pack must exit 0.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("packed"), "pack summary:\n{stdout}");
    assert!(stdout.contains("SHA256:"), "signing fingerprint:\n{stdout}");
    assert!(
        stdout.contains("not included"),
        "credentials excluded by default:\n{stdout}"
    );
    assert!(bundle.is_file(), "the bundle file exists");

    // Dry-run pull on B: verified, reported, nothing written.
    let output = run_wizard(&home_b.0, &["sync", "pull", &bundle_str, "--dry-run"], &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "dry-run pull must exit 0.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("signature: OK"), "{stdout}");
    assert!(stdout.contains("dry run"), "{stdout}");
    assert!(
        !home_b.0.join(".wizard/config.toml").exists(),
        "dry run writes nothing"
    );
    assert!(
        !home_b.0.join(".wizard/sync/trusted_keys").exists(),
        "dry run pins nothing"
    );

    // Real pull on B: files arrive, the key is pinned (TOFU).
    let output = run_wizard(&home_b.0, &["sync", "pull", &bundle_str], &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pull must exit 0.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("applied"), "apply summary:\n{stdout}");
    assert_eq!(
        std::fs::read_to_string(home_b.0.join(".wizard/config.toml")).expect("config arrived"),
        "model = \"qwen3.6:27b\"\n"
    );
    assert_eq!(
        std::fs::read_to_string(home_b.0.join(".wizard/skills/demo/SKILL.md"))
            .expect("skill arrived"),
        "# demo skill\n"
    );
    let pinned = std::fs::read_to_string(home_b.0.join(".wizard/sync/trusted_keys"))
        .expect("trusted_keys pinned");
    assert!(pinned.contains("# pinned"), "pin comment present: {pinned}");
}

#[test]
fn sync_pull_without_a_source_is_an_actionable_error() {
    let home = TempDir::new();
    let output = run_wizard(&home.0, &["sync", "pull"], &[]);
    assert!(!output.status.success(), "no source anywhere must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[sync]") && stderr.contains("source"),
        "error points at the config key:\n{stderr}"
    );
}

#[test]
fn harness_export_writes_a_complete_bundle() {
    let home = TempDir::new();
    let bundle = home.0.join("bundle");
    let output = run_wizard(
        &home.0,
        &["harness", "export", bundle.to_str().unwrap()],
        &[],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "harness export must exit 0.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let prompt = std::fs::read_to_string(bundle.join("system_prompt.md"))
        .expect("bundle has system_prompt.md");
    assert!(!prompt.trim().is_empty(), "exported prompt is non-empty");

    // One description file per compiled-in tool, contents matching the compiled
    // defaults' non-empty guarantee. `web_search` is a plugin tool
    // (`--features tool-web`), so it is only expected when this build has it:
    // the bundle describes what the binary can do, not what some other build
    // could.
    let mut tools = vec!["read_file", "write_file", "execute"];
    if cfg!(feature = "tool-web") {
        tools.push("web_search");
    }
    for tool in tools {
        let path = bundle.join("tool_descriptions").join(format!("{tool}.md"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("bundle has tool_descriptions/{tool}.md"));
        assert!(!text.trim().is_empty(), "{tool} description is non-empty");
    }

    // Bundled skills and loadout subagents ride along (dev-build discovery
    // via the repo checkout), plus the built-in worker definition.
    assert!(
        bundle.join("skills/coding/SKILL.md").is_file(),
        "bundled coding skill exported"
    );
    assert!(
        bundle.join("subagents/reviewer.toml").is_file(),
        "loadout reviewer subagent exported"
    );
    assert!(
        bundle.join("subagents/worker.toml").is_file(),
        "built-in worker subagent exported"
    );
    assert!(bundle.join("HARNESS.md").is_file(), "bundle guide exported");
    assert!(stdout.contains("exported harness bundle"), "{stdout}");
}

/// `wizard gateway <verb>` reaches the gateway plugin, or says which build it is
/// not in.
///
/// The gateway is the first plugin to own two entrypoints, and they are two
/// *names* rather than one name at two argument types, because the service
/// registry keys on the name alone — see `entrypoint::GATEWAY_SERVICE`. A unit
/// test asserts both registrations and both argument types; only a process can
/// prove that `crate::run`'s dispatch arm looks one of them up successfully,
/// since a lookup at the wrong name or the wrong type reads exactly like a
/// plugin that was never compiled in.
///
/// `gateway status` rather than `gateway setup`, which is interactive, and
/// rather than `--gateway`, which dispatches after config load and then does
/// not return until Ctrl-C. The present-build assertion is deliberately weak:
/// whatever a host's service supervisor says — installed, not installed, or "no
/// systemd here" — it must not be the absent sentence. What is proven is that
/// the lookup found a body, not what that body thinks of this machine.
#[test]
fn the_gateway_admin_surface_reaches_the_plugin_or_says_it_is_absent() {
    let home = TempDir::new();

    let run = run_wizard(&home.0, &["gateway", "status"], &[]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );

    if !cfg!(feature = "gateway") {
        assert!(!run.status.success(), "{said}");
        assert!(said.contains("--features gateway"), "{said}");
        return;
    }

    assert!(
        !said.contains("not in this build"),
        "`wizard gateway status` did not reach the plugin: {said}"
    );
}

/// `wizard mcp-serve` speaks the protocol, or says which build it is not in.
///
/// The one thing a unit test cannot check about this surface: whether a whole
/// process, started from the `clap` variant in core and dispatched through a
/// lookup, actually answers JSON-RPC on its stdout. `serve::handle` is unit
/// tested against a registry built in-process; only a subprocess proves the
/// entrypoint in between finds a body and that the body reaches stdin.
///
/// `initialize` rather than `tools/list`, because the roster depends on which
/// tool plugins this leg compiled in and the handshake does not. Stdin is
/// closed after the one request, which is how the loop is meant to end.
#[test]
fn mcp_serve_answers_a_handshake_or_says_it_is_absent() {
    use std::io::Write;
    use std::process::Stdio;

    let home = TempDir::new();

    if !cfg!(feature = "mcp") {
        let run = run_wizard(&home.0, &["mcp-serve"], &[]);
        let stderr = String::from_utf8_lossy(&run.stderr).to_string();
        assert!(!run.status.success(), "{stderr}");
        assert!(stderr.contains("--features mcp"), "{stderr}");
        return;
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_wizard"))
        .arg("mcp-serve")
        .env("HOME", &home.0)
        .current_dir(&home.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp-serve");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .expect("write the request");
    // The loop ends at EOF, so the pipe has to close or `wait_with_output`
    // waits for a server doing exactly what it was told to do.
    drop(stdin);
    let out = child.wait_with_output().expect("mcp-serve exits at EOF");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains(r#""serverInfo""#), "{stdout}");
    assert!(stdout.contains(r#""name":"wizard""#), "{stdout}");
}

/// `wizard peers` reaches the mesh plugin, or says which build it is not in.
///
/// The one thing a unit test cannot check about this subcommand: whether the
/// two clap parsers — core's, which takes an unparsed vector, and the plugin's,
/// which takes eight subcommands and a three-state trust enum — actually meet
/// in a running binary. `src/cli.rs` proves the arguments cross unchanged and
/// `plugins::mesh::cli` proves what they mean; only a process can prove the
/// lookup in between finds anything.
///
/// Both sides are asserted against the same feature flag, so the leg in
/// `contrib/check-tool-plugins.sh` that builds without the mesh checks the
/// degrade path rather than skipping it.
#[test]
fn peers_reaches_the_mesh_plugin_or_says_it_is_absent() {
    let home = TempDir::new();

    let listed = run_wizard(&home.0, &["peers", "list"], &[]);
    let stdout = String::from_utf8_lossy(&listed.stdout).to_string();
    let stderr = String::from_utf8_lossy(&listed.stderr).to_string();

    if !cfg!(feature = "mesh") {
        assert!(!listed.status.success(), "{stdout}{stderr}");
        assert!(stderr.contains("this build has no mesh"), "{stderr}");
        return;
    }

    assert!(listed.status.success(), "{stdout}{stderr}");
    assert!(stdout.contains("no peers on this machine"), "{stdout}");

    // The plugin's parser, not core's: the usage line and the eight
    // subcommands are what somebody sees after mistyping one. Core's
    // passthrough variant would have printed `wizard peers [ARGS]...` and
    // listed nothing, which is why `disable_help_flag` is on it.
    let help = run_wizard(&home.0, &["peers", "--help"], &[]);
    let help_text = String::from_utf8_lossy(&help.stdout).to_string();
    assert!(help.status.success(), "{help_text}");
    for subcommand in [
        "list", "address", "add", "trust", "forget", "ping", "refresh", "watch",
    ] {
        assert!(help_text.contains(subcommand), "{subcommand}: {help_text}");
    }

    // A trust state the store cannot record is refused by the store's own
    // enum, reached through core's passthrough. This is the assertion the
    // whole `Subcommand` seam exists for: core cannot name `Trust`, so this
    // list can only have come from the plugin.
    let bad = run_wizard(&home.0, &["peers", "trust", "wiz1abc", "allowed"], &[]);
    let refusal = String::from_utf8_lossy(&bad.stderr).to_string();
    assert!(!bad.status.success(), "{refusal}");
    assert!(refusal.contains("blocked, known, trusted"), "{refusal}");
}

/// `wizard help peers` prints the same document `wizard peers --help` does.
///
/// clap's help *subcommand* is not reached by `disable_help_flag`, so it used
/// to render core's passthrough variant — "Usage: wizard peers [ARGS]..." and
/// nothing else — while the flag spelling one word away printed the plugin's
/// eight subcommands. Two spellings of one request; two different answers, one
/// of them useless.
///
/// Asserted as byte equality rather than by looking for the subcommands in
/// both, because "they both mention `refresh`" is exactly what was true before
/// and after the wrong one. On a build with no mesh there is nothing to
/// forward to and clap keeps the request, which is the other half of the
/// claim.
#[test]
fn the_help_subcommand_and_the_help_flag_print_the_same_thing_for_a_plugin_tree() {
    let home = TempDir::new();

    let via_subcommand = run_wizard(&home.0, &["help", "peers"], &[]);
    let subcommand_text = String::from_utf8_lossy(&via_subcommand.stdout).to_string();
    assert!(via_subcommand.status.success(), "{subcommand_text}");

    if !cfg!(feature = "mesh") {
        // Nothing registered `peers`, so nothing was forwarded and clap
        // answered from core's own variant. What matters is that it is still
        // an answer and not a crash.
        assert!(subcommand_text.contains("Usage:"), "{subcommand_text}");
        return;
    }

    let via_flag = run_wizard(&home.0, &["peers", "--help"], &[]);
    let flag_text = String::from_utf8_lossy(&via_flag.stdout).to_string();
    assert!(via_flag.status.success(), "{flag_text}");
    assert_eq!(subcommand_text, flag_text);
    assert!(
        subcommand_text.contains("wizard peers <COMMAND>"),
        "the plugin's usage line, not core's `[ARGS]...`:\n{subcommand_text}"
    );
}

/// `--help` lists a plugin-owned subcommand exactly when this build has one,
/// and describes it in the plugin's words.
///
/// The bug: the descriptions were doc comments on core's `clap` variants, so
/// a `--no-default-features` binary advertised an ACP server and a fleet it
/// could not start, in the present tense, and there was no build on which that
/// text was wrong enough to notice.
///
/// `gui` is the deliberate exception and is checked in both directions for
/// that reason: it stays listed with core's own text when absent, because the
/// window is a `curl` away rather than a rebuild, and switches to the
/// plugin's when present — where core's sentence would be telling the reader
/// to go and get something already in front of them.
#[test]
fn help_lists_a_plugin_subcommand_exactly_when_this_build_has_it() {
    let home = TempDir::new();
    let output = run_wizard(&home.0, &["--help"], &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    // One line per subcommand, so the row is "  <name>  <description>" and a
    // dropped row leaves no line starting with the name.
    let listed = |name: &str| {
        stdout
            .lines()
            .any(|line| line.trim_start().starts_with(&format!("{name} ")))
    };

    for (compiled_in, name) in [
        (cfg!(feature = "acp"), "acp"),
        (cfg!(feature = "fleet"), "fleet"),
        (cfg!(feature = "mcp"), "mcp-serve"),
        (cfg!(feature = "mesh"), "peers"),
    ] {
        assert_eq!(listed(name), compiled_in, "`{name}` in --help:\n{stdout}");
    }

    // Always listed, and the two texts are distinguishable: core's names the
    // feature flag, the plugin's does not.
    assert!(listed("gui"), "{stdout}");
    let gui_line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("gui "))
        .expect("the gui row");
    assert_eq!(
        gui_line.contains("Needs a build with `--features native`"),
        !cfg!(feature = "native"),
        "{gui_line}"
    );

    // A subcommand `--help` no longer lists still parses and still explains
    // itself, which is the whole reason dropping the row is acceptable.
    if !cfg!(feature = "acp") {
        let absent = run_wizard(&home.0, &["acp"], &[]);
        let stderr = String::from_utf8_lossy(&absent.stderr).to_string();
        assert!(!absent.status.success(), "{stderr}");
        assert!(stderr.contains("--features acp"), "{stderr}");
    }
}
