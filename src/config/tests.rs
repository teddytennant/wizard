//! Tests for [`crate::config`].
//!
//! Split out of `config.rs` rather than left at the bottom of it: the module
//! is the widest surface in the tree — every `~/.wizard` path, every env
//! override, the provider table — and its tests had grown to be more than half
//! the file, which made the file's real length a poor signal about the code in
//! it.

use clap::Parser;

use super::*;

fn cli(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("wizard").chain(args.iter().copied())).expect("valid args")
}

#[test]
fn tests_never_write_to_the_real_wizard_dir() {
    // Regression guard, and not a hypothetical one: the suite exercises
    // code that persists config (the TUI's `/vim` toggle, `/mode`,
    // provider setup, onboarding). When this pointed at $HOME, running
    // `cargo test` silently overwrote the developer's own config.toml —
    // providers and all. It did, once.
    let dir = Config::wizard_dir().expect("a wizard dir");
    let home = dirs::home_dir().expect("a home dir");
    assert_ne!(dir, home.join(".wizard"));
    assert!(
        dir.starts_with(std::env::temp_dir()),
        "tests must use a temp wizard dir, got {}",
        dir.display()
    );
}

#[test]
fn defaults_match_docs() {
    let config = Config::default();
    assert_eq!(config.model, "qwen3.6:27b");
    assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
    assert_eq!(config.llamacpp_host, DEFAULT_LLAMACPP_HOST);
    assert!(config.gguf_path.is_none());
    assert_eq!(config.mode, Mode::Genie);
    assert_eq!(config.max_steps, StepBudget::UNLIMITED);
    assert!(!config.continuous);
    assert!(!config.plan_first);
    assert!(!config.plan_each_cycle);
    assert_eq!(config.retry_base_secs, 5);
    assert_eq!(config.retry_max_secs, 300);
    assert_eq!(config.cycle_pause_secs, 0);
    // No gate unless one is asked for: a gate runs commands unattended.
    assert!(config.gates.is_empty());
    assert_eq!(config.gate_max_attempts, 3);
    assert_eq!(config.gate_timeout_secs, 1_800);
    assert_eq!(config.compact_threshold_bytes, 48_000);
    assert!(!config.rollback_failed_cycles);
    assert_eq!(config.max_consecutive_failures, 5);
    assert_eq!(config.checkpoints.keep_turns, 50);
    assert_eq!(config.fleet.max_minutes, 30);
    assert!(config.fleet.synthesize);
}

#[test]
fn checkpoints_section_parses() {
    let config: Config = toml::from_str("[checkpoints]\nkeep_turns = 7").expect("valid toml");
    assert_eq!(config.checkpoints.keep_turns, 7);
    let config: Config = toml::from_str("rollback_failed_cycles = true").expect("valid toml");
    assert!(config.rollback_failed_cycles);
}

/// A config written before the knob existed must keep the documented
/// default rather than deserializing to `0`, which the loop reads as "no
/// bound at all" — the exact opposite of the safe reading.
#[test]
fn max_consecutive_failures_defaults_when_absent_and_zero_is_explicit() {
    let config: Config = toml::from_str("continuous = true").expect("valid toml");
    assert_eq!(config.max_consecutive_failures, 5);
    let config: Config = toml::from_str("max_consecutive_failures = 0").expect("valid toml");
    assert_eq!(config.max_consecutive_failures, 0);
}

#[test]
fn fleet_section_parses_with_partial_keys() {
    let config: Config =
        toml::from_str("[fleet]\nmax_minutes = 10\nsynthesize = false").expect("valid toml");
    assert_eq!(config.fleet.max_minutes, 10);
    assert!(!config.fleet.synthesize);

    let config: Config = toml::from_str("[fleet]\nmax_minutes = 90").expect("valid toml");
    assert_eq!(config.fleet.max_minutes, 90);
    assert!(config.fleet.synthesize, "missing key takes the default");
}

#[test]
fn update_config_defaults() {
    let update = UpdateConfig::default();
    assert!(update.notify);
    assert!(!update.auto);
    assert_eq!(update.repo, "teddytennant/wizard");
    assert_eq!(update.interval_hours, 24);
}

#[test]
fn config_without_update_table_deserializes_to_defaults() {
    // Configs written before `[update]` existed must still parse.
    let config: Config = toml::from_str("model = \"qwen3.6:27b\"").expect("valid toml");
    assert_eq!(config.update, UpdateConfig::default());
}

#[test]
fn update_section_parses_with_partial_keys() {
    let config: Config =
        toml::from_str("[update]\nauto = true\ninterval_hours = 6").expect("valid toml");
    assert!(config.update.auto);
    assert_eq!(config.update.interval_hours, 6);
    // Unspecified keys take their defaults.
    assert!(config.update.notify);
    assert_eq!(config.update.repo, "teddytennant/wizard");

    let config: Config =
        toml::from_str("[update]\nrepo = \"acme/wizard\"\nnotify = false").expect("valid toml");
    assert_eq!(config.update.repo, "acme/wizard");
    assert!(!config.update.notify);
    assert!(!config.update.auto, "missing key takes the default");
}

#[test]
fn mode_parameters() {
    assert_eq!(Mode::Genie.temperature(), 0.8);
    assert_eq!(Mode::Sovereign.temperature(), 0.6);
    assert_eq!(Mode::Genie.to_string(), "genie");
    assert_eq!(Mode::Sovereign.to_string(), "sovereign");
}

#[test]
fn missing_keys_take_defaults() {
    let config: Config = toml::from_str("model = \"qwen3.5:9b\"").expect("valid toml");
    assert_eq!(config.model, "qwen3.5:9b");
    assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
    assert_eq!(config.mode, Mode::Genie);
    assert_eq!(config.max_steps, StepBudget::UNLIMITED);
}

#[test]
fn the_mesh_listener_and_mdns_are_both_off_by_default() {
    // The one thing about `[mesh]` that must not drift. A mesh that opened
    // a socket on install would be a security surface nobody asked for,
    // and an mDNS advertisement broadcasts this machine's public key to
    // every device on the network. Both are opt-in, and this is the test
    // that says so.
    let mesh = MeshConfig::default();
    assert!(!mesh.listen, "the mesh listener is off until somebody asks");
    assert!(!mesh.mdns, "and so is announcing this machine on the LAN");
    assert!(mesh.routes.is_empty());
    assert_eq!(mesh.listen_addr, DEFAULT_MESH_LISTEN_ADDR);
    assert_eq!(Config::default().mesh, mesh);

    // A config file that says nothing about the mesh reads back as off,
    // rather than as whatever a missing field happens to deserialize to.
    let quiet: Config = toml::from_str("model = \"qwen3.6:27b\"").expect("parse");
    assert!(!quiet.mesh.listen);
    assert!(!quiet.mesh.mdns);
    // And a `[mesh]` section that sets something *else* still leaves the
    // listener off: this is the fail-open shape the module keeps warning
    // about, where a field added later defaults to the permissive side.
    let partial: Config = toml::from_str("[mesh]\nmdns = true\n").expect("parse");
    assert!(partial.mesh.mdns);
    assert!(!partial.mesh.listen);
}

