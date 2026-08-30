//! Fixture-driven tests for the streaming provider adapters.
//!
//! Every SSE test inside `src/plugins/anthropic.rs` and `src/plugins/openai/` builds
//! its input as an inline Rust string literal and feeds it straight to
//! `decode_sse`. That asserts what the author believed the wire format was, and
//! it keeps asserting it forever: when a provider renames a field, adds a
//! block type, or changes where usage rides, every one of those tests stays
//! green and production breaks. It also skips the transport entirely, so
//! nothing covers the path from `chat_stream` through reqwest to the decoder.
//!
//! These tests fix both halves. The bytes live on disk under
//! `tests/fixtures/<provider>/<case>.sse`, exactly as a server would send
//! them, and [`RecordedProvider`] serves them over a real loopback socket to
//! the real provider client. Nothing here reimplements SSE: a failure means
//! the adapter no longer understands the recorded bytes.
//!
//! # Re-recording a fixture
//!
//! The fixtures are transcriptions of the adapters' own inline literals, which
//! makes them a shared, reviewable corpus but does not yet make them captures.
//! Turning them into captures needs a dump hook inside each adapter's stream
//! path, keyed on `WIZARD_RECORD_FIXTURES`; that hook lives in
//! `src/llm/*.rs`, which this file does not own. **It does not exist yet** —
//! `grep -rn WIZARD_RECORD_FIXTURES src/` finds nothing. Until it lands, a new
//! fixture is written by hand and reviewed like any other test input.
//!
//! One fixture has crossed back the other way, which is the direction worth
//! copying: `openai/parallel_tool_calls.sse` is `include_str!`d by
//! `llm::test_support::PARALLEL_TOOL_BATCH_SSE`, so the adapters' own in-process
//! decoder tests and the over-a-socket ones here read the same bytes and
//! cannot drift.
//!
//! Whatever writes them, the contract this file relies on is small:
//!
//! * one file per stream, byte-for-byte what the socket carried, `\n` line
//!   endings, no BOM;
//! * lines beginning with a colon are SSE comments. Both decoders skip every
//!   line that does not start with `data` and a colon, so the provenance
//!   header at the top of each fixture is inert. Keep writing one.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// Every backend is a plugin now, so a build compiled without one of these
// features has no transport to replay its recordings into. Everything in this
// file that names a provider type is gated; the fixture guards at the bottom
// are not, because they only read bytes off disk and a build that dropped a
// plugin must not drop its recordings.
#[cfg(feature = "provider-chatgpt")]
use wizard::plugins::chatgpt::ChatgptProvider;

use wizard::llm::provider::LlmProvider;
use wizard::llm::wire::OpenAiProvider;
use wizard::llm::{CacheTokens, ChatChunk, ChatMessage, ChatRequest, ToolCall};
#[cfg(feature = "provider-anthropic")]
use wizard::plugins::anthropic::AnthropicProvider;

/// How many bytes of the fixture go into each HTTP chunked frame.
///
/// Deliberately smaller than one SSE event and not a divisor of any line
/// length in the corpus, so every replay hands the decoder frames that start
/// and end mid-JSON. hyper surfaces one `Bytes` per chunked frame, so this is
/// the split point the decoder's reassembly buffer actually sees, the thing
/// an in-process `stream::iter` of whole events never exercises.
const REPLAY_FRAME_BYTES: usize = 48;

/// A recorded provider stream, served over loopback.
///
/// Accepts exactly one connection, captures the request the adapter sent, and
/// replays a fixture as `text/event-stream`. The provider under test is a real
/// [`AnthropicProvider`] / [`OpenAiProvider`] pointed at [`Self::root`], so the
/// bytes travel the same reqwest path they do in production.
struct RecordedProvider {
    /// Server root to point a provider at, e.g. `http://127.0.0.1:41243`.
    /// No trailing slash and no `/v1`: the adapters differ on which of them
    /// owns that suffix.
    root: String,
    /// Body of the request the adapter sent, filled in before the fixture is
    /// replayed. `None` until a client has connected.
    captured: Arc<Mutex<Option<String>>>,
}

