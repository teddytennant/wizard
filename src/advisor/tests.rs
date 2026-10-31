use async_trait::async_trait;
use futures_util::stream;

use super::*;
use crate::llm::{CacheTokens, ChatChunk, ChatStream, Role};

fn provider(name: &str, model: &str) -> ProviderConfig {
    ProviderConfig {
        name: name.into(),
        model: model.into(),
        ..ProviderConfig::default()
    }
}

#[test]
fn a_spec_resolves_to_a_provider_a_model_on_one_or_the_active_provider() {
    let providers = vec![provider("local", "small"), provider("big", "huge-1")];
    let active = providers[0].clone();

    let t = resolve("big", &providers, &active);
    assert_eq!(
        (t.provider.name.as_str(), t.model.as_str()),
        ("big", "huge-1")
    );

    let t = resolve("big/huge-2", &providers, &active);
    assert_eq!(
        (t.provider.name.as_str(), t.model.as_str()),
        ("big", "huge-2")
    );

    let t = resolve("big:huge-3", &providers, &active);
    assert_eq!(t.model, "huge-3");

    let t = resolve("huge-1", &providers, &active);
    assert_eq!(t.provider.name, "big");

    let t = resolve("opus", &providers, &active);
    assert_eq!(
        (t.provider.name.as_str(), t.model.as_str()),
        ("local", "opus")
    );
    assert!(t.how.contains("active provider"), "{}", t.how);
}

#[test]
fn the_brief_carries_the_summary_and_the_question() {
    assert_eq!(brief("why?", ""), "Question:\nwhy?");
    let b = brief("why?", "built X, tests fail");
    assert!(b.starts_with("Where the work stands:\nbuilt X"), "{b}");
    assert!(b.ends_with("Question:\nwhy?"), "{b}");
}

struct Scripted {
    reply: &'static str,
    seen: std::sync::Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl LlmProvider for Scripted {
    async fn health(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn supports_native_tools(&self, _: &str) -> anyhow::Result<bool> {
        Ok(true)
    }
    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
    async fn chat_stream(&self, request: ChatRequest) -> anyhow::Result<ChatStream> {
        self.seen.lock().unwrap().push(request);
        let chunk = ChatChunk {
            message: Some(ChatMessage::assistant(self.reply)),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: Some("stop".into()),
            eval_count: None,
            prompt_eval_count: None,
            cache: CacheTokens::NONE,
            reasoning_eval_count: None,
            billed_ticks: None,
        };
        Ok(Box::pin(stream::iter(vec![Ok(chunk)])))
    }
    fn label(&self) -> String {
        "scripted".into()
    }
}

#[tokio::test]
async fn consult_sends_the_summary_to_the_named_model_with_no_tools() {
    let client = Arc::new(Scripted {
        reply: "  Check the lock order first.  ",
        seen: Default::default(),
    });
    let answer = consult(
        client.clone(),
        "big-model",
        "deadlock?",
        "two threads take a then b",
        None,
    )
    .await
    .unwrap();
    assert_eq!(answer, "Check the lock order first.");
    let seen = client.seen.lock().unwrap();
    let request = &seen[0];
    assert_eq!(request.model, "big-model");
    assert!(request.tools.is_empty());
    assert_eq!(request.messages[0].role, Role::System);
    let user = request.messages[1].text();
    assert!(user.contains("two threads take a then b") && user.contains("deadlock?"));
}

#[tokio::test]
async fn an_empty_answer_is_an_error_not_silence() {
    let client = Arc::new(Scripted {
        reply: "   ",
        seen: Default::default(),
    });
    let err = consult(client, "m", "q", "", None).await.unwrap_err();
    assert!(err.contains("nothing"), "{err}");
}

static SETTING_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_command_sets_reports_and_clears_the_setting_in_claude_codes_words() {
    let _turn = SETTING_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    if std::env::var_os(ENV).is_some() {
        return;
    }
    assert!(command("").unwrap().starts_with("Advisor: off."));
    let set = command("some-model").unwrap();
    assert!(set.starts_with("Advisor set to some-model"), "{set}");
    assert_eq!(configured().as_deref(), Some("some-model"));
    assert!(command("").unwrap().starts_with("Advisor: some-model"));
    assert!(command("off").unwrap().starts_with("Advisor disabled."));
    assert_eq!(configured(), None);
}