#[test]
fn a_malformed_listen_address_is_an_error_rather_than_a_silent_fallback() {
    // Binding the default when somebody typed an address they meant is how
    // a node ends up listening somewhere its operator did not intend.
    let mesh = MeshConfig::default();
    assert_eq!(
        mesh.listen_socket().expect("the default parses").port(),
        DEFAULT_MESH_PORT
    );
    let broken = MeshConfig {
        listen_addr: "0.0.0.0".to_string(),
        ..MeshConfig::default()
    };
    let err = broken.listen_socket().expect_err("no port");
    assert!(format!("{err:#}").contains("host:port"), "{err:#}");
}

/// `WIZARD_CODE_MODE` moves in both directions, and an unrecognised value
/// moves nothing.
///
/// Both halves matter: an exported `WIZARD_CODE_MODE=maybe` must not
/// silently arm a model-authored interpreter, and must not silently disarm
/// one the user turned on in `config.toml` either.
#[test]
fn the_code_mode_env_override_moves_in_both_directions() {
    let mut config = Config::default();
    assert!(!config.code_mode, "off by default");

    config.apply_env_from(|name| (name == "WIZARD_CODE_MODE").then(|| "1".to_string()));
    assert!(config.code_mode);
    config.apply_env_from(|name| (name == "WIZARD_CODE_MODE").then(|| " no ".to_string()));
    assert!(!config.code_mode);
    config.apply_env_from(|name| (name == "WIZARD_CODE_MODE").then(|| "true".to_string()));
    assert!(config.code_mode);
    config.apply_env_from(|name| (name == "WIZARD_CODE_MODE").then(|| "maybe".to_string()));
    assert!(config.code_mode, "an unrecognised value changes nothing");
    config.apply_env_from(|_| None);
    assert!(config.code_mode, "and an unset variable changes nothing");
}

#[test]
fn full_file_round_trips() {
    let original = Config {
        model: "llama3.3:70b".to_string(),
        ollama_host: "http://10.0.0.5:11434".to_string(),
        llamacpp_host: "http://10.0.0.5:8080".to_string(),
        gguf_path: Some("/models/qwen3-8b-q4_k_m.gguf".to_string()),
        mode: Mode::Sovereign,
        reasoning_effort: Some(ReasoningEffort::High),
        max_steps: StepBudget::new(200),
        continuous: true,
        plan_first: true,
        omakase: true,
        plan_each_cycle: true,
        rollback_failed_cycles: true,
        max_consecutive_failures: 9,
        retry_base_secs: 10,
        retry_max_secs: 600,
        cycle_pause_secs: 30,
        gates: vec!["cargo fmt --check".to_string(), "cargo test".to_string()],
        gate_max_attempts: 4,
        gate_timeout_secs: 600,
        compact_threshold_bytes: 96_000,
        providers: vec![ProviderConfig {
            name: "openai".to_string(),
            kind: ProviderKind::OPENAI,
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }],
        active_provider: Some("openai".to_string()),
        gateway: GatewayConfig {
            kind: GatewayKind::Telegram,
            token_env: Some("MY_BOT_TOKEN".to_string()),
            allowed_chat_ids: vec![42, -100123],
        },
        ui: UiConfig {
            spinner_verbs: vec!["Pondering".to_string(), "Musing".to_string()],
            vim: true,
            skin: Some("codex".to_string()),
        },
        web: WebConfig {
            fetch_max_bytes: 250_000,
            allow_local: true,
            search_backend: "brave".to_string(),
            search_api_key_env: Some("BRAVE_API_KEY".to_string()),
            search_model: Some("grok-4.6".to_string()),
        },
        shell: ShellConfig { timeout_secs: 45 },
        checkpoints: CheckpointConfig { keep_turns: 12 },
        fleet: FleetConfig {
            max_minutes: 45,
            synthesize: false,
        },
        update: UpdateConfig {
            notify: false,
            auto: true,
            repo: "acme/wizard".to_string(),
            interval_hours: 6,
        },
        sync: SyncConfig {
            source: Some("https://example.com/wizard-sync.tar.gz".to_string()),
        },
        mesh: MeshConfig {
            listen: true,
            listen_addr: "127.0.0.1:4300".to_string(),
            mdns: true,
            routes: BTreeMap::from([(
                "wiz1AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
                "10.0.0.9:4242".to_string(),
            )]),
        },
        fusion: Some(FusionConfig {
            panel: vec!["openai".to_string()],
            synthesizer: "openai".to_string(),
            rounds: 2,
        }),
        ultra: Some(UltraConfig {
            lenses: vec!["skeptic".to_string(), "minimalist".to_string()],
            judges: 2,
            candidate_max_steps: 8,
            judge_max_steps: 4,
            timeout_secs: 120,
            max_draft_chars: 4_000,
        }),
        code_mode: true,
    };
    let raw = toml::to_string_pretty(&original).expect("serialize");
    let parsed: Config = toml::from_str(&raw).expect("parse back");
    assert_eq!(parsed.model, original.model);
    assert_eq!(parsed.ollama_host, original.ollama_host);
    assert_eq!(parsed.llamacpp_host, original.llamacpp_host);
    assert_eq!(parsed.gguf_path, original.gguf_path);
    assert_eq!(parsed.mode, original.mode);
    assert_eq!(parsed.reasoning_effort, original.reasoning_effort);
    assert_eq!(parsed.max_steps, original.max_steps);
    assert_eq!(parsed.continuous, original.continuous);
    assert_eq!(parsed.plan_first, original.plan_first);
    assert_eq!(parsed.plan_each_cycle, original.plan_each_cycle);
    assert_eq!(parsed.retry_base_secs, original.retry_base_secs);
    assert_eq!(parsed.retry_max_secs, original.retry_max_secs);
    assert_eq!(parsed.cycle_pause_secs, original.cycle_pause_secs);
    assert_eq!(parsed.gates, original.gates);
    assert_eq!(parsed.gate_max_attempts, original.gate_max_attempts);
    assert_eq!(parsed.gate_timeout_secs, original.gate_timeout_secs);
    assert_eq!(
        parsed.compact_threshold_bytes,
        original.compact_threshold_bytes
    );
    assert_eq!(parsed.code_mode, original.code_mode);
    assert_eq!(parsed.providers.len(), 1);
    assert_eq!(parsed.providers[0].name, "openai");
    assert_eq!(parsed.providers[0].kind, ProviderKind::OPENAI);
    assert_eq!(
        parsed.providers[0].api_key_env.as_deref(),
        Some("OPENAI_API_KEY")
    );
    assert_eq!(parsed.active_provider.as_deref(), Some("openai"));
    assert_eq!(parsed.gateway.kind, GatewayKind::Telegram);
    assert_eq!(parsed.gateway.token_env.as_deref(), Some("MY_BOT_TOKEN"));
    assert_eq!(parsed.gateway.allowed_chat_ids, vec![42, -100123]);
    assert_eq!(parsed.ui, original.ui);
    assert_eq!(parsed.web, original.web);
    assert_eq!(
        parsed.rollback_failed_cycles,
        original.rollback_failed_cycles
    );
    assert_eq!(
        parsed.max_consecutive_failures,
        original.max_consecutive_failures
    );
    assert_eq!(parsed.checkpoints, original.checkpoints);
    assert_eq!(parsed.fleet, original.fleet);
    assert_eq!(parsed.update, original.update);
    assert_eq!(parsed.sync, original.sync);
    assert_eq!(parsed.fusion, original.fusion);
    assert_eq!(parsed.ultra, original.ultra);
}