impl RecordedProvider {
    /// Bind a loopback port that will replay `fixture` (a path relative to
    /// `tests/fixtures/`) to the first client that connects.
    async fn replay(fixture: &str) -> Self {
        let body = read_fixture(fixture);
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();

        let captured = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&captured);
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            *sink.lock().expect("capture lock") = Some(read_request_body(&mut socket).await);

            // Chunked rather than content-length: the framing is what splits
            // the fixture into the reads the decoder has to reassemble, and a
            // real streaming endpoint has no length to send anyway.
            let head = "HTTP/1.1 200 OK\r\n\
                        content-type: text/event-stream\r\n\
                        cache-control: no-cache\r\n\
                        transfer-encoding: chunked\r\n\
                        \r\n";
            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            for frame in body.as_bytes().chunks(REPLAY_FRAME_BYTES) {
                let size = format!("{:x}\r\n", frame.len());
                if socket.write_all(size.as_bytes()).await.is_err()
                    || socket.write_all(frame).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
                let _ = socket.flush().await;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
            let _ = socket.flush().await;

            // Drain before dropping the socket: closing one that still has
            // unread bytes in its receive buffer sends an RST, which would
            // tear down the response the client has not finished reading.
            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                let mut scratch = [0u8; 1024];
                while let Ok(read) = socket.read(&mut scratch).await {
                    if read == 0 {
                        break;
                    }
                }
            })
            .await;
        });

        Self {
            root: format!("http://127.0.0.1:{port}"),
            captured,
        }
    }

    /// The Anthropic client, pointed here. It appends `/v1/messages` itself.
    #[cfg(feature = "provider-anthropic")]
    fn anthropic(&self, model: &str) -> AnthropicProvider {
        AnthropicProvider::new(&self.root, model, "test-key")
    }

    /// The OpenAI client, pointed here. Its base URL carries the `/v1`.
    fn openai(&self, model: &str) -> OpenAiProvider {
        OpenAiProvider::new(format!("{}/v1", self.root), model, "sk-test")
    }

    /// The ChatGPT-subscription client (the Responses API), pointed here. It
    /// appends `/responses` itself.
    ///
    /// Unlike the other two this one authorizes from a token file rather than
    /// from a constructor argument, so [`stub_chatgpt_tokens`] has to have put
    /// one where it will look.
    #[cfg(feature = "provider-chatgpt")]
    fn responses(&self, model: &str) -> ChatgptProvider {
        stub_chatgpt_tokens();
        ChatgptProvider::new(&self.root, model).expect("build the Responses client")
    }

    /// The request body the adapter put on the wire. Panics if nothing
    /// connected, which can only mean the provider never sent the request.
    fn request_body(&self) -> String {
        self.captured
            .lock()
            .expect("capture lock")
            .clone()
            .expect("the adapter sent a request")
    }
}

/// Read one HTTP request off `socket` and return its body.
///
/// Byte-at-a-time through the head so none of the body is swallowed before
/// `content-length` is known. Request heads here are a few hundred bytes.
async fn read_request_body(socket: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte).await {
            Ok(0) | Err(_) => return String::new(),
            Ok(_) => head.push(byte[0]),
        }
    }
    let length = String::from_utf8_lossy(&head)
        .to_ascii_lowercase()
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if length == 0 {
        return String::new();
    }
    let mut body = vec![0u8; length];
    if socket.read_exact(&mut body).await.is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// Give this test binary a `~/.wizard` of its own holding a stub ChatGPT
