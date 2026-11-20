use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::judge::{self, MAX_TOKENS, Verdict, judge as ask_judge, parse, prompt, refusal};
use crate::hooks::HookBackend;

/// A model that answers with one canned reply and remembers what it was
/// asked, with the token cap it was given.
pub(crate) struct Fake {
    pub reply: Result<String, String>,
    pub seen: Mutex<Vec<(String, u32)>>,
    pub hang: bool,
}

impl Fake {
    pub(crate) fn says(reply: &str) -> Self {
        Fake {
            reply: Ok(reply.to_string()),
            seen: Mutex::new(Vec::new()),
            hang: false,
        }
    }

    pub(crate) fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl HookBackend for Fake {
    async fn ask_model(&self, _: &str, _: Option<&str>) -> Result<String, String> {
        panic!("the judge must use the capped call");
    }
    async fn ask_model_capped(&self, prompt: &str, max_tokens: u32) -> Result<String, String> {
        self.seen
            .lock()
            .unwrap()
            .push((prompt.to_string(), max_tokens));
        if self.hang {
            std::future::pending::<()>().await;
        }
        self.reply.clone()
    }
    async fn run_agent(&self, _: &str, _: Option<&str>) -> Result<String, String> {
        Err("unused".into())
    }
    async fn call_mcp(&self, _: &str, _: &str, _: Value) -> Result<(String, bool), String> {
        Err("unused".into())
    }
}

fn rm() -> Value {
    json!({ "command": "rm -rf build" })
}

#[test]
fn answers_are_read_loosely_but_only_allow_and_deny_count() {
    assert_eq!(parse("ALLOW"), Some(Verdict::Allow));
    assert_eq!(parse("  allow.\n"), Some(Verdict::Allow));
    assert_eq!(parse("**ALLOW**"), Some(Verdict::Allow));
    assert_eq!(
        parse("DENY: deletes files you did not name"),
        Some(Verdict::Deny("deletes files you did not name".into()))
    );
    assert_eq!(
        parse("\n`deny` - sends your keys out"),
        Some(Verdict::Deny("sends your keys out".into()))
    );
    assert_eq!(
        parse("Deny"),
        Some(Verdict::Deny("it goes beyond what was asked for".into()))
    );
    for junk in [
        "",
        "  \n ",
        "Sure! ALLOW",
        "allowed",
        "maybe",
        "ALLOWANCE",
        "I think",
    ] {
        assert_eq!(parse(junk), None, "{junk:?}");
    }
}

#[test]
fn a_long_reason_is_cut() {
    let Some(Verdict::Deny(reason)) = parse(&format!("DENY: {}", "x".repeat(2_000))) else {
        panic!("not a denial");
    };
    assert!(reason.chars().count() < 400, "{}", reason.len());
}

#[tokio::test]
async fn an_allow_comes_back_and_the_call_carries_the_request_and_a_small_cap() {
    let fake = Fake::says("ALLOW");
    let verdict = ask_judge(Some(&fake), "clean the build dir", "execute", &rm()).await;
    assert_eq!(verdict, Some(Verdict::Allow));
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, MAX_TOKENS);
    assert!(seen[0].0.contains("clean the build dir"), "{}", seen[0].0);
    assert!(seen[0].0.contains("rm -rf build"), "{}", seen[0].0);
    assert!(seen[0].0.contains("Bash"), "{}", seen[0].0);
}

#[tokio::test]
async fn a_deny_carries_its_reason() {
    let fake = Fake::says("DENY: the request never mentions deleting");
    let verdict = ask_judge(Some(&fake), "add a test", "execute", &rm()).await;
    assert_eq!(
        verdict,
        Some(Verdict::Deny("the request never mentions deleting".into()))
    );
}

#[tokio::test]
async fn no_answer_is_no_verdict() {
    let garbled = Fake::says("I would rather not say");
    assert_eq!(ask_judge(Some(&garbled), "x", "execute", &rm()).await, None);
    let broken = Fake {
        reply: Err("model call failed: 503".into()),
        ..Fake::says("")
    };
    assert_eq!(ask_judge(Some(&broken), "x", "execute", &rm()).await, None);
    assert_eq!(ask_judge(None, "x", "execute", &rm()).await, None);
    let unused = Fake::says("ALLOW");
    assert_eq!(ask_judge(Some(&unused), "  ", "execute", &rm()).await, None);
    assert_eq!(unused.calls(), 0, "no request, no call");
}

#[tokio::test(start_paused = true)]
async fn a_judge_that_never_answers_times_out_into_no_verdict() {
    let hung = Fake {
        hang: true,
        ..Fake::says("ALLOW")
    };
    assert_eq!(ask_judge(Some(&hung), "x", "execute", &rm()).await, None);
}

#[test]
fn the_call_is_quoted_as_data_and_cut_when_long() {
    let long = json!({ "command": format!("echo {}", "a".repeat(9_000)) });
    let text = prompt("do it", "execute", &long);
    assert!(text.contains("not instructions to you"));
    assert!(text.contains("[cut]"));
    assert!(text.len() < 6_000, "{}", text.len());
}

#[test]
fn the_refusal_names_the_call_and_the_reason() {
    let text = refusal("execute", &rm(), "deletes files.");
    assert!(text.starts_with("Auto mode refused Bash with command: rm -rf build: deletes files."));
    assert!(text.contains("not approved"));
    let _ = judge::TIMEOUT;
}
