//! Streaming HTTP client for Ollama's native `/api/chat` endpoint.
//!
//! Thin `reqwest` wrapper — no `ollama-rs` dependency, keeping the binary
//! small. Provides a startup health probe, a native-tool-support probe, and
//! NDJSON streaming chat.

use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;

use super::{ChatChunk, ChatRequest};

/// Boxed NDJSON chunk stream returned by [`OllamaClient::chat_stream`].
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>;

/// How long to wait for a TCP/TLS connection before declaring Ollama down.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Overall timeout for small control requests (`/api/tags`, `/api/show`).
/// Chat requests are exempt — generation can legitimately take minutes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Errors specific to talking to Ollama, surfaced so the TUI can render
/// actionable messages (e.g. "is Ollama running?").
#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    #[error(
        "cannot reach Ollama at {host} — is the server running? Start it with `ollama serve` (or check `ollama_host` in ~/.wizard/config.toml). Cause: {source}"
    )]
    Unreachable {
        host: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("model '{0}' is not installed (try `ollama pull {0}`)")]
    ModelMissing(String),
    #[error("Ollama returned HTTP {status}: {body}")]
    Api {
        status: reqwest::StatusCode,
        body: String,
    },
}

impl OllamaError {
    /// Whether this error is transient — a retry after backoff may succeed.
    /// Connection/timeout failures and server-busy/rate-limit/5xx statuses
    /// are transient; a missing model or a 4xx (other than 429) is not.
    pub fn is_transient(&self) -> bool {
        match self {
            OllamaError::Unreachable { .. } => true,
            OllamaError::ModelMissing(_) => false,
            OllamaError::Api { status, .. } => status.as_u16() == 429 || status.is_server_error(),
        }
    }
}

/// Client bound to one Ollama host. Cheap to clone.
#[derive(Debug, Clone)]
pub struct OllamaClient {
    http: reqwest::Client,
    host: String,
}

impl OllamaClient {
    /// Create a client for `host` (e.g. `http://127.0.0.1:11434`). Trailing
    /// slashes are trimmed.
    pub fn new(host: impl Into<String>) -> Self {
        let host = host.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            // Builder construction only fails when the TLS backend cannot
            // initialize; fall back to the default client rather than panic.
            .unwrap_or_default();
        Self { http, host }
    }

    /// Base URL this client talks to.
    pub fn host(&self) -> &str {
        &self.host
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.host, path)
    }

    /// Map a transport-level `reqwest` failure into an actionable error.
    /// Connection refusals and timeouts become [`OllamaError::Unreachable`],
    /// which tells the user to run `ollama serve`.
    fn transport_error(&self, source: reqwest::Error) -> anyhow::Error {
        if source.is_connect() || source.is_timeout() {
            OllamaError::Unreachable {
                host: self.host.clone(),
                source,
            }
            .into()
        } else {
            anyhow::Error::new(source).context(format!("HTTP request to {} failed", self.host))
        }
    }

    /// Read the body of a non-success response and convert it into
    /// [`OllamaError::ModelMissing`] (404 mentioning the model) or
    /// [`OllamaError::Api`].
    async fn status_error(
        &self,
        response: reqwest::Response,
        model: Option<&str>,
    ) -> anyhow::Error {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if let Some(model) = model
            && status == reqwest::StatusCode::NOT_FOUND
            && body.contains("not found")
        {
            return OllamaError::ModelMissing(model.to_string()).into();
        }
        OllamaError::Api { status, body }.into()
    }

    /// Startup health probe: `GET /api/tags`. Errors with
    /// [`OllamaError::Unreachable`] when the server is down.
    pub async fn health(&self) -> Result<()> {
        let response = self
            .http
            .get(self.url("/api/tags"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, None).await);
        }
        Ok(())
    }

    /// List locally installed model tags (`GET /api/tags`).
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let response = self
            .http
            .get(self.url("/api/tags"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, None).await);
        }
        let tags: TagsResponse = response
            .json()
            .await
            .context("failed to parse /api/tags response")?;
        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    /// Probe whether `model` supports native tool calling
    /// (`POST /api/show`, inspect `capabilities` for `"tools"`). When this
    /// returns `false` the agent loop falls back to a prompt-based JSON tool
    /// protocol (see `docs/byom.md`).
    pub async fn supports_native_tools(&self, model: &str) -> Result<bool> {
        let response = self
            .http
            .post(self.url("/api/show"))
            .timeout(PROBE_TIMEOUT)
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, Some(model)).await);
        }
        let info: ShowResponse = response
            .json()
            .await
            .context("failed to parse /api/show response")?;
        let supported = info
            .capabilities
            .as_deref()
            .is_some_and(|caps| caps.iter().any(|c| c == "tools"));
        if !supported {
            tracing::debug!(
                model,
                capabilities = ?info.capabilities,
                "model does not advertise native tool support; \
                 the agent loop will use the JSON tool protocol"
            );
        }
        Ok(supported)
    }

    /// Start a streaming chat completion (`POST /api/chat`, NDJSON).
    /// Yields [`ChatChunk`]s until one with `done == true`; the caller
    /// accumulates `message.content` deltas and collects `tool_calls`.
    pub async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let model = request.model.clone();
        let response = self
            .http
            .post(self.url("/api/chat"))
            .json(&request)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, Some(&model)).await);
        }
        let bytes = response
            .bytes_stream()
            .map(|item| match item {
                Ok(chunk) => Ok(chunk.to_vec()),
                Err(e) => Err(anyhow!(e).context("Ollama response stream was interrupted")),
            })
            .boxed();
        Ok(decode_ndjson(bytes))
    }
}