/// token, so [`ChatgptProvider`] can authorize a request to a loopback socket.
///
/// The Responses client is the only adapter here whose credentials come off
/// disk rather than out of its constructor, and refusing to run without them
/// is correct of it. `use_wizard_dir` redirects every `~/.wizard` path for
/// the process — first call wins, hence the `OnceLock` — so the developer's
/// real token file is neither read nor written.
///
/// The access token is deliberately not a JWT: `expires_soon` cannot parse an
/// expiry out of it and answers `false`, which is what keeps this off the
/// network. A JWT-shaped one near expiry would send the client to
/// `auth.openai.com` for a refresh in the middle of a unit test.
#[cfg(feature = "provider-chatgpt")]
fn stub_chatgpt_tokens() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("wizard-recorded-provider-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the stub wizard dir");
        std::fs::write(
            dir.join("chatgpt_oauth.json"),
            r#"{"access_token":"stub-not-a-jwt","account_id":"acct_test"}"#,
        )
        .expect("write the stub token file");
        wizard::config::use_wizard_dir(dir);
    });
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read_fixture(relative: &str) -> String {
    let path = fixtures_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read fixture {}: {err}", path.display()))
}

/// Drive one turn through `provider` and collect every chunk it yields.
///
/// The request content is irrelevant to a replay, but it has to be a real
/// [`ChatRequest`] because building it is what the adapter's request path
/// does, and that path is under test too (see the `request_body` assertions).
async fn collect(provider: &dyn LlmProvider, model: &str) -> Vec<ChatChunk> {
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![ChatMessage::user("go")],
        tools: Vec::new(),
        stream: true,
        options: None,
    };
    let mut stream = provider.chat_stream(request).await.expect("stream opens");
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.expect("chunk decodes"));
    }
    chunks
}

/// The two calls a recorded parallel batch decoded to, and the follow-up
/// history that answers them.
struct DecodedBatch {
    calls: Vec<ToolCall>,
    /// `[system, user, assistant-from-the-wire, results]`, where `results` is
    /// the single `Role::Tool` message the agent loop accumulates a whole
    /// batch onto (`agent::turn`). What each adapter does with that one
    /// message is the thing under test: Anthropic must fold it into one
    /// `user` message of `tool_result` blocks, Chat Completions into one
    /// `tool` message per result, Responses into consecutive
    /// `function_call_output` items — and none of them may reach for the tool
    /// name or the arrival order to decide which answer goes with which call.
    follow_up: Vec<ChatMessage>,
}

/// Decode a recorded two-call batch and build the request that answers it.
///
/// The answers are attached in **reverse** call order on purpose. Both calls
/// name the same tool, so name lookup cannot tell them apart; reversing them
/// means arrival order cannot either, and the only thing left that can put
/// `contents of a` against the call that asked for `a` is the id. An adapter
/// that pairs positionally passes every other assertion here and fails these.
fn decode_batch(chunks: &[ChatChunk]) -> DecodedBatch {
    let last = chunks.last().expect("a final chunk");
    assert!(last.done, "the batch arrives on the final chunk");
    let assistant = last.message.clone().expect("the assistant turn");
    let calls = assistant
        .tool_calls()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        calls.len(),
        2,
        "this fixture is a two-call batch: {calls:?}"
    );
    assert_ne!(calls[0].id, calls[1].id, "two calls, two ids");
    assert_eq!(
        calls[0].function.name, calls[1].function.name,
        "both calls must name the same tool, or the fixture is not testing \
         what it exists to test"
    );

    let name = calls[0].function.name.clone();
    let mut results = ChatMessage::tool_result(&calls[1].id, &name, "contents of b");
    results.push_tool_result(&calls[0].id, &name, "contents of a");
    DecodedBatch {
        follow_up: vec![
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("read both"),
            assistant,
            results,
        ],
        calls,
    }
}

/// Send `messages` through `provider` and return the JSON body it put on the
/// wire. The recorded reply is drained and discarded: this half of the round
/// trip is about the request.
async fn body_sent(
    recorded: &RecordedProvider,
    provider: &dyn LlmProvider,
    model: &str,
    messages: Vec<ChatMessage>,
) -> serde_json::Value {
    let request = ChatRequest {
        model: model.to_string(),
        messages,
        tools: vec![wizard::llm::ToolSpec::function(
            "read_file",
            "Read a file.",
            serde_json::json!({ "type": "object" }),
        )],
        stream: true,
        options: None,
    };
    let mut stream = provider.chat_stream(request).await.expect("stream opens");
    while let Some(item) = stream.next().await {
        item.expect("chunk decodes");
    }
    serde_json::from_str(&recorded.request_body()).expect("the request body is JSON")
}

