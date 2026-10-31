use serde_json::Value;

use super::*;
use crate::trigger::Runner;

fn settings(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("settings.json")
}

fn env_of_file(file: &Path) -> Value {
    let raw = std::fs::read_to_string(file).unwrap();
    serde_json::from_str::<Value>(&raw).unwrap()["env"].clone()
}

#[test]
fn every_name_has_a_row_in_the_command_table() {
    for name in NAMES {
        assert!(super::super::spec(name).is_some(), "/{name} has no row");
        assert!(handles(name));
    }
    assert!(!handles("model"));
}

#[test]
fn option_words_take_equals_and_flag_forms() {
    let (bare, named) = options("us-east-1 profile=dev --model m1 --region=eu-west-1");
    assert_eq!(bare, vec!["us-east-1"]);
    assert_eq!(named["profile"], "dev");
    assert_eq!(named["model"], "m1");
    assert_eq!(named["region"], "eu-west-1");
}

#[test]
fn setup_bedrock_writes_the_env_block_keeps_the_rest_and_never_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let file = settings(&dir);
    std::fs::write(&file, r#"{"model":"keep-me","env":{"OTHER":"1"}}"#).unwrap();
    let reply = setup_bedrock(&file, "us-east-1 profile=dev model=us.anthropic.sonnet").unwrap();
    assert!(reply.contains("Bedrock route saved"), "{reply}");
    assert!(reply.contains("wizard --provider claude"), "{reply}");
    let env = env_of_file(&file);
    assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"], "1");
    assert_eq!(env["AWS_REGION"], "us-east-1");
    assert_eq!(env["AWS_PROFILE"], "dev");
    assert_eq!(env["ANTHROPIC_MODEL"], "us.anthropic.sonnet");
    assert_eq!(env["OTHER"], "1");
    let whole: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(whole["model"], "keep-me");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        !text.contains("SECRET") && !text.contains("ACCESS_KEY"),
        "{text}"
    );

    let off = setup_bedrock(&file, "off").unwrap();
    assert!(off.contains("removed"), "{off}");
    let env = env_of_file(&file);
    assert!(env.get("CLAUDE_CODE_USE_BEDROCK").is_none());
    assert_eq!(env["OTHER"], "1");
}

#[test]
fn setup_vertex_needs_a_project_and_defaults_the_region() {
    let dir = tempfile::tempdir().unwrap();
    let file = settings(&dir);
    let guide = setup_vertex(&file, "").unwrap();
    assert!(
        guide.contains("/setup-vertex <project-id>")
            && guide.contains("gcloud auth application-default login"),
        "{guide}"
    );
    assert!(!file.exists(), "guidance writes nothing");

    setup_vertex(&file, "my-proj").unwrap();
    let env = env_of_file(&file);
    assert_eq!(env["CLAUDE_CODE_USE_VERTEX"], "1");
    assert_eq!(env["ANTHROPIC_VERTEX_PROJECT_ID"], "my-proj");
    assert_eq!(env["CLOUD_ML_REGION"], "global");
    setup_vertex(&file, "my-proj region=us-east5 model=m").unwrap();
    assert_eq!(env_of_file(&file)["CLOUD_ML_REGION"], "us-east5");
    setup_vertex(&file, "off").unwrap();
    assert!(
        env_of_file(&file)
            .get("ANTHROPIC_VERTEX_PROJECT_ID")
            .is_none()
    );
}

#[test]
fn a_setup_command_refuses_a_settings_file_that_is_not_json() {
    let dir = tempfile::tempdir().unwrap();
    let file = settings(&dir);
    std::fs::write(&file, "{ nope").unwrap();
    assert!(setup_bedrock(&file, "us-east-1").is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ nope");
}

fn store(dir: &tempfile::TempDir) -> Triggers {
    Triggers::at(
        dir.path().join("t"),
        dir.path().join("schedule.toml"),
        Runner::with_program("/bin/true"),
    )
}

#[test]
fn remote_control_prints_how_to_attach_and_on_off_manage_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let before = remote_control(&store, "").unwrap();
    for needle in ["wizard acp", "ssh ", "gateway", "No HTTP trigger yet"] {
        assert!(before.contains(needle), "{needle}: {before}");
    }
    let on = remote_control(&store, "on").unwrap();
    let token = store.list().unwrap()[0].webhook_token.clone().unwrap();
    assert!(
        on.contains(&token),
        "the user's own terminal shows the token: {on}"
    );
    assert!(
        on.contains("wizard trigger serve") && on.contains("/v1/triggers/"),
        "{on}"
    );
    remote_control(&store, "on").unwrap();
    assert_eq!(
        store.list().unwrap().len(),
        1,
        "asking again makes no second trigger"
    );
    let off = remote_control(&store, "off").unwrap();
    assert!(off.contains("no longer works"), "{off}");
    assert!(store.list().unwrap().is_empty());
    assert!(remote_control(&store, "sideways").is_err());
}

#[test]
fn remote_env_lists_where_triggers_run() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    assert!(remote_env(&store).unwrap().contains("no triggers yet"));
    store
        .create(
            &serde_json::json!({"name": "nightly", "prompt": "p", "cron_expression": "0 3 * * *"}),
            dir.path(),
        )
        .unwrap();
    let text = remote_env(&store).unwrap();
    assert!(
        text.contains("nightly") && text.contains(&dir.path().display().to_string()),
        "{text}"
    );
    assert!(text.contains("scheduler:"), "{text}");
}

#[test]
fn desktop_and_mobile_say_what_they_are() {
    let text = mobile();
    assert!(
        text.contains("no Anthropic mobile app link") && text.contains("wizard acp"),
        "{text}"
    );
    assert!(text.contains("wizard-android.apk"), "{text}");
}