#[test]
fn ultra_defaults_when_section_missing() {
    let config: Config = toml::from_str("model = \"m\"").expect("valid toml");
    assert!(config.ultra.is_none());
    assert_eq!(config.effective_ultra(), UltraConfig::default());

    // A partial block fills the rest from the defaults, so adding a knob to
    // `[ultra]` never invalidates a config that predates it.
    let config: Config = toml::from_str("[ultra]\njudges = 0").expect("valid toml");
    let ultra = config.effective_ultra();
    assert_eq!(ultra.judges, 0);
    assert_eq!(ultra.lenses, UltraConfig::default().lenses);
    assert_eq!(
        ultra.candidate_max_steps,
        UltraConfig::default().candidate_max_steps
    );
    assert_eq!(ultra.timeout_secs, UltraConfig::default().timeout_secs);
}

#[test]
fn sync_defaults_when_section_missing() {
    let config: Config = toml::from_str("model = \"m\"").expect("valid toml");
    assert_eq!(config.sync, SyncConfig::default());
    assert!(config.sync.source.is_none());

    let config: Config =
        toml::from_str("[sync]\nsource = \"~/bundles/w.tar.gz\"").expect("valid toml");
    assert_eq!(config.sync.source.as_deref(), Some("~/bundles/w.tar.gz"));
}

#[test]
fn web_defaults_when_section_missing() {
    let config: Config = toml::from_str("model = \"m\"").expect("valid toml");
    assert_eq!(config.web, WebConfig::default());
    assert_eq!(config.web.fetch_max_bytes, 100_000);
    assert!(!config.web.allow_local);
    assert_eq!(config.web.search_backend, "duckduckgo");
    assert!(config.web.search_api_key_env.is_none());
}

#[test]
fn web_section_parses_partial_keys() {
    let config: Config = toml::from_str(
        "[web]\nsearch_backend = \"tavily\"\nsearch_api_key_env = \"TAVILY_API_KEY\"",
    )
    .expect("valid toml");
    assert_eq!(config.web.search_backend, "tavily");
    assert_eq!(
        config.web.search_api_key_env.as_deref(),
        Some("TAVILY_API_KEY")
    );
    assert_eq!(config.web.fetch_max_bytes, 100_000, "missing keys default");
}

#[test]
fn spinner_verbs_default_when_section_missing() {
    let config: Config = toml::from_str("model = \"qwen3.5:9b\"").expect("valid toml");
    assert!(config.ui.spinner_verbs.is_empty());
    for seed in 0..64 {
        let verb = config.ui.spinner_verb(seed);
        assert!(UiConfig::DEFAULT_SPINNER_VERBS.contains(&verb));
    }
}

#[test]
fn spinner_verbs_default_when_list_empty() {
    let config: Config = toml::from_str("[ui]\nspinner_verbs = []").expect("valid toml");
    assert!(config.ui.spinner_verbs.is_empty());
    assert!(UiConfig::DEFAULT_SPINNER_VERBS.contains(&config.ui.spinner_verb(7)));
}

#[test]
fn spinner_verbs_custom_list_replaces_defaults() {
    let config: Config =
        toml::from_str("[ui]\nspinner_verbs = [\"Pondering\", \"Musing\"]").expect("valid toml");
    assert_eq!(config.ui.spinner_verbs, vec!["Pondering", "Musing"]);
    for seed in 0..64 {
        let verb = config.ui.spinner_verb(seed);
        assert!(verb == "Pondering" || verb == "Musing");
    }
}

#[test]
fn spinner_verb_is_deterministic_per_seed_and_varies_across_seeds() {
    let ui = UiConfig::default();
    assert_eq!(ui.spinner_verb(42), ui.spinner_verb(42));
    // The hash must not collapse every seed onto one verb.
    let first = ui.spinner_verb(0);
    assert!((1..64).any(|seed| ui.spinner_verb(seed) != first));
}

#[test]
fn gateway_defaults_to_none_and_round_trips() {
    // A config without a [gateway] table defaults to None / terminal only.
    let config: Config = toml::from_str("model = \"m\"").expect("valid toml");
    assert_eq!(config.gateway.kind, GatewayKind::None);
    assert!(config.gateway.token_env.is_none());
    assert!(config.gateway.allowed_chat_ids.is_empty());
    assert_eq!(config.gateway.token_env(), GatewayConfig::DEFAULT_TOKEN_ENV);

    // A Telegram gateway round-trips through TOML.
    let raw = toml::to_string_pretty(&Config {
        gateway: GatewayConfig {
            kind: GatewayKind::Telegram,
            token_env: None,
            allowed_chat_ids: vec![7],
        },
        ..Config::default()
    })
    .expect("serialize");
    let parsed: Config = toml::from_str(&raw).expect("parse back");
    assert_eq!(parsed.gateway.kind, GatewayKind::Telegram);
    assert_eq!(parsed.gateway.allowed_chat_ids, vec![7]);
}

/// Adversarial: onboarding now pastes the key instead of naming an env
/// var, so the stored key must be what the provider actually reads back,
/// and must still lose to an exported variable (the documented override).
///
/// Both sources are injected rather than real. Under `cfg(test)`,
/// [`Config::wizard_dir`] is one directory for the whole process, so
/// `credentials.toml` is a single file that several other tests in this
/// binary (`gui::settings`, `app`) write concurrently
/// through `credentials::store`, which is a read-modify-write. A test that
/// stored a key there and read it back could lose its entry to an
/// interleaved writer and fail for reasons that have nothing to do with
/// precedence. The on-disk half of the contract is covered where it
/// belongs and without sharing: the `store_get_remove_round_trip` and
/// `stored_file_is_0600` tests in `crate::credentials` both run against a
/// tempdir of their own.
#[test]
fn the_env_var_wins_over_a_stored_provider_key() {
    let provider = ProviderConfig {
        name: "test-key-precedence".to_string(),
        kind: ProviderKind::OPENAI,
        base_url: "https://example.invalid/v1".to_string(),
        model: "m".to_string(),
        api_key_env: Some("WIZARD_TEST_KEY_PRECEDENCE".to_string()),
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    };
    // Stand-ins for the process environment and for credentials.toml:
    // this test neither depends on nor disturbs either.
    fn env_is(value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |name: &str| (name == "WIZARD_TEST_KEY_PRECEDENCE").then(|| value.to_string())
    }
    fn stored_is(value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |name: &str| (name == "test-key-precedence").then(|| value.to_string())
    }
    let nothing = |_: &str| None;

    // Neither stored nor exported: no key at all. This is the state
    // onboarding used to leave behind, and it 401s on the first turn.
    assert_eq!(provider.resolved_key_from(None, nothing, nothing), "");

    // Paste-and-store, exactly as onboarding does: with no variable
    // exported, the stored key is what goes out.
    assert_eq!(
        provider.resolved_key_from(None, nothing, stored_is("sk-pasted\n")),
        "sk-pasted",
        "a key pasted with a trailing newline still works"
    );

    // The env var overrides the stored key, trailing newline and all
    // (`export KEY=$(cat file)`).
    assert_eq!(
        provider.resolved_key_from(None, env_is("sk-exported\n"), stored_is("sk-pasted")),
        "sk-exported"
    );
    // …but an empty or blank export is not an override.
    assert_eq!(
        provider.resolved_key_from(None, env_is("   "), stored_is("sk-pasted")),
        "sk-pasted",
        "a blank env var must not blank out the stored key"
    );
    // A different provider's stored key is not this provider's key.
    assert_eq!(
        provider.resolved_key_from(None, nothing, |name: &str| (name == "someone-else")
            .then(|| "sk-theirs".to_string())),
        ""
    );
    // A provider with no `api_key_env` still honors the backend default.
    let defaulted = ProviderConfig {
        api_key_env: None,
        ..provider.clone()
    };
    assert_eq!(
        defaulted.resolved_key_from(
            Some("WIZARD_TEST_KEY_PRECEDENCE"),
            env_is("sk-default"),
            nothing
        ),
        "sk-default"
    );
}