/// Text of every non-final chunk that carries a message, in order.
fn streamed_text(chunks: &[ChatChunk]) -> Vec<(bool, String)> {
    chunks
        .iter()
        .filter(|chunk| !chunk.done)
        .filter_map(|chunk| {
            chunk
                .message
                .as_ref()
                .map(|message| (chunk.thinking, message.text()))
        })
        .collect()
}

#[cfg(feature = "provider-anthropic")]
#[tokio::test]
async fn anthropic_text_then_tool_use_replays_from_the_recorded_stream() {
    let recorded = RecordedProvider::replay("anthropic/text_then_tool_use.sse").await;
    let chunks = collect(&recorded.anthropic("claude-fable-5"), "claude-fable-5").await;

    assert_eq!(
        streamed_text(&chunks),
        vec![(false, "Hi".to_string())],
        "the text block streams live, the tool_use block does not"
    );

    let last = chunks.last().expect("a final chunk");
    assert!(last.done);
    assert_eq!(last.done_reason.as_deref(), Some("tool_use"));
    assert_eq!(last.prompt_eval_count, Some(9), "input_tokens from usage");
    assert_eq!(last.eval_count, Some(6), "output_tokens from usage");

    let calls = last
        .message
        .as_ref()
        .expect("tool call message")
        .tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].function.name, "execute");
    assert_eq!(
        calls[0].function.arguments["command"], "ls",
        "the two input_json_delta fragments reassemble into one argument object"
    );

    // The other half of the contract: what the adapter asked for. `max_tokens`
    // is required on every Messages request and a value over the model's own
    // ceiling is a permanent 400, so its presence is not incidental.
    let sent = recorded.request_body();
    assert!(sent.contains("\"stream\":true"), "{sent}");
    assert!(sent.contains("\"max_tokens\""), "{sent}");
}

#[cfg(feature = "provider-anthropic")]
#[tokio::test]
async fn anthropic_thinking_deltas_stay_flagged_through_the_transport() {
    let recorded = RecordedProvider::replay("anthropic/thinking_then_text.sse").await;
    let chunks = collect(&recorded.anthropic("claude-fable-5"), "claude-fable-5").await;

    assert_eq!(
        streamed_text(&chunks),
        vec![
            (true, "Considering...".to_string()),
            (false, "Answer.".to_string()),
        ],
        "reasoning is flagged, answer text is not"
    );
    assert!(chunks.last().expect("a final chunk").done);
}

#[cfg(feature = "provider-anthropic")]
#[tokio::test]
async fn anthropic_degenerate_tool_input_survives_the_recorded_stream() {
    let recorded = RecordedProvider::replay("anthropic/tool_input_not_json.sse").await;
    let chunks = collect(&recorded.anthropic("claude-fable-5"), "claude-fable-5").await;

    let last = chunks.last().expect("a final chunk");
    assert!(last.done);
    let calls = last
        .message
        .as_ref()
        .expect("tool call message")
        .tool_calls();
    assert_eq!(calls.len(), 2, "neither block is dropped");
    assert_eq!(calls[0].function.name, "execute");
    assert_eq!(
        calls[0].function.arguments,
        serde_json::Value::String("not json".to_string()),
        "input that never became JSON degrades to a string argument"
    );
    assert_eq!(calls[1].function.name, "list");
    assert_eq!(
        calls[1].function.arguments,
        serde_json::json!({}),
        "a tool_use block that received no input gets empty arguments"
    );
}

