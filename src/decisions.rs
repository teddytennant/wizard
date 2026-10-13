//! Decision models: small, fast classifier endpoints that answer yes/no
//! questions about a piece of text with a probability, in a few hundred
//! milliseconds. `[decisions]` in `config.toml` picks one; without the table,
//! or without a key for its provider, [`from_config`] is `None` and nothing
//! here runs.
//!
//! Two wire shapes cover the three providers. OpenAI takes
//! `POST {base}/decisions`. SubQ and Jev share `POST {base}/v1/systemone`,
//! where a yes/no question is a `noul`. The first caller is
//! [`crate::agent::routing`]; anything else that wants a typed answer about a
//! prompt asks through [`DecisionProvider`] the same way. See
//! docs/decisions.md.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::config::DecisionsConfig;

/// A yes/no question. `name` keys its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub name: String,
    pub instructions: String,
}

impl Question {
    pub fn new(name: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            instructions: instructions.into(),
        }
    }
}

/// Probability of yes, by question name. A question the provider refused or
/// left out is absent.
pub type Scores = BTreeMap<String, f64>;

#[async_trait]
pub trait DecisionProvider: Send + Sync + std::fmt::Debug {
    fn id(&self) -> &str;
    async fn decide(&self, input: &str, questions: &[Question]) -> Result<Scores>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    OpenAi,
    SystemOne,
}

/// What a provider name stands for: wire shape, base URL, model, key variable.
fn defaults(provider: &str) -> Option<(Wire, &'static str, &'static str, &'static str)> {
    match provider {
        "openai" => Some((
            Wire::OpenAi,
            "https://api.openai.com/v1",
            "gpt-6-luna",
            "OPENAI_API_KEY",
        )),
        "subq" => Some((
            Wire::SystemOne,
            "https://api.subq.ai",
            "subq-snap-preview",
            "SUBQ_API_KEY",
        )),
        // Pinned, because thresholds are tuned per model and `jev-latest` moves.
        "jev" => Some((
            Wire::SystemOne,
            "https://api.typesafe.ai",
            "jev-1.13.0",
            "JEV_API_KEY",
        )),
        _ => None,
    }
}