/// `GET /api/tags` response body (subset we care about).
#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<ModelTag>,
}

#[derive(Debug, Deserialize)]
struct ModelTag {
    name: String,
}

/// `POST /api/show` response body (subset we care about). Older Ollama
/// versions omit `capabilities` entirely; we treat that as "no native
/// tools" and use the JSON protocol fallback.
#[derive(Debug, Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Option<Vec<String>>,
}

/// In-band error line Ollama can emit mid-stream: `{"error": "..."}`.
#[derive(Debug, Deserialize)]
struct ErrorLine {
    error: String,
}

/// Parse one NDJSON line into a [`ChatChunk`], surfacing Ollama's in-band
/// `{"error": ...}` lines as errors.
fn parse_chunk_line(line: &str) -> Result<ChatChunk> {
    match serde_json::from_str::<ChatChunk>(line) {
        Ok(chunk) => Ok(chunk),
        Err(parse_err) => {
            if let Ok(err) = serde_json::from_str::<ErrorLine>(line) {
                bail!("Ollama error: {}", err.error);
            }
            let preview: String = line.chars().take(200).collect();
            Err(anyhow!(parse_err).context(format!("unparseable line from Ollama: {preview}")))
        }
    }
}

/// Decoder state for [`decode_ndjson`].
struct NdjsonState<S> {
    bytes: S,
    buf: Vec<u8>,
    finished: bool,
}