#[tokio::test]
async fn openai_split_tool_call_replays_from_the_recorded_stream() {
    let recorded = RecordedProvider::replay("openai/split_tool_call.sse").await;
    let chunks = collect(&recorded.openai("gpt-4o"), "gpt-4o").await;

    assert_eq!(streamed_text(&chunks), vec![(false, "Hi".to_string())]);

    let last = chunks.last().expect("a final chunk");
    assert!(last.done);
    assert_eq!(last.done_reason.as_deref(), Some("tool_calls"));
    assert_eq!(last.prompt_eval_count, Some(11));
    assert_eq!(last.eval_count, Some(4));

    let calls = last
        .message
        .as_ref()
        .expect("tool call message")
        .tool_calls();
    assert_eq!(calls.len(), 1, "two fragments, one call");
    assert_eq!(calls[0].function.name, "execute");
    assert_eq!(calls[0].function.arguments["command"], "ls");

    // The usage block the fixture carries only arrives because the request
    // asked for it. Without this the token counts above would be a fiction of
    // the fixture rather than something the adapter can obtain.
    let sent = recorded.request_body();
    assert!(
        sent.contains("\"include_usage\":true"),
        "the adapter must request usage on SSE streams: {sent}"
    );
}

#[tokio::test]
async fn openai_reasoning_content_stays_flagged_through_the_transport() {
    let recorded = RecordedProvider::replay("openai/xai_reasoning_content.sse").await;
    let chunks = collect(&recorded.openai("grok-4.3"), "grok-4.3").await;

    assert_eq!(
        streamed_text(&chunks),
        vec![
            (true, "Weighing the ".to_string()),
            (true, "options.".to_string()),
            (false, "Done.".to_string()),
        ],
        "reasoning_content is a vendor extension, and it must not reach the transcript as answer text"
    );

    let last = chunks.last().expect("a final chunk");
    assert!(last.done);
    assert_eq!(last.done_reason.as_deref(), Some("stop"));
}

#[tokio::test]
async fn openai_generated_image_streams_live_from_the_recorded_stream() {
    let recorded = RecordedProvider::replay("openai/generated_images.sse").await;
    let chunks = collect(&recorded.openai("gpt-4o"), "gpt-4o").await;

    assert_eq!(
        streamed_text(&chunks),
        vec![(false, "here you go".to_string())]
    );

    let images: Vec<_> = chunks
        .iter()
        .filter(|chunk| !chunk.images.is_empty())
        .collect();
    assert_eq!(images.len(), 1, "one chunk carries the image");
    assert!(!images[0].done, "images stream live, like text");
    assert_eq!(images[0].images[0].mime, "image/png");
    assert_eq!(images[0].images[0].b64, "QUJD");

    assert!(chunks.last().expect("a final chunk").done);
}