/// `~/.wizard` holds session JSONLs (full tool output), logs and
/// credentials. Every directory `ensure_dirs` creates must be private the
/// moment it exists, not only once some credential writer happens to
/// tighten it.
///
/// The mode itself, and the fact that a pre-existing loose directory is
/// tightened rather than left alone, belong to
/// [`crate::platform::secrets`] and are asserted there (exactly 0700, plus
/// the exFAT/CIFS case where the chmod cannot work at all). What config
/// owns, and what this covers, is the *set* of directories: a new one
/// added to `ensure_dirs` and not created privately is the regression.
#[test]
fn state_dirs_are_created_private() {
    Config::ensure_dirs().expect("ensure_dirs");
    for dir in [
        Config::wizard_dir().expect("wizard dir"),
        Config::sessions_dir().expect("sessions dir"),
        Config::logs_dir().expect("logs dir"),
        Config::wizard_dir().expect("wizard dir").join("running"),
    ] {
        assert!(
            crate::platform::secrets::is_protected(&dir).expect("stat"),
            "{} must not be readable by other users",
            dir.display()
        );
    }
}

#[test]
fn legacy_ollama_config_synthesizes_llamacpp() {
    // A file with only model/ollama_host (no providers table) still
    // parses, but the synthesized local provider is llama.cpp — Ollama
    // is opt-in via an explicit [[providers]] entry.
    let config =
        Config::from_toml("model = \"qwen3.5:9b\"\nollama_host = \"http://10.0.0.5:11434\"")
            .expect("valid toml");
    assert!(config.providers.is_empty());
    let active = config.active();
    assert_eq!(active.name, "local");
    assert_eq!(active.kind, ProviderKind::LLAMACPP);
    assert_eq!(active.base_url, DEFAULT_LLAMACPP_HOST);
    assert_eq!(active.model, "qwen3.5:9b");
    assert!(active.api_key_env.is_none());
    assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
}

#[test]
fn fresh_default_synthesizes_llamacpp() {
    // No config file at all: the synthesized provider is llama.cpp.
    let config = Config::default();
    let active = config.active();
    assert_eq!(active.name, "local");
    assert_eq!(active.kind, ProviderKind::LLAMACPP);
    assert_eq!(active.base_url, DEFAULT_LLAMACPP_HOST);
    assert_eq!(active.model, "qwen3.6:27b");
    assert!(active.api_key_env.is_none());
    assert!(active.gguf_path.is_none());

    // An empty file is equivalent to no file.
    let config = Config::from_toml("").expect("valid toml");
    assert_eq!(config.active().kind, ProviderKind::LLAMACPP);
}

#[test]
fn saved_default_config_stays_llamacpp_on_reload() {
    // save() writes every field, including ollama_host — its presence
    // must not change the synthesized llama.cpp default.
    let raw = toml::to_string_pretty(&Config::default()).expect("serialize");
    assert!(raw.contains("ollama_host"), "save writes legacy fields");
    let config = Config::from_toml(&raw).expect("parse back");
    assert_eq!(config.active().kind, ProviderKind::LLAMACPP);
}

#[test]
fn llamacpp_provider_round_trips_through_toml() {
    let original = Config {
        providers: vec![ProviderConfig {
            name: "local".to_string(),
            kind: ProviderKind::LLAMACPP,
            base_url: "http://127.0.0.1:8080".to_string(),
            model: "qwen3-8b".to_string(),
            api_key_env: None,
            gguf_path: Some("/home/u/.wizard/models/qwen3-8b-q4_k_m.gguf".to_string()),
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }],
        active_provider: Some("local".to_string()),
        ..Config::default()
    };
    let raw = toml::to_string_pretty(&original).expect("serialize");
    assert!(raw.contains("kind = \"llamacpp\""), "raw: {raw}");
    let parsed: Config = toml::from_str(&raw).expect("parse back");
    assert_eq!(parsed.providers.len(), 1);
    assert_eq!(parsed.providers[0].kind, ProviderKind::LLAMACPP);
    assert_eq!(
        parsed.providers[0].gguf_path.as_deref(),
        Some("/home/u/.wizard/models/qwen3-8b-q4_k_m.gguf")
    );
    assert!(parsed.providers[0].api_key_env.is_none());
    assert_eq!(parsed.active().kind, ProviderKind::LLAMACPP);
}

#[test]
fn xai_kinds_round_trip_through_toml() {
    let original = Config {
        providers: vec![
            ProviderConfig {
                name: "xai".to_string(),
                kind: ProviderKind::XAI,
                base_url: "https://api.x.ai/v1".to_string(),
                model: "grok-4.3".to_string(),
                api_key_env: Some("XAI_API_KEY".to_string()),
                gguf_path: None,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            },
            ProviderConfig {
                name: "xai-account".to_string(),
                kind: ProviderKind::XAI_OAUTH,
                base_url: "https://api.x.ai/v1".to_string(),
                model: "grok-4.3".to_string(),
                api_key_env: None,
                gguf_path: None,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            },
        ],
        active_provider: Some("xai-account".to_string()),
        ..Config::default()
    };
    let raw = toml::to_string_pretty(&original).expect("serialize");
    // The serde names are what the /provider parser and Display use.
    assert!(raw.contains("kind = \"xai\""), "raw: {raw}");
    assert!(raw.contains("kind = \"xaioauth\""), "raw: {raw}");
    let parsed: Config = toml::from_str(&raw).expect("parse back");
    assert_eq!(parsed.providers[0].kind, ProviderKind::XAI);
    assert_eq!(
        parsed.providers[0].api_key_env.as_deref(),
        Some("XAI_API_KEY")
    );
    assert_eq!(parsed.providers[1].kind, ProviderKind::XAI_OAUTH);
    assert!(parsed.providers[1].api_key_env.is_none());
    assert_eq!(parsed.active().kind, ProviderKind::XAI_OAUTH);
}