/// Turn a raw byte stream into a [`ChatStream`] by splitting on newlines and
/// parsing each line as a [`ChatChunk`]. The stream ends after the chunk
/// with `done == true` (or on transport EOF / error).
fn decode_ndjson<S>(bytes: S) -> ChatStream
where
    S: Stream<Item = Result<Vec<u8>>> + Send + Unpin + 'static,
{
    let state = NdjsonState {
        bytes,
        buf: Vec::new(),
        finished: false,
    };
    stream::try_unfold(state, |mut state| async move {
        if state.finished {
            return Ok(None);
        }
        loop {
            // Drain any complete lines already buffered.
            while let Some(pos) = state.buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = state.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let chunk = parse_chunk_line(line)?;
                if chunk.done {
                    state.finished = true;
                }
                return Ok(Some((chunk, state)));
            }
            match state.bytes.next().await {
                Some(Ok(data)) => state.buf.extend_from_slice(&data),
                Some(Err(e)) => return Err(e),
                None => {
                    // EOF: flush a trailing line without a newline (also the
                    // whole body when the caller requested `stream: false`).
                    state.finished = true;
                    let rest = String::from_utf8_lossy(&state.buf);
                    let rest = rest.trim();
                    if rest.is_empty() {
                        return Ok(None);
                    }
                    let chunk = parse_chunk_line(rest)?;
                    return Ok(Some((chunk, state)));
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Role;

    #[test]
    fn host_trailing_slash_is_trimmed() {
        let client = OllamaClient::new("http://127.0.0.1:11434///");
        assert_eq!(client.host(), "http://127.0.0.1:11434");
        assert_eq!(client.url("/api/tags"), "http://127.0.0.1:11434/api/tags");
    }

    #[test]
    fn parses_content_delta_chunk() {
        let chunk = parse_chunk_line(
            r#"{"model":"m","message":{"role":"assistant","content":"hel"},"done":false}"#,
        )
        .expect("valid chunk");
        assert!(!chunk.done);
        let message = chunk.message.expect("message present");
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.content, "hel");
    }

    #[test]
    fn parses_tool_call_chunk() {
        let chunk = parse_chunk_line(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"src/main.rs"}}}]},"done":false}"#,
        )
        .expect("valid chunk");
        let message = chunk.message.expect("message present");
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].function.name, "read_file");
        assert_eq!(
            message.tool_calls[0].function.arguments["path"],
            "src/main.rs"
        );
    }

    #[test]
    fn surfaces_in_band_error_line() {
        let err = parse_chunk_line(r#"{"error":"model 'x' not found"}"#)
            .expect_err("error line must fail");
        assert!(err.to_string().contains("model 'x' not found"));
    }

    #[test]
    fn transient_classification() {
        let status = |code: u16| reqwest::StatusCode::from_u16(code).expect("valid status");
        assert!(
            OllamaError::Api {
                status: status(503),
                body: String::new(),
            }
            .is_transient()
        );
        assert!(
            OllamaError::Api {
                status: status(429),
                body: String::new(),
            }
            .is_transient()
        );
        assert!(
            !OllamaError::Api {
                status: status(400),
                body: String::new(),
            }
            .is_transient()
        );
        assert!(!OllamaError::ModelMissing("m".to_string()).is_transient());
    }

    #[tokio::test]
    async fn decodes_split_ndjson_lines() {
        // One line split across two network reads, plus a final done line.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(br#"{"message":{"role":"assistant","#.to_vec()),
            Ok(br#""content":"hi"},"done":false}"#.to_vec()),
            Ok(b"\n".to_vec()),
            Ok(
                br#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","eval_count":7}"#
                    .to_vec(),
            ),
        ];
        let mut chunks = decode_ndjson(stream::iter(parts));
        let first = chunks
            .next()
            .await
            .expect("first chunk")
            .expect("first chunk ok");
        assert_eq!(first.message.expect("message").content, "hi");
        assert!(!first.done);
        let last = chunks
            .next()
            .await
            .expect("final chunk")
            .expect("final chunk ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("stop"));
        assert_eq!(last.eval_count, Some(7));
        assert!(chunks.next().await.is_none(), "stream ends after done");
    }

    #[tokio::test]
    async fn stops_after_done_chunk_even_with_trailing_data() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"{\"done\":true}\n{\"message\":{\"role\":\"assistant\",\"content\":\"x\"},\"done\":false}\n".to_vec(),
        )];
        let mut chunks = decode_ndjson(stream::iter(parts));
        let first = chunks.next().await.expect("chunk").expect("ok");
        assert!(first.done);
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn flushes_trailing_line_without_newline_at_eof() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            br#"{"message":{"role":"assistant","content":"all"},"done":true}"#.to_vec(),
        )];
        let mut chunks = decode_ndjson(stream::iter(parts));
        let only = chunks.next().await.expect("chunk").expect("ok");
        assert!(only.done);
        assert_eq!(only.message.expect("message").content, "all");
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn propagates_transport_errors() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"{\"done\":false}\n".to_vec()),
            Err(anyhow!("connection reset")),
        ];
        let mut chunks = decode_ndjson(stream::iter(parts));
        assert!(chunks.next().await.expect("chunk").is_ok());
        let err = chunks.next().await.expect("item").expect_err("error");
        assert!(err.to_string().contains("connection reset"));
    }
}
