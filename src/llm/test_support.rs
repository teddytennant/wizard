//! Fixtures and loopback servers shared by the provider adapters' tests.
//!
//! Compiled only under `cfg(test)`, so none of it — the recorded stream, the
//! two throwaway TCP servers — reaches a release build.
//!
//! Half of these fixtures used to live inside `openai.rs` as an
//! `openai::testing` module, which meant the tests of `ollama`, `llamacpp`
//! and `cloudflare` all reached into one provider's file for them. That is
//! the same coupling the `wire`/`openai` split exists to remove, one layer
//! down: a shared fixture is not OpenAI's, it is the family's.
//!
//! Which is also why it stays in core now that the adapters are plugins in
//! `src/plugins/`. A fixture module that lived in one of them would be a
//! dependency edge between plugins, and deleting that plugin would take four
//! other plugins' tests with it.
//!
//! The point of serving a recording over a real socket, rather than feeding
//! bytes to a decoder in-process, is that it captures **the request the
//! adapter sent**. Half of every adapter's contract is the body it puts on
//! the wire, and an in-process decoder test can never see it: a body the
//! provider would reject with an HTTP 400 passes those tests forever.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One-shot HTTP responder on loopback: accepts a single connection,
/// reads the request, writes `response` verbatim and closes. Enough to
/// drive a provider's real failure path, headers included, without
/// taking a mock-HTTP dependency. Returns the server root to point a
/// provider at (no trailing slash, no `/v1`).
pub(crate) async fn one_shot_http_server(response: &'static str) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            // Drain the rest of the request before dropping the socket:
            // closing one that still has unread bytes in its receive
            // buffer sends an RST, which would tear down the reply the
            // client has not read yet and make the test flaky.
            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                while let Ok(read) = socket.read(&mut buf).await {
                    if read == 0 {
                        break;
                    }
                }
            })
            .await;
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// A recorded two-call parallel batch, **both calls naming the same
/// tool**. Transcribed from a `gpt-4o` stream: each call opens on its own
/// `index` carrying its own `call_…` id, and the arguments arrive in
/// fragments afterwards, interleaved between the two indices.
///
/// Same-tool is the case the whole id change exists for. Correlating a
/// result to a call by tool name cannot represent it: both calls here are
/// `read_file`, so a name lookup has two answers and picks one.
///
/// **The bytes live on disk**, not in this literal, and they are the same
/// bytes `tests/recorded_provider.rs` serves over a socket. An inline
/// literal asserts what the author believed the wire format was and keeps
/// asserting it after the provider changes; a fixture at least fails in
/// one place when it is re-recorded, and cannot drift from the copy the
/// transport tests use because there is no second copy.
pub(crate) const PARALLEL_TOOL_BATCH_SSE: &str =
    include_str!("../../tests/fixtures/openai/parallel_tool_calls.sse");

/// A history whose last turn is a two-call parallel batch to **one** tool
/// and the two answers to it, plus the system prompt that gives a keyed
/// prompt cache something to key on.
///
/// Shared by all four adapters in this family so they are all asked the
/// same question: given one `Role::Tool` message holding a whole batch,
/// what goes on the wire?
pub(crate) fn parallel_batch_request(model: &str) -> crate::llm::ChatRequest {
    use crate::llm::{ChatMessage, ChatRequest, FunctionCall, ToolCall, ToolSpec};

    // Written out rather than built with `ToolCall::new`, which mints a
    // synthetic id: these ids are the provider's own, and the fixture
    // stream answers to exactly these two.
    let mut assistant = ChatMessage::assistant("reading both");
    assistant.push_tool_call(ToolCall {
        id: "call_aaa".to_string(),
        function: FunctionCall {
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "a" }),
        },
    });
    assistant.push_tool_call(ToolCall {
        id: "call_bbb".to_string(),
        function: FunctionCall {
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "b" }),
        },
    });
    let mut results = ChatMessage::tool_result("call_aaa", "read_file", "contents of a");
    results.push_tool_result("call_bbb", "read_file", "contents of b");

    ChatRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("read both"),
            assistant,
            results,
        ],
        tools: vec![ToolSpec::function(
            "read_file",
            "Read a file.",
            serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        )],
        stream: true,
        options: None,
    }
}

/// A recorded stream, served once over loopback.
pub(crate) struct Recorded {
    /// Server root to point a provider at: no trailing slash and no
    /// `/v1`, because the adapters in this family disagree about which of
    /// them owns that suffix.
    pub(crate) root: String,
    captured: Arc<Mutex<Option<String>>>,
}

impl Recorded {
    /// Bind a loopback port that captures the next request's body and
    /// answers it with `body` as a chunked `text/event-stream`.
    ///
    /// Chunked, and in frames far smaller than one event, because that is
    /// what hands the decoder reads that start and end mid-JSON: the
    /// reassembly buffer is the part an in-process stream of whole events
    /// never exercises.
    pub(crate) async fn replay(body: &'static str) -> Self {
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

            let head = "HTTP/1.1 200 OK\r\n\
                        content-type: text/event-stream\r\n\
                        cache-control: no-cache\r\n\
                        transfer-encoding: chunked\r\n\
                        \r\n";
            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            for frame in body.as_bytes().chunks(48) {
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

            // Drain before dropping: closing a socket that still has
            // unread bytes in its receive buffer sends an RST, which
            // would tear down the response the client is still reading.
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

    /// The request body the adapter put on the wire, parsed. Panics when
    /// nothing connected, which can only mean the request was never sent.
    pub(crate) fn request_body(&self) -> serde_json::Value {
        let raw = self
            .captured
            .lock()
            .expect("capture lock")
            .clone()
            .expect("the adapter sent a request");
        serde_json::from_str(&raw).expect("the adapter sent valid json")
    }
}

/// Read one HTTP request off `socket` and return its body. Byte at a time
/// through the head, so none of the body is swallowed before
/// `content-length` is known.
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

/// Assert that `messages` answers the parallel batch on the assistant
/// message at `assistant` the way this API requires: every result is its
/// own `tool`-role message, they are **consecutive** (nothing spliced
/// between them), and each `tool_call_id` is one of the ids the assistant
/// turn actually issued.
///
/// The third clause is the one that fails when a result is matched to a
/// call by name: two calls to the same tool produce two results, and a
/// name match hands both of them the first call's id.
pub(crate) fn assert_batch_is_answerable(messages: &[serde_json::Value], assistant: usize) {
    let issued: Vec<&str> = messages[assistant]["tool_calls"]
        .as_array()
        .expect("the assistant turn carries its tool calls")
        .iter()
        .map(|call| call["id"].as_str().expect("every call has an id"))
        .collect();
    assert!(
        issued.len() > 1,
        "this assertion is about a *batch*; got {issued:?}"
    );

    let answers = &messages[assistant + 1..assistant + 1 + issued.len()];
    let answered: Vec<&str> = answers
        .iter()
        .map(|message| {
            assert_eq!(
                message["role"], "tool",
                "a batch's results must be consecutive: {message}"
            );
            message["tool_call_id"]
                .as_str()
                .expect("every result names the call it answers")
        })
        .collect();
    assert_eq!(
        answered, issued,
        "each result must carry the id of the call it answers, in order"
    );
}