#[test]
fn openrouter_kind_round_trips_through_toml() {
    let original = Config {
        providers: vec![ProviderConfig {
            name: "openrouter".to_string(),
            kind: ProviderKind::OPENROUTER,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            model: "openrouter/auto".to_string(),
            api_key_env: Some("OPENROUTER_API_KEY".to_string()),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }],
        active_provider: Some("openrouter".to_string()),
        ..Config::default()
    };
    let raw = toml::to_string_pretty(&original).expect("serialize");
    // The serde name is what the /provider parser and Display use.
    assert!(raw.contains("kind = \"openrouter\""), "raw: {raw}");
    let parsed: Config = toml::from_str(&raw).expect("parse back");
    assert_eq!(parsed.providers[0].kind, ProviderKind::OPENROUTER);
    assert_eq!(
        parsed.providers[0].api_key_env.as_deref(),
        Some("OPENROUTER_API_KEY")
    );
    assert_eq!(parsed.active().kind, ProviderKind::OPENROUTER);
}

/// The `build()` at the end is what needs the plugin; the serde round trip
/// above it does not, and is covered for every kind by
/// `registry::a_kind_serializes_as_the_bare_string_it_is_on_disk`.
#[cfg(feature = "provider-cloudflare")]
#[test]
fn cloudflare_kind_round_trips_through_toml() {
    let original = Config {
        providers: vec![ProviderConfig {
            name: "cloudflare".to_string(),
            kind: ProviderKind::CLOUDFLARE,
            base_url: "https://api.cloudflare.com/client/v4/accounts/acc123/ai/v1".to_string(),
            model: "@cf/zai-org/glm-5.2".to_string(),
            api_key_env: Some("CLOUDFLARE_API_TOKEN".to_string()),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }],
        active_provider: Some("cloudflare".to_string()),
        ..Config::default()
    };
    let raw = toml::to_string_pretty(&original).expect("serialize");
    // The serde name is what the /provider parser and Display use.
    assert!(raw.contains("kind = \"cloudflare\""), "raw: {raw}");
    let parsed: Config = toml::from_str(&raw).expect("parse back");
    assert_eq!(parsed.providers[0].kind, ProviderKind::CLOUDFLARE);
    assert_eq!(parsed.providers[0].model, "@cf/zai-org/glm-5.2");
    assert_eq!(
        parsed.providers[0].api_key_env.as_deref(),
        Some("CLOUDFLARE_API_TOKEN")
    );
    assert_eq!(parsed.active().kind, ProviderKind::CLOUDFLARE);

    // build() dispatches to the Cloudflare client (labeled by vendor+model),
    // proving the wiring from config to provider.
    let client = parsed.active().build().expect("builds a cloudflare client");
    assert_eq!(client.label(), "cloudflare:@cf/zai-org/glm-5.2");
}

#[test]
fn provider_cost_rates_parse_and_round_trip() {
    let raw = "\
[[providers]]
name = \"claude\"
kind = \"anthropic\"
base_url = \"https://api.anthropic.com\"
model = \"claude-fable-5\"
api_key_env = \"ANTHROPIC_API_KEY\"
usd_per_mtok_in = 3.0
usd_per_mtok_out = 15.0
";
    let config: Config = toml::from_str(raw).expect("valid toml");
    let provider = &config.providers[0];
    assert_eq!(provider.usd_per_mtok_in, Some(3.0));
    assert_eq!(provider.usd_per_mtok_out, Some(15.0));

    let serialized = toml::to_string_pretty(&config).expect("serialize");
    let parsed: Config = toml::from_str(&serialized).expect("parse back");
    assert_eq!(parsed.providers[0].usd_per_mtok_in, Some(3.0));
    assert_eq!(parsed.providers[0].usd_per_mtok_out, Some(15.0));

    // Unset rates stay absent on the wire.
    let bare: Config = toml::from_str("model = \"m\"").expect("valid toml");
    assert_eq!(bare.active().usd_per_mtok_in, None);
    let serialized = toml::to_string_pretty(&bare).expect("serialize");
    assert!(!serialized.contains("usd_per_mtok"), "{serialized}");
}

/// The whole back-compat claim, against a literal file rather than
/// against a round trip of something this code built.
///
/// The nine kinds were enum variants with `#[serde(rename_all =
/// "lowercase")]`; they are now registry ids. Nothing but this test can
/// tell you the two spellings still match, because every other test in
/// this file writes the config before it reads it and would pass just as
/// happily if both halves had moved together. `chatgptoauth` is included
/// here and was missing from the old `/provider add` parser's list, which
/// is how that drift was found.
#[test]
fn a_config_written_by_an_older_build_loads_unchanged() {
    let raw = r#"
model = "qwen3.6:27b"
active_provider = "claude"

[[providers]]
name = "local"
kind = "llamacpp"
base_url = "http://127.0.0.1:11435"
model = "Qwen3.6-27B-Q4_K_M"
gguf_path = "/m/Qwen3.6-27B-Q4_K_M.gguf"

[[providers]]
name = "ol"
kind = "ollama"
base_url = "http://127.0.0.1:11434"
model = "qwen3.5:9b"

[[providers]]
name = "oai"
kind = "openai"
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
api_key_env = "OPENAI_API_KEY"

[[providers]]
name = "claude"
kind = "anthropic"
base_url = "https://api.anthropic.com"
model = "claude-fable-5"
usd_per_mtok_in = 3.0
usd_per_mtok_out = 15.0

[[providers]]
name = "or"
kind = "openrouter"
base_url = "https://openrouter.ai/api/v1"
model = "openrouter/auto"

[[providers]]
name = "x"
kind = "xai"
base_url = "https://api.x.ai/v1"
model = "grok-4.6"

[[providers]]
name = "xo"
kind = "xaioauth"
base_url = "https://api.x.ai/v1"
model = "grok-4.6"

[[providers]]
name = "gpt"
kind = "chatgptoauth"
base_url = "https://chatgpt.com/backend-api/codex"
model = "gpt-5.6-sol"

[[providers]]
name = "cf"
kind = "cloudflare"
base_url = "https://api.cloudflare.com/client/v4/accounts/abc/ai/v1"
model = "@cf/zai-org/glm-5.2"
"#;
    let config = Config::from_toml(raw).expect("an existing config must parse");

    let kinds: Vec<String> = config
        .providers
        .iter()
        .map(|provider| provider.kind.to_string())
        .collect();
    assert_eq!(
        kinds,
        [
            "llamacpp",
            "ollama",
            "openai",
            "anthropic",
            "openrouter",
            "xai",
            "xaioauth",
            "chatgptoauth",
            "cloudflare"
        ]
    );

    // Every kind this build installs resolves to a registered backend, and
    // every one of them builds. This is the "all nine keep working"
    // assertion: a constructor that moved to the wrong module, or a
    // descriptor that was never added to the table, fails right here.
    //
    // Counted rather than asserted straight through the loop, because a kind
    // whose provider is a plugin the build left out resolves to nothing *on
    // purpose* — see `plugins::anthropic_is_present_exactly_when_its_feature_is`.
    // The count is what stops the skip from swallowing a real regression: a
    // kind that stops resolving for any other reason takes it below the floor.
    let mut resolved = 0;
    for provider in &config.providers {
        if registry::installed(&provider.kind).is_none() {
            continue;
        }
        assert!(provider.descriptor().is_some(), "{}", provider.kind);
        assert!(provider.build().is_ok(), "{} did not build", provider.kind);
        resolved += 1;
    }
    // The floor is what this build installs, not a literal: the fixture
    // above names all nine kinds, so every kind the registry answers to has
    // to be one of them and has to have resolved. A provider that silently
    // stopped registering takes `kinds()` down with it and the two sides stay
    // equal, which is why the fixture's own coverage is asserted as well.
    for kind in registry::kinds() {
        assert!(
            config.providers.iter().any(|p| p.kind == kind),
            "the fixture must name every kind this build ships: {kind}"
        );
    }
    assert_eq!(
        resolved,
        registry::kinds().len(),
        "every kind this build installs must resolve and build"
    );

    // The rest of the entry survives the trip, and the selection resolves.
    assert_eq!(config.active().name, "claude");
    assert_eq!(config.active().usd_per_mtok_in, Some(3.0));
    assert_eq!(
        config.providers[0].gguf_path.as_deref(),
        Some("/m/Qwen3.6-27B-Q4_K_M.gguf")
    );
    assert_eq!(
        config.providers[2].api_key_env.as_deref(),
        Some("OPENAI_API_KEY")
    );

    // And writing it back out produces the same nine `kind = "..."` lines
    // it came in with, so a config touched by `/settings` is still
    // readable by the build the user came from.
    let written = toml::to_string_pretty(&config).expect("serialize");
    for kind in &kinds {
        assert!(
            written.contains(&format!("kind = \"{kind}\"")),
            "{kind} missing from:\n{written}"
        );
    }
}

