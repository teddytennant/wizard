//! The OpenAI provider: `https://api.openai.com`'s own Chat Completions
//! endpoint, and whatever else a user configures as `kind = "openai"`.
//!
//! Almost nothing about talking to OpenAI is peculiar to OpenAI. The request
//! shape, the SSE decoding, the bearer-token seam, the retry classification
//! and the model-family field rules are the *protocol*, shared by every
//! adapter in this family, and they live in [`super::wire`]. This module is
//! what is left when those are taken out, and it is deliberately small: five
//! other providers used to import their protocol from here, which is what
//! stopped any of them from being lifted out on their own.
//!
//! What is left is one request field. OpenAI's Chat Completions API accepts a
//! `prompt_cache_key` that routes a turn to the cache the previous turn
//! warmed; no other endpoint on this wire has it, for the reasons listed on
//! `is_openai_api`. So the endpoint test and the key itself stay here, and
//! [`provider`] is what hands them to the shared client. Every other adapter
//! builds that client directly and gets no key, which is exactly the
//! behaviour each of them already documented and tested for.

use crate::llm::registry::{Credentials, ProviderDescriptor, ProviderKind};
use crate::llm::wire::OpenAiProvider;
use crate::llm::{ChatMessage, Role};

/// OpenAI's own API root, scheme included. `prompt_cache_key` is a field of
/// *this* endpoint's Chat Completions API, not of the wire shape, so it is
/// matched on rather than guessed at; see [`is_openai_api`].
const OPENAI_API_ROOT: &str = "https://api.openai.com";

/// Build the client for a provider configured as `kind = "openai"`.
///
/// `base_url` is whatever the user configured, and it is often not OpenAI:
/// the `openai` kind is also how vLLM, LM Studio, DeepSeek and the
/// `compat.rs` presets (Groq, together.ai, Gemini) are reached. That is why
/// the prompt cache key is attached from the URL rather than from the kind —
/// the kind says "speaks this wire shape", not "is OpenAI".
pub fn provider(
    base_url: impl Into<String>,
    model: impl Into<String>,
    api_key: impl Into<String>,
) -> OpenAiProvider {
    let base_url = base_url.into();
    // Trimmed the same way the client trims it, so a configured trailing
    // slash cannot decide whether the key is sent.
    let keyed = is_openai_api(base_url.trim_end_matches('/'));
    let provider = OpenAiProvider::new(base_url, model, api_key);
    if keyed {
        provider.with_prompt_cache_key(prompt_cache_key)
    } else {
        provider
    }
}

/// Whether `base_url` is OpenAI's own API, the only endpoint in this family
/// known to implement `prompt_cache_key`.
///
/// Everything else that speaks this wire shape degrades to no key at all,
/// which is not a loss of caching, only of routing affinity:
///
/// * llama.cpp, LM Studio and vLLM reuse their own KV cache across requests
///   without being told which conversation they are serving, and there is no
///   prompt-caching API to key into (see `llamacpp.rs`);
/// * Cloudflare Workers AI has no prompt cache at all (see `cloudflare.rs`);
/// * xAI, DeepSeek and the other hosted endpoints configured as an `openai`
///   provider cache automatically and document no key field.
///
/// Sending it to them anyway would put a field on the wire that is at best
/// ignored and at worst rejected by a strict server, so the match is on the
/// endpoint rather than on the wire shape. The comparison is anchored at the
/// scheme and terminated at the path so `https://api.openai.com.example.net`
/// is not mistaken for it.
fn is_openai_api(base_url: &str) -> bool {
    let Some(rest) = base_url.strip_prefix(OPENAI_API_ROOT) else {
        return false;
    };
    // What follows the host has to be a boundary: a path, a port, or nothing.
    rest.is_empty() || rest.starts_with('/') || rest.starts_with(':')
}