/// Exit criterion 8, Anthropic leg: a parallel tool-call batch succeeds, with
/// a recorded fixture test.
///
/// Two round trips over two sockets. The first replays a recorded two-call
/// batch through the real `chat_stream`, so the ids come off the wire rather
/// than out of this test. The second posts the answers and captures what the
/// adapter actually sent, which is the only place a body the Messages API
/// would 400 can be caught.
///
/// Three documented 400s are asserted against: an unanswered `tool_use` id, a
/// `tool_result` that does not sit in the message immediately following the
/// assistant turn, and — the one this whole workstream is about — results
/// that are not bound to their calls by id. All three were true of the
/// pre-content-block adapter, which pushed each result as its own `user`
/// message and matched them by tool name plus FIFO.
#[cfg(feature = "provider-anthropic")]
#[tokio::test]
async fn anthropic_answers_a_recorded_parallel_batch_in_one_message() {
    let recorded = RecordedProvider::replay("anthropic/parallel_tool_calls.sse").await;
    let chunks = collect(&recorded.anthropic("claude-opus-5"), "claude-opus-5").await;

    assert_eq!(
        streamed_text(&chunks),
        vec![(false, "Reading both.".to_string())],
        "the text block streams live; neither tool_use block does"
    );
    let batch = decode_batch(&chunks);
    assert_eq!(batch.calls[0].id, "toolu_01A");
    assert_eq!(batch.calls[1].id, "toolu_01B");
    assert_eq!(batch.calls[0].function.arguments["path"], "a");
    assert_eq!(
        batch.calls[1].function.arguments["path"], "b",
        "the second call's input arrived in one fragment, the first's in two"
    );

    let last = chunks.last().expect("a final chunk");
    assert_eq!(last.done_reason.as_deref(), Some("tool_use"));
    // Anthropic reports the three prompt counts as siblings: the size is
    // their sum, and the split is what keeps the turn off the full input rate.
    assert_eq!(last.prompt_eval_count, Some(1_200 + 44_000 + 900));
    assert_eq!(
        last.cache,
        CacheTokens {
            read: 44_000,
            write: 900
        }
    );

    let answering = RecordedProvider::replay("anthropic/parallel_tool_calls.sse").await;
    let body = body_sent(
        &answering,
        &answering.anthropic("claude-opus-5"),
        "claude-opus-5",
        batch.follow_up,
    )
    .await;

    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3, "user, assistant, one answering message");
    let calls: Vec<&serde_json::Value> = messages[1]["content"]
        .as_array()
        .expect("assistant blocks")
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .collect();
    assert_eq!(calls.len(), 2, "{body}");

    // ONE message answers the whole batch, and it is the very next one.
    assert_eq!(messages[2]["role"], "user");
    let answers = messages[2]["content"].as_array().expect("result blocks");
    assert_eq!(answers.len(), 2, "one message, both results: {body}");
    for answer in answers {
        assert_eq!(answer["type"], "tool_result");
    }

    // Correlation is by id and by nothing else: the answers were attached in
    // reverse call order and both calls name the same tool, so neither
    // position nor name can be what put these two pairs together.
    let paired: Vec<(&str, &str)> = answers
        .iter()
        .map(|answer| {
            (
                answer["tool_use_id"].as_str().expect("an id"),
                answer["content"].as_str().expect("a body"),
            )
        })
        .collect();
    assert_eq!(
        paired,
        vec![
            ("toolu_01B", "contents of b"),
            ("toolu_01A", "contents of a")
        ],
        "{body}"
    );
    // Every issued id is answered; an unanswered one is a 400.
    let issued: Vec<&str> = calls
        .iter()
        .map(|call| call["id"].as_str().expect("an id"))
        .collect();
    for id in issued {
        assert!(
            paired.iter().any(|(answered, _)| *answered == id),
            "tool_use {id} went unanswered: {body}"
        );
    }
}