/// A kind nothing has registered — a typo, or a provider left out of this
/// profile once providers are plugins — loads rather than making the whole
/// file unparseable, and complains when something tries to use it.
///
/// This is the one deliberate behavior change in the move off the enum,
/// which is why it is pinned rather than left to be discovered.
#[test]
fn an_unknown_kind_loads_and_fails_at_build() {
    let raw = r#"
[[providers]]
name = "future"
kind = "some-provider-plugin"
base_url = "https://example.test/v1"
model = "m"

[[providers]]
name = "oai"
kind = "openai"
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
"#;
    let config = Config::from_toml(raw).expect("an absent plugin must not break the file");
    assert_eq!(config.providers.len(), 2);

    // The healthy provider is still selectable and still builds, which is
    // the point: one missing backend must not cost the user the others. Only
    // meaningful where `openai` is one of the backends this build has — with
    // the plugin left out both entries in the file are absent plugins, which
    // is a different (and also correct) story.
    #[cfg(feature = "provider-openai")]
    assert!(config.providers[1].build().is_ok());

    let message = match config.providers[0].build() {
        Ok(_) => panic!("an unregistered kind must not build"),
        Err(err) => err.to_string(),
    };
    assert!(message.contains("some-provider-plugin"), "{message}");
    // The valid list is generated from what is installed rather than typed
    // out, which is the whole improvement over the old enum's error — and
    // which means on a build with no provider plugins there is nothing in it
    // to check.
    #[cfg(feature = "provider-openai")]
    assert!(message.contains("openai"), "{message}");
}

/// Preparation is best effort by construction — every hosted backend has
/// none — so an unregistered kind stays quiet here and the complaint comes
/// from `build`, which is the call that cannot degrade.
#[tokio::test]
async fn preparing_an_unknown_kind_is_not_an_error() {
    let provider = ProviderConfig {
        name: "future".to_string(),
        kind: ProviderKind::new("some-provider-plugin"),
        base_url: "https://example.test/v1".to_string(),
        model: "m".to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    };
    assert!(provider.prepare("m").await.is_ok());
    assert!(provider.build().is_err());
}

/// Each of the nine descriptors resolves its key from the same place the
/// old `match` arm did. The three that carried a default env var are the
/// ones worth asserting: those literals used to live in two files.
#[test]
fn a_kinds_default_key_env_is_what_its_build_arm_used_to_pass() {
    // `None` for a kind this build does not ship, so each assertion below is
    // skipped rather than rewritten by its plugin's feature: the question is
    // what the descriptor declares, and an absent plugin declares nothing.
    let env_for = |kind: ProviderKind| {
        registry::installed(&kind)
            .map(|descriptor| descriptor.credentials().default_env().map(str::to_string))
    };
    let expect = |kind: ProviderKind, want: Option<&str>| {
        if let Some(got) = env_for(kind.clone()) {
            assert_eq!(got.as_deref(), want, "{kind}");
        }
    };
    expect(
        ProviderKind::OPENROUTER,
        Some(crate::llm::registry::defaults::OPENROUTER_KEY_ENV),
    );
    expect(
        ProviderKind::XAI,
        Some(crate::llm::xai_oauth::DEFAULT_KEY_ENV),
    );
    expect(
        ProviderKind::CLOUDFLARE,
        Some(crate::llm::registry::defaults::CLOUDFLARE_KEY_ENV),
    );
    // The two that deliberately guess nothing: `openai` is also how vLLM
    // and LM Studio are reached, and `anthropic`'s variable is picked up
    // by the BYOP fallback rather than here.
    expect(ProviderKind::OPENAI, None);
    expect(ProviderKind::ANTHROPIC, None);
    // Backends with no key at all.
    expect(ProviderKind::LLAMACPP, None);
    expect(ProviderKind::OLLAMA, None);
    expect(ProviderKind::XAI_OAUTH, None);
    expect(ProviderKind::CHATGPT_OAUTH, None);
}

#[test]
fn provider_kind_display_matches_serde_names() {
    for (kind, name) in [
        (ProviderKind::LLAMACPP, "llamacpp"),
        (ProviderKind::OLLAMA, "ollama"),
        (ProviderKind::OPENAI, "openai"),
        (ProviderKind::ANTHROPIC, "anthropic"),
        (ProviderKind::OPENROUTER, "openrouter"),
        (ProviderKind::XAI, "xai"),
        (ProviderKind::XAI_OAUTH, "xaioauth"),
        (ProviderKind::CLOUDFLARE, "cloudflare"),
    ] {
        assert_eq!(kind.to_string(), name);
        let json = serde_json::to_value(kind).expect("serialize kind");
        assert_eq!(
            json,
            serde_json::json!(name),
            "Display and serde must agree"
        );
    }
}

#[test]
fn active_selects_by_name_and_falls_back_to_first() {
    let providers = vec![
        ProviderConfig {
            name: "local".to_string(),
            kind: ProviderKind::OLLAMA,
            base_url: "http://127.0.0.1:11434".to_string(),
            model: "qwen3.6:27b".to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        },
        ProviderConfig {
            name: "claude".to_string(),
            kind: ProviderKind::ANTHROPIC,
            base_url: "https://api.anthropic.com".to_string(),
            model: "claude-fable-5".to_string(),
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        },
    ];

    // Explicit selection by name.
    let config = Config {
        providers: providers.clone(),
        active_provider: Some("claude".to_string()),
        ..Config::default()
    };
    assert_eq!(config.active().name, "claude");
    assert_eq!(config.active().kind, ProviderKind::ANTHROPIC);

    // Unset active_provider falls back to the first.
    let config = Config {
        providers: providers.clone(),
        active_provider: None,
        ..Config::default()
    };
    assert_eq!(config.active().name, "local");

    // Unknown active_provider also falls back to the first.
    let config = Config {
        providers,
        active_provider: Some("missing".to_string()),
        ..Config::default()
    };
    assert_eq!(config.active().name, "local");
}