/// The `prompt_cache_key` for one request: a short digest of the system
/// prompt and the model tag.
///
/// What the server caches is the *prefix* of the messages array, and the
/// system prompt is the largest part of that prefix which does not change
/// between the turns of one conversation: the charter, the tool list, the
/// project instructions. Digesting it yields a key that is identical on every
/// turn of a session (so a follow-up turn routes to the machine holding the
/// prefix the first turn warmed) and different for another project, another
/// mode, or another model, which is exactly the grouping worth separating.
///
/// Only the **leading** run of system messages counts, and that restriction
/// is the whole difference between a key that works and one that looks like
/// it does. `Role::System` is not reserved for the prompt here: the agent
/// loop pushes a tool-failure nudge as a system message, and so do the task
/// and subagent notes, all of them landing in the middle of the history and
/// staying there. Folding those into the digest re-keys the session the first
/// time a tool fails, routing every later turn away from the cache the
/// earlier ones warmed, for a prefix that never changed. The leading run is
/// literally the prefix the server caches, so it is what the key follows: it
/// moves when a compaction rewrites the head of the history (where a new key
/// is correct, the prefix really did change) and not otherwise.
///
/// It is a routing hint and never an identity. Nothing user-supplied is
/// echoed and nothing about the machine goes in; the digest is truncated to
/// eight bytes, so what lands on the wire is a bucket name and not a
/// recoverable copy of the prompt.
///
/// `None` when the request opens with no system prompt. There is then no
/// stable prefix to route on, and a constant fallback key would herd every
/// such request onto a single cache.
fn prompt_cache_key(model: &str, messages: &[ChatMessage]) -> Option<String> {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut hashed_anything = false;
    for message in messages.iter().take_while(|m| m.role == Role::System) {
        let text = message.text();
        if text.is_empty() {
            continue;
        }
        hasher.update(text.as_bytes());
        // A separator, so two system messages cannot be reshuffled into the
        // same digest as one concatenated message.
        hasher.update([0]);
        hashed_anything = true;
    }
    if !hashed_anything {
        return None;
    }
    // The model is part of the key because a cached prefix belongs to one
    // model: routing a gpt-5 turn to the cache a gpt-4o turn filled buys
    // nothing.
    hasher.update(model.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    Some(format!("wz-{hex}"))
}

/// How `kind = "openai"` is registered.
///
/// [`Credentials::ApiKey`] with no default env var, because this kind is also
/// how vLLM, LM Studio, DeepSeek and every `compat.rs` preset is reached:
/// there is no one variable to guess at, so an unconfigured `api_key_env`
/// falls through to the stored credential rather than to `OPENAI_API_KEY`.
/// That is what the old `match` arm did by passing `None`, and guessing here
/// would start sending an OpenAI key to a local vLLM.
pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::OPENAI,
        "OpenAI-compatible",
        Credentials::ApiKey { default_env: None },
        |config| {
            let key = config.api_key();
            if key.is_empty() {
                config.warn_missing_key("API key", "an env var");
            }
            Ok(std::sync::Arc::new(provider(
                config.base_url.clone(),
                config.model.clone(),
                key,
            )))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ChatRequest;

    #[test]
    fn the_prompt_cache_key_rides_only_on_openais_own_api() {
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ],
            tools: Vec::new(),
            stream: true,
            options: None,
        };

        let hosted = provider("https://api.openai.com/v1", "gpt-4o", "sk-test");
        let key = hosted.build_request_body(&request)["prompt_cache_key"]
            .as_str()
            .expect("OpenAI's own endpoint gets the key")
            .to_string();
        assert!(key.starts_with("wz-"), "{key}");

        // Everything else in the family degrades to no key rather than
        // putting a field on the wire that is at best ignored: a local
        // OpenAI-compatible server, Cloudflare's endpoint, xAI, OpenRouter.
        for base_url in [
            "http://127.0.0.1:1234/v1",
            "https://api.cloudflare.com/client/v4/accounts/acc/ai/v1",
            "https://api.x.ai/v1",
            "https://openrouter.ai/api/v1",
            // A host that merely starts with OpenAI's name is not OpenAI.
            "https://api.openai.com.example.net/v1",
        ] {
            let other = provider(base_url, "gpt-4o", "k");
            assert!(
                other
                    .build_request_body(&request)
                    .get("prompt_cache_key")
                    .is_none(),
                "{base_url} must not receive prompt_cache_key"
            );
        }
    }

    /// The seam itself: a shared client nobody handed a key function to sends
    /// no key, even pointed at OpenAI.
    ///
    /// This is what lets the field live in this module instead of in the
    /// shared one. Every other adapter in the family — OpenRouter, xAI by key
    /// and by OAuth, Cloudflare, llama.cpp — builds the client directly and
    /// reaches this branch, and each of them documents that it sends no key.
    /// Were the client to go back to sniffing the URL, they would inherit a
    /// field the moment one of them was pointed at a proxy of OpenAI's, and
    /// nothing else would notice.
    #[test]
    fn the_shared_client_alone_sends_no_key() {
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ],
            tools: Vec::new(),
            stream: true,
            options: None,
        };
        let bare = OpenAiProvider::new("https://api.openai.com/v1", "gpt-4o", "sk-test");
        assert!(
            bare.build_request_body(&request)
                .get("prompt_cache_key")
                .is_none()
        );
    }

    #[test]
    fn the_prompt_cache_key_is_stable_per_prefix_and_model() {
        let turn = |system: &str, model: &str, user: &str| {
            prompt_cache_key(
                model,
                &[ChatMessage::system(system), ChatMessage::user(user)],
            )
        };

        // The point of the key: two turns of one conversation route to the
        // same cache even though the user text differs.
        assert_eq!(
            turn("You are Wizard.", "gpt-4o", "first"),
            turn("You are Wizard.", "gpt-4o", "second"),
        );
        // A different prefix, mode or project is a different cache.
        assert_ne!(
            turn("You are Wizard.", "gpt-4o", "x"),
            turn("You are Wizard, in a different project.", "gpt-4o", "x"),
        );
        // A cached prefix belongs to one model.
        assert_ne!(
            turn("You are Wizard.", "gpt-4o", "x"),
            turn("You are Wizard.", "gpt-5", "x"),
        );
        // Two system messages cannot reshuffle into the same digest as one.
        assert_ne!(
            prompt_cache_key(
                "gpt-4o",
                &[ChatMessage::system("ab"), ChatMessage::system("c")]
            ),
            prompt_cache_key("gpt-4o", &[ChatMessage::system("abc")]),
        );
        // No system prompt: no stable prefix, so no key rather than one
        // constant key funnelling every such request onto one cache.
        assert_eq!(prompt_cache_key("gpt-4o", &[ChatMessage::user("hi")]), None);
        assert_eq!(
            prompt_cache_key("gpt-4o", &[ChatMessage::system("")]),
            None,
            "an empty system prompt is not a prefix"
        );
        // Nothing recoverable from the prompt reaches the wire.
        let key = turn("SECRET-PROJECT-NAME", "gpt-4o", "x").expect("a key");
        assert!(!key.contains("SECRET"), "{key}");
        assert_eq!(key.len(), "wz-".len() + 16, "truncated digest: {key}");
    }

    /// A mid-conversation system message must not move the key.
    ///
    /// `Role::System` is not reserved for the system prompt: the agent loop
    /// pushes a tool-failure nudge as one (`turn.rs`, after the batch's
    /// results), and the task and subagent notes are system messages too.
    /// They land in the middle of the history and stay there, so digesting
    /// every system message meant one failed tool call re-keyed the rest of
    /// the session and sent every later turn to a cold cache, for a prefix
    /// that had not changed a byte.
    #[test]
    fn a_mid_conversation_system_note_does_not_re_key_the_session() {
        let prompt = ChatMessage::system("You are Wizard.");
        let quiet = vec![
            prompt.clone(),
            ChatMessage::user("read both"),
            ChatMessage::assistant("done"),
        ];
        let nudged = vec![
            prompt.clone(),
            ChatMessage::user("read both"),
            ChatMessage::assistant("done"),
            // Exactly what `turn.rs` pushes after a tool reports a failure.
            ChatMessage::system("The `execute` tool failed; try a narrower command."),
            ChatMessage::user("keep going"),
        ];
        assert_eq!(
            prompt_cache_key("gpt-4o", &quiet),
            prompt_cache_key("gpt-4o", &nudged),
            "a nudge in the middle of the history is not part of the cached prefix"
        );

        // The head of the array still is. A compaction splices its summary in
        // at index 1, and that genuinely rewrites the prefix, so the key is
        // supposed to move with it.
        let compacted = vec![
            prompt,
            ChatMessage::system("Summary of earlier conversation: ..."),
            ChatMessage::user("keep going"),
        ];
        assert_ne!(
            prompt_cache_key("gpt-4o", &quiet),
            prompt_cache_key("gpt-4o", &compacted),
            "a rewritten prefix is a different cache"
        );

        // And a history that opens with a user message has no prefix to route
        // on at all, however many system notes appear later.
        assert_eq!(
            prompt_cache_key(
                "gpt-4o",
                &[ChatMessage::user("hi"), ChatMessage::system("a note")]
            ),
            None
        );
    }
}