/// Exit criterion 8, Chat Completions leg. Same two round trips, and the same
/// question asked of a different wire shape: this API answers a batch with one
/// `tool` message per result rather than one message holding all of them, so
/// what "one result message per turn" means here is that the run of `tool`
/// messages is contiguous and immediately follows the assistant turn. Anything
/// interleaved into it — the images payload, a nudge — is a 400.
#[tokio::test]
async fn openai_answers_a_recorded_parallel_batch_in_one_contiguous_run() {
    let recorded = RecordedProvider::replay("openai/parallel_tool_calls.sse").await;
    let chunks = collect(&recorded.openai("gpt-4o"), "gpt-4o").await;

    let batch = decode_batch(&chunks);
    assert_eq!(batch.calls[0].id, "call_aaa");
    assert_eq!(batch.calls[1].id, "call_bbb");
    assert_eq!(batch.calls[0].function.arguments["path"], "a");
    assert_eq!(batch.calls[1].function.arguments["path"], "b");

    let last = chunks.last().expect("a final chunk");
    assert_eq!(last.done_reason.as_deref(), Some("tool_calls"));
    assert_eq!(last.prompt_eval_count, Some(2048));
    assert_eq!(
        last.cache,
        CacheTokens {
            read: 1_920,
            write: 0
        },
        "the cached prefix is a subset of prompt_tokens, never added to it"
    );

    let answering = RecordedProvider::replay("openai/parallel_tool_calls.sse").await;
    let body = body_sent(
        &answering,
        &answering.openai("gpt-4o"),
        "gpt-4o",
        batch.follow_up,
    )
    .await;

    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(
        messages.len(),
        5,
        "system, user, assistant, result, result: {body}"
    );
    let issued: Vec<&str> = messages[2]["tool_calls"]
        .as_array()
        .expect("the assistant turn carries its calls")
        .iter()
        .map(|call| call["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(issued, vec!["call_aaa", "call_bbb"]);

    let paired: Vec<(&str, &str)> = messages[3..5]
        .iter()
        .map(|message| {
            assert_eq!(
                message["role"], "tool",
                "a batch's results must be contiguous: {message}"
            );
            (
                message["tool_call_id"].as_str().expect("an id"),
                message["content"].as_str().expect("a body"),
            )
        })
        .collect();
    assert_eq!(
        paired,
        vec![("call_bbb", "contents of b"), ("call_aaa", "contents of a")],
        "{body}"
    );
}

/// Exit criterion 8, Responses leg — the one with no recorded fixture at all
/// before this, only an in-process decoder test.
///
/// Three things this shape adds over the other two. The results are top-level
/// `function_call_output` items rather than messages, so "contiguous" is about
/// the `input` array. The reasoning item has to be replayed ahead of the calls
/// it produced, because this client sends `store: false` and the endpoint
/// remembers nothing. And the request goes out with OAuth credentials read
/// from disk, so this is also the only test that drives that path.
#[cfg(feature = "provider-chatgpt")]
#[tokio::test]
async fn responses_answers_a_recorded_parallel_batch_in_one_contiguous_run() {
    let recorded = RecordedProvider::replay("responses/parallel_tool_calls.sse").await;
    let chunks = collect(&recorded.responses("gpt-5.6-sol"), "gpt-5.6-sol").await;

    let batch = decode_batch(&chunks);
    assert_eq!(batch.calls[0].id, "call_a");
    assert_eq!(batch.calls[1].id, "call_b");
    assert_eq!(batch.calls[0].function.arguments["path"], "a.rs");
    assert_eq!(batch.calls[1].function.arguments["path"], "b.rs");

    let last = chunks.last().expect("a final chunk");
    assert_eq!(last.prompt_eval_count, Some(48_000));
    assert_eq!(
        last.cache,
        CacheTokens {
            read: 46_080,
            write: 0
        },
        "a store:false client re-sends the whole conversation every step, so \
         most of the prompt is a cache read and billing it fresh is not close"
    );

    let answering = RecordedProvider::replay("responses/parallel_tool_calls.sse").await;
    let body = body_sent(
        &answering,
        &answering.responses("gpt-5.6-sol"),
        "gpt-5.6-sol",
        batch.follow_up,
    )
    .await;

    let input = body["input"].as_array().expect("input items");
    let kinds: Vec<&str> = input
        .iter()
        .map(|item| item["type"].as_str().expect("a type"))
        .collect();
    assert_eq!(
        kinds,
        [
            "message",
            "reasoning",
            "function_call",
            "function_call",
            "function_call_output",
            "function_call_output",
        ],
        "the reasoning leads its own calls, and nothing is interleaved between \
         the two outputs: {body}"
    );
    assert_eq!(
        input[1]["encrypted_content"], "gAAAAAB-opaque",
        "the encrypted reasoning is replayed verbatim, or the model derives \
         its whole chain of thought again and is billed for it again"
    );

    let paired: Vec<(&str, &str)> = input[4..6]
        .iter()
        .map(|item| {
            (
                item["call_id"].as_str().expect("a call_id"),
                item["output"].as_str().expect("an output"),
            )
        })
        .collect();
    assert_eq!(
        paired,
        vec![("call_b", "contents of b"), ("call_a", "contents of a")],
        "{body}"
    );
}

/// The Responses fixture that is not a batch, so the provider directory holds
/// more than one recorded stream (see `every_recorded_fixture_is_usable_sse`).
///
/// Reasoning summary deltas and answer text arrive on different event names
/// and mean different things: a decoder that folds one into the other puts the
/// model's private deliberation into the transcript as its answer.
#[cfg(feature = "provider-chatgpt")]
#[tokio::test]
async fn responses_reasoning_stays_flagged_through_the_transport() {
    let recorded = RecordedProvider::replay("responses/reasoning_then_text.sse").await;
    let chunks = collect(&recorded.responses("gpt-5.6-sol"), "gpt-5.6-sol").await;

    assert_eq!(
        streamed_text(&chunks),
        vec![
            (true, "Weighing the ".to_string()),
            (true, "options.".to_string()),
            (false, "Four".to_string()),
            (false, "teen.".to_string()),
        ],
    );

    let last = chunks.last().expect("a final chunk");
    assert!(last.done);
    assert_eq!(last.prompt_eval_count, Some(11));
    assert_eq!(
        last.cache,
        CacheTokens::NONE,
        "no breakdown reported means all-fresh, which is the safe reading"
    );
}

/// Fixtures are only worth having if they stay usable. This is the guard: a
/// fixture that gets truncated to its comment header, saved with CRLF endings
/// by an editor, or added to a provider directory that no test drives would
/// otherwise sit there passing.
///
/// Scoped to the provider directories by name rather than to every child of
/// `tests/fixtures/`, because that directory now also holds recorded input for
/// other subsystems (`claude_sessions/` is `.jsonl`, being Claude Code's
/// on-disk transcript format rather than anything off an SSE socket). A new
/// provider has to be added to the list below, which is the point: the guard
/// only covers what it is told about, and silently walking every sibling
/// directory is how it came to fail on a fixture that was never its business.
#[test]
fn every_recorded_fixture_is_usable_sse() {
    const PROVIDERS: [&str; 3] = ["anthropic", "openai", "responses"];

    let root = fixtures_root();
    let mut providers = 0;
    for name in PROVIDERS {
        let provider = root.join(name);
        assert!(
            provider.is_dir(),
            "{} is missing; every name in PROVIDERS must have fixtures",
            provider.display()
        );
        providers += 1;

        let mut fixtures = 0;
        for entry in std::fs::read_dir(&provider).expect("read provider directory") {
            let path = entry.expect("read fixture entry").path();
            assert_eq!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("sse"),
                "{} is not a .sse fixture",
                path.display()
            );
            fixtures += 1;

            let body = std::fs::read_to_string(&path).expect("fixture is utf-8");
            assert!(
                !body.contains('\r'),
                "{} has CRLF line endings; fixtures are the bytes off the \
                 socket and must stay LF",
                path.display()
            );
            assert!(
                body.lines().any(|line| line.starts_with("data:")),
                "{} carries no data line, so it decodes to nothing",
                path.display()
            );
            assert!(
                body.lines()
                    .next()
                    .is_some_and(|line| line.starts_with(':')),
                "{} has no provenance header; start it with SSE comment lines \
                 saying which stream it came from",
                path.display()
            );
        }
        assert!(
            fixtures >= 2,
            "{} holds {fixtures} fixture(s); a provider with one recorded \
             stream is a provider whose format is still an assertion",
            provider.display()
        );
    }
    assert!(
        providers >= 3,
        "expected fixtures for at least three providers"
    );
}

/// Exit criterion 8 names three providers, and the fixture corpus is what
/// makes the claim checkable. This asserts each of them has a recorded batch
/// and that the batch really is one: two calls to one tool.
///
/// Without it a `parallel_tool_calls.sse` could be quietly reduced to a single
/// call — by a re-record against a model that happened not to fan out — and
/// every test above would keep passing while covering nothing.
#[test]
fn every_provider_has_a_recorded_two_call_batch() {
    for provider in ["anthropic", "openai", "responses"] {
        let path = fixtures_root()
            .join(provider)
            .join("parallel_tool_calls.sse");
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        let data: String = body
            .lines()
            .filter(|line| line.starts_with("data:"))
            .collect::<Vec<_>>()
            .join("\n");
        let calls = data.matches("read_file").count();
        assert!(
            calls >= 2,
            "{} names read_file {calls} time(s); a one-call stream is not a \
             parallel batch",
            path.display()
        );
    }
}