#[test]
fn active_provider_mismatch_flags_unknown_names_only() {
    let provider = ProviderConfig {
        name: "local".to_string(),
        kind: ProviderKind::LLAMACPP,
        base_url: DEFAULT_LLAMACPP_HOST.to_string(),
        model: "qwen3.6:27b".to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    };

    // Resolving name / unset name: no mismatch.
    let config = Config {
        providers: vec![provider.clone()],
        active_provider: Some("local".to_string()),
        ..Config::default()
    };
    assert_eq!(config.active_provider_mismatch(), None);
    let config = Config {
        providers: vec![provider.clone()],
        active_provider: None,
        ..Config::default()
    };
    assert_eq!(config.active_provider_mismatch(), None);

    // Unknown name (typo / removed provider): flagged.
    let config = Config {
        providers: vec![provider],
        active_provider: Some("claud".to_string()),
        ..Config::default()
    };
    assert_eq!(config.active_provider_mismatch().as_deref(), Some("claud"));

    // A named provider with no providers configured is also a mismatch —
    // the synthesized local default runs instead.
    let config = Config {
        active_provider: Some("ghost".to_string()),
        ..Config::default()
    };
    assert_eq!(config.active_provider_mismatch().as_deref(), Some("ghost"));
}

#[test]
fn env_model_overrides_active_provider_when_configured() {
    let mut config = Config {
        providers: vec![ProviderConfig {
            name: "openai".to_string(),
            kind: ProviderKind::OPENAI,
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }],
        active_provider: Some("openai".to_string()),
        ..Config::default()
    };
    config.apply_env_from(|name| match name {
        "WIZARD_MODEL" => Some("gpt-4o-mini".to_string()),
        _ => None,
    });
    assert_eq!(config.active().model, "gpt-4o-mini");
    assert_eq!(config.model, "gpt-4o-mini", "legacy field also updated");
}

#[test]
fn unknown_keys_are_ignored() {
    let config: Config = toml::from_str("model = \"m\"\nfuture_option = true").expect("valid toml");
    assert_eq!(config.model, "m");
}

#[test]
fn env_overrides_model_and_host() {
    let mut config = Config::default();
    config.apply_env_from(|name| match name {
        "WIZARD_MODEL" => Some("  llama3.3:70b  ".to_string()),
        "WIZARD_OLLAMA_HOST" => Some("http://10.0.0.5:11434///".to_string()),
        _ => None,
    });
    assert_eq!(config.model, "llama3.3:70b", "model is trimmed");
    assert_eq!(
        config.ollama_host, "http://10.0.0.5:11434",
        "host trailing slashes are trimmed"
    );
}

#[test]
fn env_ollama_host_does_not_change_synthesized_kind() {
    // The env var updates the field (for explicitly configured Ollama
    // providers) but the synthesized local provider stays llama.cpp.
    let mut config = Config::default();
    config.apply_env_from(|name| match name {
        "WIZARD_OLLAMA_HOST" => Some("http://10.0.0.5:11434".to_string()),
        _ => None,
    });
    assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
    assert_eq!(config.active().kind, ProviderKind::LLAMACPP);
}

#[test]
fn env_llamacpp_host_overrides_synthesized_base_url() {
    let mut config = Config::from_toml("model = \"qwen3.5:9b\"").expect("valid toml");
    config.apply_env_from(|name| match name {
        "WIZARD_OLLAMA_HOST" => Some("http://10.0.0.5:11434".to_string()),
        "WIZARD_LLAMACPP_HOST" => Some("http://10.0.0.5:8080///".to_string()),
        _ => None,
    });
    let active = config.active();
    assert_eq!(active.kind, ProviderKind::LLAMACPP);
    assert_eq!(
        active.base_url, "http://10.0.0.5:8080",
        "host trailing slashes are trimmed"
    );
    assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
}

#[test]
fn env_gguf_path_feeds_synthesized_and_active_llamacpp_provider() {
    // Synthesized provider picks up the path.
    let mut config = Config::default();
    config.apply_env_from(|name| match name {
        "WIZARD_GGUF_PATH" => Some("  /models/a.gguf  ".to_string()),
        _ => None,
    });
    assert_eq!(config.gguf_path.as_deref(), Some("/models/a.gguf"));
    assert_eq!(config.active().gguf_path.as_deref(), Some("/models/a.gguf"));

    // An explicitly configured active llamacpp provider is updated too;
    // other kinds are left alone.
    let mut config = Config {
        providers: vec![ProviderConfig {
            name: "local".to_string(),
            kind: ProviderKind::LLAMACPP,
            base_url: "http://127.0.0.1:8080".to_string(),
            model: "qwen3-8b".to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }],
        active_provider: Some("local".to_string()),
        ..Config::default()
    };
    config.apply_env_from(|name| match name {
        "WIZARD_GGUF_PATH" => Some("/models/b.gguf".to_string()),
        _ => None,
    });
    assert_eq!(config.active().gguf_path.as_deref(), Some("/models/b.gguf"));
}

#[test]
fn env_unset_keeps_existing_values() {
    let mut config = Config::default();
    config.apply_env_from(|_| None);
    assert_eq!(config.model, "qwen3.6:27b");
    assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
    assert_eq!(config.llamacpp_host, DEFAULT_LLAMACPP_HOST);
    assert!(config.gguf_path.is_none());
}

#[test]
fn env_empty_values_are_ignored() {
    let mut config = Config::default();
    config.apply_env_from(|name| match name {
        "WIZARD_MODEL" => Some("   ".to_string()),
        "WIZARD_OLLAMA_HOST" => Some("".to_string()),
        "WIZARD_LLAMACPP_HOST" => Some("  ".to_string()),
        "WIZARD_GGUF_PATH" => Some("".to_string()),
        _ => None,
    });
    assert_eq!(config.model, "qwen3.6:27b");
    assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
    assert_eq!(config.llamacpp_host, DEFAULT_LLAMACPP_HOST);
    assert!(config.gguf_path.is_none());
    assert_eq!(
        config.active().kind,
        ProviderKind::LLAMACPP,
        "empty env values do not opt into Ollama"
    );
}

#[test]
fn cli_mode_overrides_config() {
    let mut config = Config::default();
    config.apply_cli(&cli(&["--mode", "sovereign"]));
    assert_eq!(config.mode, Mode::Sovereign);
    assert_eq!(
        config.max_steps,
        StepBudget::UNLIMITED,
        "sovereign does not cap an unlimited budget"
    );
}

#[test]
fn plan_flag_sets_plan_first() {
    let mut config = Config::default();
    assert!(!config.plan_first);
    assert!(!config.plan_each_cycle);
    config.apply_cli(&cli(&["--plan"]));
    assert!(config.plan_first);
    assert!(!config.plan_each_cycle, "--plan never affects cycles");

    // The flag only sets, never clears, the config value.
    let mut config = Config {
        plan_first: true,
        ..Config::default()
    };
    config.apply_cli(&cli(&[]));
    assert!(config.plan_first);
}