/// The key for `config`'s provider: its environment variable, then the key
/// stored under the provider's name in `credentials.toml`.
fn resolve_key(config: &DecisionsConfig, default_env: &str) -> Option<String> {
    let env = config.api_key_env.as_deref().unwrap_or(default_env);
    std::env::var(env)
        .ok()
        .or_else(|| crate::credentials::get(&config.provider))
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

/// The configured decision model, or `None` when there is no `[decisions]`
/// table, its provider is unknown, or no key for it can be found.
pub fn from_config(config: Option<&DecisionsConfig>) -> Option<Arc<dyn DecisionProvider>> {
    let config = config?;
    let (_, _, _, default_env) = defaults(&config.provider)?;
    let key = resolve_key(config, default_env)?;
    Some(Arc::new(HttpDecisions::new(config, key)?))
}

#[derive(Debug)]
pub struct HttpDecisions {
    id: String,
    wire: Wire,
    url: String,
    model: String,
    key: String,
    http: reqwest::Client,
}

impl HttpDecisions {
    pub fn new(config: &DecisionsConfig, key: String) -> Option<Self> {
        let (wire, base, model, _) = defaults(&config.provider)?;
        let base = config
            .base_url
            .as_deref()
            .unwrap_or(base)
            .trim_end_matches('/');
        // OpenRouter serves Jev under its own id.
        let model = match config.model.as_deref() {
            Some(model) => model,
            None if config.provider == "jev" && base.contains("openrouter.ai") => "jev-1.13",
            None => model,
        };
        let url = match wire {
            Wire::OpenAi => format!("{base}/decisions"),
            Wire::SystemOne => format!("{base}/v1/systemone"),
        };
        Some(Self {
            id: config.provider.clone(),
            wire,
            url,
            model: model.to_string(),
            key,
            http: reqwest::Client::builder()
                .timeout(config.timeout())
                .build()
                .ok()?,
        })
    }

    fn body(&self, input: &str, questions: &[Question]) -> Value {
        match self.wire {
            Wire::OpenAi => json!({
                "model": self.model,
                "input": input,
                "questions": questions
                    .iter()
                    .map(|q| json!({ "type": "predicate", "name": q.name, "instructions": q.instructions }))
                    .collect::<Vec<_>>(),
            }),
            Wire::SystemOne => json!({
                "model": self.model,
                "state": input,
                "questions": questions
                    .iter()
                    .map(|q| (q.name.clone(), json!({ "type": "noul", "instructions": q.instructions })))
                    .collect::<Map<String, Value>>(),
            }),
        }
    }

    fn scores(&self, reply: &Value, questions: &[Question]) -> Result<Scores> {
        let answers = reply.get("answers");
        let found: Vec<(String, Option<f64>)> = match self.wire {
            // Answers come back in question order; a missing name means "the
            // question at this position".
            Wire::OpenAi => answers
                .and_then(Value::as_array)
                .context("the reply has no answers")?
                .iter()
                .enumerate()
                .filter_map(|(i, a)| {
                    let name = a
                        .get("name")
                        .and_then(Value::as_str)
                        .or(questions.get(i).map(|q| q.name.as_str()))?;
                    Some((
                        name.to_string(),
                        a.get("probability").and_then(Value::as_f64),
                    ))
                })
                .collect(),
            Wire::SystemOne => answers
                .and_then(Value::as_object)
                .context("the reply has no answers")?
                .iter()
                .map(|(name, a)| (name.clone(), a.get("noul").and_then(Value::as_f64)))
                .collect(),
        };
        Ok(found
            .into_iter()
            .filter_map(|(name, p)| Some((name, p.filter(|p| (0.0..=1.0).contains(p))?)))
            .collect())
    }
}

#[async_trait]
impl DecisionProvider for HttpDecisions {
    fn id(&self) -> &str {
        &self.id
    }

    async fn decide(&self, input: &str, questions: &[Question]) -> Result<Scores> {
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&self.body(input, questions))
            .send()
            .await
            .map_err(|err| err.without_url())?;
        let status = response.status();
        let text = response.text().await.map_err(|err| err.without_url())?;
        if !status.is_success() {
            let said: String = text.chars().take(200).collect();
            bail!("{} decisions: status {status}: {said}", self.id);
        }
        let reply: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} decisions: the reply is not JSON", self.id))?;
        self.scores(&reply, questions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answer every connection with `reply` after `delay`, recording each raw
    /// request.
    async fn serve(reply: Value, delay: std::time::Duration) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (requests, body) = (Arc::clone(&requests), reply.to_string());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf[..read]).to_string());
                    tokio::time::sleep(delay).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (base, seen)
    }

    fn config(provider: &str, base_url: &str) -> DecisionsConfig {
        DecisionsConfig {
            provider: provider.to_string(),
            base_url: Some(base_url.to_string()),
            ..DecisionsConfig::default()
        }
    }

    fn questions() -> Vec<Question> {
        vec![
            Question::new("a", "Is it a?"),
            Question::new("b", "Is it b?"),
            Question::new("c", "Is it c?"),
        ]
    }

    fn sent(seen: &Mutex<Vec<String>>) -> (String, Value) {
        let request = seen.lock().unwrap()[0].clone();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        (head.to_lowercase(), serde_json::from_str(body).unwrap())
    }

    #[tokio::test]
    async fn openai_posts_predicates_to_decisions_and_reads_probabilities() {
        let reply = json!({
            "model": "gpt-6-luna",
            "usage": { "input_tokens": 40 },
            "answers": [
                { "type": "predicate", "name": "a", "probability": 0.97 },
                { "type": "predicate", "name": null, "probability": 0.02 },
                { "type": "refusal", "name": "c" },
            ],
        });
        let (base, seen) = serve(reply, Default::default()).await;
        let provider = HttpDecisions::new(&config("openai", &base), "sk-test".into()).unwrap();
        let scores = provider.decide("fix the bug", &questions()).await.unwrap();
        assert_eq!(
            scores,
            Scores::from([("a".to_string(), 0.97), ("b".to_string(), 0.02)])
        );
        let (head, body) = sent(&seen);
        assert!(head.starts_with("post /decisions "), "{head}");
        assert!(head.contains("authorization: bearer sk-test"));
        assert_eq!(body["model"], "gpt-6-luna");
        assert_eq!(body["input"], "fix the bug");
        assert_eq!(
            body["questions"][0],
            json!({ "type": "predicate", "name": "a", "instructions": "Is it a?" })
        );
        assert_eq!(body["questions"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn subq_and_jev_post_nouls_to_systemone() {
        for (name, model) in [("subq", "subq-snap-preview"), ("jev", "jev-1.13.0")] {
            let reply = json!({
                "model": model,
                "answers": {
                    "a": { "type": "noul", "noul": 0.9 },
                    "b": { "type": "noul", "noul": 0.1 },
                    "c": { "type": "refusal" },
                },
            });
            let (base, seen) = serve(reply, Default::default()).await;
            let provider = HttpDecisions::new(&config(name, &base), "key".into()).unwrap();
            assert_eq!(provider.id(), name);
            let scores = provider.decide("fix the bug", &questions()).await.unwrap();
            assert_eq!(
                scores,
                Scores::from([("a".to_string(), 0.9), ("b".to_string(), 0.1)])
            );
            let (head, body) = sent(&seen);
            assert!(head.starts_with("post /v1/systemone "), "{head}");
            assert!(head.contains("authorization: bearer key"));
            assert_eq!(body["model"], model);
            assert_eq!(body["state"], "fix the bug");
            assert_eq!(
                body["questions"]["b"],
                json!({ "type": "noul", "instructions": "Is it b?" })
            );
        }
    }

    #[test]
    fn the_model_and_base_url_can_be_overridden_and_openrouter_gets_its_jev_id() {
        let mut openrouter = config("jev", "https://openrouter.ai/api/");
        let provider = HttpDecisions::new(&openrouter, "k".into()).unwrap();
        assert_eq!(provider.url, "https://openrouter.ai/api/v1/systemone");
        assert_eq!(provider.model, "jev-1.13");
        openrouter.model = Some("jev-latest".to_string());
        let provider = HttpDecisions::new(&openrouter, "k".into()).unwrap();
        assert_eq!(provider.model, "jev-latest");
        let provider = HttpDecisions::new(&DecisionsConfig::default(), "k".into()).unwrap();
        assert_eq!(provider.url, "https://api.openai.com/v1/decisions");
        assert!(HttpDecisions::new(&config("nope", "http://x"), "k".into()).is_none());
    }

    #[tokio::test]
    async fn a_slow_or_failing_endpoint_is_an_error_not_a_hang() {
        let (base, _) = serve(json!({}), std::time::Duration::from_secs(5)).await;
        let mut slow = config("openai", &base);
        slow.timeout_ms = 50;
        let provider = HttpDecisions::new(&slow, "k".into()).unwrap();
        let started = std::time::Instant::now();
        assert!(provider.decide("x", &questions()).await.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        let (base, _) = serve(json!({ "error": "nope" }), Default::default()).await;
        let provider = HttpDecisions::new(&config("openai", &base), "k".into()).unwrap();
        let err = provider.decide("x", &questions()).await.unwrap_err();
        assert!(format!("{err:#}").contains("no answers"), "{err:#}");
    }

    #[test]
    fn no_table_no_known_provider_or_no_key_means_no_provider() {
        assert!(from_config(None).is_none());
        let unknown = config("nope", "http://127.0.0.1:1");
        assert!(from_config(Some(&unknown)).is_none());
        let keyless = DecisionsConfig {
            api_key_env: Some("WIZARD_TEST_DECISIONS_KEY_THAT_IS_NOT_SET".to_string()),
            provider: "wizard-test-no-such-credential".to_string(),
            ..DecisionsConfig::default()
        };
        assert!(resolve_key(&keyless, "ALSO_NOT_SET_WIZARD_TEST").is_none());
    }
}