#[test]
fn continuous_flag_forces_sovereign() {
    let mut config = Config::default();
    config.apply_cli(&cli(&["--continuous"]));
    assert_eq!(config.mode, Mode::Sovereign);
    assert!(config.continuous);
    assert_eq!(config.max_steps, StepBudget::UNLIMITED);
}

#[test]
fn sovereign_keeps_explicitly_higher_max_steps() {
    let mut config = Config {
        max_steps: StepBudget::new(250),
        ..Config::default()
    };
    config.apply_cli(&cli(&["--mode", "sovereign"]));
    assert_eq!(config.max_steps, StepBudget::new(250));
}

#[test]
fn sovereign_raises_a_capped_budget_to_its_floor() {
    let mut config = Config {
        max_steps: StepBudget::new(25),
        ..Config::default()
    };
    config.apply_cli(&cli(&["--mode", "sovereign"]));
    assert_eq!(config.max_steps, StepBudget::new(100));
}

#[test]
fn step_budget_zero_is_unlimited() {
    let unlimited = StepBudget::new(0);
    assert_eq!(unlimited, StepBudget::UNLIMITED);
    assert_eq!(unlimited, StepBudget::default());
    assert_eq!(unlimited.cap(), None);
    assert_eq!(unlimited.last_step(), u32::MAX);
    assert_eq!(unlimited.to_string(), "no step limit");
    // Unattended posture never shrinks an unlimited budget.
    assert_eq!(unlimited.for_mode(Mode::Sovereign), StepBudget::UNLIMITED);

    let capped = StepBudget::new(25);
    assert_eq!(capped.cap(), Some(25));
    assert_eq!(capped.last_step(), 25);
    assert_eq!(capped.to_string(), "25 steps");
    assert_eq!(capped.for_mode(Mode::Genie), capped);
}

#[test]
fn step_budget_is_a_bare_integer_in_toml() {
    let config: Config = toml::from_str("max_steps = 7").expect("valid toml");
    assert_eq!(config.max_steps, StepBudget::new(7));
    let raw = toml::to_string_pretty(&config).expect("serialize");
    assert!(raw.contains("max_steps = 7"), "{raw}");

    let config: Config = toml::from_str("max_steps = 0").expect("valid toml");
    assert!(config.max_steps.cap().is_none(), "0 opts out of the limit");
}

#[test]
fn unknown_keys_are_ignored_and_not_written_back() {
    // Old configs carried an `auto_approve` key for the since-removed
    // approval gate. Unknown keys must still load (no `deny_unknown_fields`)
    // and never reappear on re-serialization.
    let config: Config = toml::from_str("auto_approve = false").expect("old key parses");
    let raw = toml::to_string_pretty(&config).expect("serialize");
    assert!(
        !raw.contains("auto_approve"),
        "deprecated key is not written back: {raw}"
    );
}

#[test]
fn a_legacy_gui_step_budget_still_loads() {
    // The GUI used to keep a budget of its own (`[gui] max_steps`). It now
    // runs on the shared one like every other surface, and a config still
    // carrying the old section must load — not fail — and not gain it back.
    let config: Config =
        toml::from_str("max_steps = 12\n[gui]\nmax_steps = 250\n").expect("old section parses");
    assert_eq!(config.max_steps, StepBudget::new(12));
    let raw = toml::to_string_pretty(&config).expect("serialize");
    assert!(
        !raw.contains("[gui]"),
        "the section is not written back: {raw}"
    );
}

#[test]
fn no_flags_leaves_config_untouched() {
    let mut config = Config::default();
    config.apply_cli(&cli(&[]));
    assert_eq!(config.mode, Mode::Genie);
    assert_eq!(config.max_steps, StepBudget::UNLIMITED);
}

#[test]
fn config_sovereign_mode_raises_a_capped_budget_without_flags() {
    let mut config = Config {
        mode: Mode::Sovereign,
        max_steps: StepBudget::new(10),
        ..Config::default()
    };
    config.apply_cli(&cli(&[]));
    assert_eq!(config.max_steps, StepBudget::new(100));
}

/// A whole config file as it is actually written on disk, not a two-field
/// fixture.
///
/// Opening [`ProviderKind`](crate::llm::registry::ProviderKind) from a closed
/// enum into a registry changed how `kind` is parsed, and the failure this
/// guards against is the one that would not show up in a unit test of the
/// parser: a real file mixes a provider with a gateway section, a `[ui]` table
/// of user strings, and a dozen scalars, and it is the *combination* that has
/// to keep deserializing. Taken from a working install, with the Telegram id
/// replaced.
const A_REAL_CONFIG: &str = r#"
model = "qwen3.6:27b"
ollama_host = "http://127.0.0.1:11434"
llamacpp_host = "http://127.0.0.1:11435"
mode = "genie"
max_steps = 0
continuous = false
plan_first = false
compact_threshold_bytes = 48000
active_provider = "xai"
code_mode = false

[[providers]]
name = "xai"
kind = "xaioauth"
base_url = "https://api.x.ai/v1"
model = "grok-4.6"

[gateway]
kind = "telegram"
token_env = "WIZARD_TELEGRAM_TOKEN"
allowed_chat_ids = [1234567890]

[ui]
spinner_verbs = ["Overcoming", "Transvaluing values"]
"#;

#[test]
fn a_real_config_file_still_deserializes_after_the_provider_registry() {
    let config: Config = toml::from_str(A_REAL_CONFIG).expect("a real config must still parse");

    assert_eq!(config.active_provider.as_deref(), Some("xai"));
    assert_eq!(config.model, "qwen3.6:27b");
    assert_eq!(config.compact_threshold_bytes, 48_000);

    let provider = config
        .providers
        .iter()
        .find(|provider| provider.name == "xai")
        .expect("the configured provider survives the round trip");
    // The point of the whole change: `kind` is now an open id rather than an
    // enum variant, and the spelling on disk has to keep resolving.
    assert_eq!(provider.kind.as_str(), "xaioauth");
    assert_eq!(provider.model, "grok-4.6");

    // A kind that parses must still be one the registry can actually build,
    // otherwise "it loads" is worthless — on a build that ships it. Absent its
    // plugin the same id parses and resolves to nothing, which is the other
    // half of the same rule and is asserted in
    // `plugins::a_kind_is_installed_exactly_when_its_plugin_is_compiled_in`.
    assert_eq!(
        crate::llm::registry::installed(&provider.kind).is_some(),
        cfg!(feature = "provider-xai"),
        "a shipped provider id must resolve exactly when its plugin is in"
    );
}

#[test]
fn a_real_config_file_round_trips_through_serialization() {
    // Wizard rewrites this file (`/provider add`, `/model`), so a parse that
    // succeeds but serializes to something the next launch reads differently
    // would corrupt a working install one command at a time.
    let config: Config = toml::from_str(A_REAL_CONFIG).expect("parse");
    let written = toml::to_string_pretty(&config).expect("serialize");
    let reparsed: Config = toml::from_str(&written).expect("a written config must parse back");

    assert_eq!(reparsed.active_provider, config.active_provider);
    assert_eq!(reparsed.model, config.model);
    assert_eq!(reparsed.providers.len(), config.providers.len());
    assert_eq!(reparsed.providers[0].kind, config.providers[0].kind);
}
