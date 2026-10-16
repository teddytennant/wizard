//! Voice over xAI's realtime API (Grok Voice) with this device's Wizard
//! sign-in to xAI (`wizard --login xai`). The call runs in this window: the
//! microphone and speaker are this device's ([`zeron_audio::duplex`]), Grok
//! Voice talks over a WebSocket to api.x.ai, and its tool calls work the
//! user's chats through the engine's own MCP tools. A dropped socket resumes
//! the same conversation instead of ending the call.
use super::VoiceControl;
use crate::state::EngineHandle;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest as _, http};
use tokio_util::sync::CancellationToken;
use zeron_proto::voice::*;

pub const MODEL: &str = "grok-voice-3.0";
/// Grok Voice's built-in voices; the first is the default.
pub const VOICES: &[&str] = &["eve", "ara", "rex", "sal", "leo"];
const ENDPOINT: &str = "wss://api.x.ai/v1/realtime";
/// A tool that waits on an agent hands control back after this long, so the
/// call never sits silent: the model says the agent is still working.
const TOOL_WAIT_CAP_SECS: u64 = 30;
/// Tool results past this are cut; transcripts can be long.
const TOOL_OUTPUT_MAX: usize = 16_000;
const RECONNECTS: u32 = 3;
/// Token freshness the stored-token fallback requires.
const TOKEN_LEEWAY_SECS: i64 = 120;

pub const INSTRUCTIONS: &str = "You are the voice of Wizard GUI, a desktop app that runs coding agents \
(Wizard, Pi and Claude) in chats across the user's projects and machines. You are the user's \
orchestrator on a live call. Get work done through the tools: list projects and chats, read what \
agents said, start new chats with the right agent and project, send them messages, check on their \
progress, and answer an agent's question with respond_to_input when the user tells you what to say. \
Delegate coding, file and shell work to those chats instead of explaining how to do it yourself. \
New chats use the wizard harness unless the user asks for another agent. Waiting on an agent is \
capped at about thirty seconds; if it is still working, say so and offer to check back. Speak \
briefly and naturally, like a colleague on a call: one or two short sentences, no lists, no \
markdown, and never read out ids, hashes or long paths. Say a few words before a tool call that \
takes a moment. If you are not sure what the user wants, ask.";

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Where the far end's audio goes: the call's speaker, or a test's recorder.
trait Playout: Send + Sync {
    fn play(&self, pcm: &[u8]);
    fn flush(&self);
}
impl Playout for zeron_audio::duplex::Duplex {
    fn play(&self, pcm: &[u8]) {
        zeron_audio::duplex::Duplex::play(self, pcm);
    }
    fn flush(&self) {
        zeron_audio::duplex::Duplex::flush(self);
    }
}

/// Whether this device's Wizard is signed in to xAI. Reads a file; call it
/// off the UI thread.
pub fn signed_in() -> bool {
    std::fs::read_to_string(token_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|tokens| tokens["access_token"].as_str().map(|t| !t.is_empty()))
        .unwrap_or(false)
}

fn token_path() -> std::path::PathBuf {
    zeron_engine::wizard_auth::wizard_home().join("xai_oauth.json")
}

pub(super) async fn run(
    engine: EngineHandle,
    voice: Option<String>,
    microphone: Option<String>,
    cancel: CancellationToken,
    events: mpsc::Sender<VoiceEvent>,
    mut controls: mpsc::Receiver<VoiceControl>,
) -> Result<(), VoiceRejection> {
    let voice = voice
        .filter(|v| VOICES.contains(&v.as_str()))
        .unwrap_or_else(|| VOICES[0].to_owned());
    let mut call = Call::new(events, &voice);
    call.publish().await?;
    super::permissions::microphone().await?;

    let tools = Arc::new(zeron_mcp::Tools::new(Arc::new(
        zeron_mcp::Zeron::with_client(
            engine
                .media_client()
                .await
                .map_err(|_| VoiceRejection::Protocol)?,
            zeron_mcp::Origin::default(),
        ),
    )));
    let functions = function_definitions(&tools);

    // Open the devices while the socket connects.
    let (frames, mut microphone_frames) = mpsc::channel::<Vec<u8>>(64);
    let audio = tokio::task::spawn_blocking(move || {
        zeron_audio::duplex::Duplex::start(microphone, move |pcm| {
            let _ = frames.try_send(pcm);
        })
    });
    let token = access_token().await?;
    let socket = connect(&token, None).await;
    let duplex = audio
        .await
        .map_err(|_| VoiceRejection::DeviceUnavailable)?
        .map_err(|_| VoiceRejection::DeviceUnavailable)?;
    let mut socket = socket?;
    configure(&mut socket, &voice, &functions).await?;

    let (tool_done, mut tools_done) = mpsc::channel::<ToolResult>(16);
    let mut levels = tokio::time::interval(Duration::from_millis(50));
    let mut reconnects = 0;
    loop {
        tokio::select! {biased;
            _ = cancel.cancelled() => break,
            control = controls.recv() => match control {
                Some(VoiceControl::Mute(muted)) => {
                    duplex.set_muted(muted);
                    call.snapshot.muted = muted;
                    call.publish().await?;
                }
                None => break,
            },
            message = socket.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let Ok(event) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    if event["type"] == "session.updated" {
                        reconnects = 0;
                    }
                    call.handle(event, &duplex, &mut socket, &tools, &tool_done).await?;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    let Some(conversation) = call.conversation.clone() else {
                        return Err(VoiceRejection::Protocol);
                    };
                    duplex.flush();
                    call.reset_turn();
                    call.snapshot.phase = VoicePhase::Starting;
                    call.publish().await?;
                    // Resume the same conversation; it keeps the call's context.
                    let resumed = loop {
                        if reconnects >= RECONNECTS {
                            return Err(VoiceRejection::Protocol);
                        }
                        reconnects += 1;
                        tracing::info!(attempt = reconnects, "voice socket dropped; resuming");
                        tokio::select! {
                            _ = cancel.cancelled() => break None,
                            _ = tokio::time::sleep(Duration::from_millis(400 * u64::from(reconnects))) => {}
                        }
                        let token = access_token().await?;
                        if let Ok(mut fresh) = connect(&token, Some(&conversation)).await
                            && configure(&mut fresh, &voice, &functions).await.is_ok()
                        {
                            break Some(fresh);
                        }
                    };
                    match resumed {
                        Some(fresh) => socket = fresh,
                        None => break,
                    }
                }
                Some(Ok(_)) => {}
            },
            pcm = microphone_frames.recv() => {
                let Some(pcm) = pcm else {
                    return Err(VoiceRejection::DeviceUnavailable);
                };
                if call.configured {
                    // A failed send surfaces as the socket's error on the next read.
                    let _ = send(&mut socket, json!({
                        "type": "input_audio_buffer.append",
                        "audio": STANDARD.encode(pcm),
                    })).await;
                }
            },
            result = tools_done.recv() => {
                if let Some(result) = result {
                    call.tool_finished(result, &mut socket).await?;
                }
            },
            _ = levels.tick() => {
                if duplex.failed() {
                    return Err(VoiceRejection::DeviceUnavailable);
                }
                let (microphone, speaker) = duplex.levels();
                let scale = |level: f32| ((level * 4.0).clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16;
                let _ = call.events.try_send(VoiceEvent::Levels {
                    generation: call.snapshot.generation,
                    microphone: scale(microphone),
                    speaker: scale(speaker),
                });
            },
        }
    }
    let _ = socket.close(None).await;
    drop(duplex);
    Ok(())
}

struct ToolResult {
    response: String,
    call_id: String,
    output: String,
}

#[derive(Default)]
struct PendingTools {
    outstanding: usize,
    outputs: Vec<(String, String)>,
    /// The response that asked for them has finished streaming.
    done: bool,
}

struct Call {
    events: mpsc::Sender<VoiceEvent>,
    snapshot: VoiceSnapshot,
    conversation: Option<String>,
    /// The session took our configuration; microphone audio may flow.
    configured: bool,
    /// The response streaming now.
    response: Option<String>,
    /// Responses the user talked over: their remaining audio is dropped.
    interrupted: HashSet<String>,
    pending: HashMap<String, PendingTools>,
    /// Tool results went in while another response was streaming; ask for
    /// the follow-up once that one ends.
    continue_after_response: bool,
    running_tools: usize,
}

impl Call {
    fn new(events: mpsc::Sender<VoiceEvent>, voice: &str) -> Self {
        let session_id = uuid::Uuid::new_v4().to_string();
        Self {
            events,
            snapshot: VoiceSnapshot {
                chat_id: format!("{ORCHESTRATOR_CHAT_PREFIX}grok-{session_id}"),
                session_id,
                generation: 1,
                phase: VoicePhase::Starting,
                muted: false,
                playing: false,
                work: VoiceWork::Idle,
                reason: None,
                voice: Some(voice.to_owned()),
                voices: VOICES.iter().map(|v| (*v).to_owned()).collect(),
            },
            conversation: None,
            configured: false,
            response: None,
            interrupted: HashSet::new(),
            pending: HashMap::new(),
            continue_after_response: false,
            running_tools: 0,
        }
    }

    async fn publish(&self) -> Result<(), VoiceRejection> {
        self.events
            .send(VoiceEvent::Snapshot {
                snapshot: self.snapshot.clone(),
            })
            .await
            .map_err(|_| VoiceRejection::Protocol)
    }

    async fn emit(&self, event: VoiceEvent) -> Result<(), VoiceRejection> {
        self.events
            .send(event)
            .await
            .map_err(|_| VoiceRejection::Protocol)
    }

    async fn partial(&self, item: &str, text: &str) -> Result<(), VoiceRejection> {
        self.emit(VoiceEvent::Partial {
            generation: self.snapshot.generation,
            item_id: Some(item.to_owned()),
            text: text.to_owned(),
        })
        .await
    }

    async fn final_text(
        &self,
        role: VoiceRole,
        item: &str,
        text: &str,
    ) -> Result<(), VoiceRejection> {
        self.emit(VoiceEvent::Final {
            transcript: VoiceTranscript {
                session_id: self.snapshot.session_id.clone(),
                item_id: item.to_owned(),
                role,
                text: text.trim().to_owned(),
                promoted_message_id: None,
            },
        })
        .await
    }

    fn interrupted(&self, event: &Value) -> bool {
        event["response_id"]
            .as_str()
            .is_some_and(|id| self.interrupted.contains(id))
    }

    /// Forget the streaming turn (after a reconnect: nothing in flight
    /// survives the old socket).
    fn reset_turn(&mut self) {
        self.configured = false;
        if let Some(response) = self.response.take() {
            self.interrupted.insert(response);
        }
    }

    async fn handle<S: Sink>(
        &mut self,
        event: Value,
        duplex: &dyn Playout,
        socket: &mut S,
        tools: &Arc<zeron_mcp::Tools>,
        tool_done: &mpsc::Sender<ToolResult>,
    ) -> Result<(), VoiceRejection> {
        let text = |key: &str| event[key].as_str().unwrap_or_default();
        match text("type") {
            "conversation.created" => {
                self.conversation = event["conversation"]["id"].as_str().map(str::to_owned);
            }
            "session.updated" => {
                if !self.configured {
                    self.configured = true;
                    self.snapshot.phase = VoicePhase::Active;
                    self.publish().await?;
                }
            }
            // The user started talking: whatever the agent still had to say
            // is dropped here at once, and the rest of that reply with it.
            "input_audio_buffer.speech_started" => {
                if let Some(response) = self.response.take() {
                    self.interrupted.insert(response);
                }
                duplex.flush();
                self.partial(text("item_id"), "").await?;
            }
            "conversation.item.input_audio_transcription.completed" => {
                self.final_text(VoiceRole::User, text("item_id"), text("transcript"))
                    .await?;
            }
            "response.created" => {
                self.response = event["response"]["id"].as_str().map(str::to_owned);
            }
            "response.output_audio.delta" | "response.audio.delta" => {
                if !self.interrupted(&event)
                    && let Ok(pcm) = STANDARD.decode(text("delta"))
                {
                    duplex.play(&pcm);
                }
            }
            "response.output_audio_transcript.delta" | "response.audio_transcript.delta" => {
                if !self.interrupted(&event) {
                    self.partial(text("item_id"), text("delta")).await?;
                }
            }
            "response.output_audio_transcript.done" | "response.audio_transcript.done" => {
                if !self.interrupted(&event) {
                    self.final_text(VoiceRole::Assistant, text("item_id"), text("transcript"))
                        .await?;
                }
            }
            "response.function_call_arguments.done" => {
                self.start_tool(
                    text("response_id").to_owned(),
                    text("call_id").to_owned(),
                    text("name").to_owned(),
                    text("arguments").to_owned(),
                    tools,
                    tool_done,
                )
                .await?;
            }
            "response.done" => {
                let id = event["response"]["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if self.response.as_deref() == Some(id.as_str()) {
                    self.response = None;
                }
                if let Some(pending) = self.pending.get_mut(&id) {
                    pending.done = true;
                }
                self.continue_tools(&id, socket).await?;
                if self.response.is_none() && std::mem::take(&mut self.continue_after_response) {
                    send(socket, json!({ "type": "response.create" })).await?;
                }
                if self.interrupted.len() > 64 {
                    self.interrupted.clear();
                }
            }
            "error" => {
                let message = event["error"]["message"].as_str().unwrap_or_default();
                let code = event["error"]["code"].as_str().unwrap_or_default();
                tracing::warn!(code, message, "Grok Voice reported an error");
                if matches!(
                    code,
                    "invalid_api_key" | "unauthorized" | "authentication_error"
                ) {
                    return Err(VoiceRejection::SignInRequired);
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn start_tool(
        &mut self,
        response: String,
        call_id: String,
        name: String,
        arguments: String,
        tools: &Arc<zeron_mcp::Tools>,
        tool_done: &mpsc::Sender<ToolResult>,
    ) -> Result<(), VoiceRejection> {
        self.pending
            .entry(response.clone())
            .or_default()
            .outstanding += 1;
        self.running_tools += 1;
        if self.running_tools == 1 {
            self.snapshot.work = VoiceWork::Working;
            self.publish().await?;
        }
        let tools = tools.clone();
        let done = tool_done.clone();
        tokio::spawn(async move {
            let output = call_tool(&tools, &name, &arguments).await;
            let _ = done
                .send(ToolResult {
                    response,
                    call_id,
                    output,
                })
                .await;
        });
        Ok(())
    }

    async fn tool_finished<S: Sink>(
        &mut self,
        result: ToolResult,
        socket: &mut S,
    ) -> Result<(), VoiceRejection> {
        self.running_tools = self.running_tools.saturating_sub(1);
        if self.running_tools == 0 {
            self.snapshot.work = VoiceWork::Idle;
            self.publish().await?;
        }
        if let Some(pending) = self.pending.get_mut(&result.response) {
            pending.outstanding = pending.outstanding.saturating_sub(1);
            pending.outputs.push((result.call_id, result.output));
        }
        self.continue_tools(&result.response, socket).await
    }

    /// Once a response that called tools has ended and all of its calls are
    /// answered, hand the results back and let the model go on.
    async fn continue_tools<S: Sink>(
        &mut self,
        response: &str,
        socket: &mut S,
    ) -> Result<(), VoiceRejection> {
        let ready = self
            .pending
            .get(response)
            .is_some_and(|p| p.done && p.outstanding == 0);
        if !ready {
            return Ok(());
        }
        let pending = self.pending.remove(response).unwrap_or_default();
        if pending.outputs.is_empty() {
            return Ok(());
        }
        for (call_id, output) in pending.outputs {
            send(
                socket,
                json!({
                    "type": "conversation.item.create",
                    "item": { "type": "function_call_output", "call_id": call_id, "output": output },
                }),
            )
            .await?;
        }
        if self.response.is_some() {
            self.continue_after_response = true;
        } else {
            send(socket, json!({ "type": "response.create" })).await?;
        }
        Ok(())
    }
}

/// The socket's sending half, as far as a call's handlers need it.
trait Sink: futures::Sink<Message> + Unpin {}
impl<S: futures::Sink<Message> + Unpin> Sink for S {}

async fn send<S: Sink>(socket: &mut S, event: Value) -> Result<(), VoiceRejection> {
    socket
        .send(Message::Text(event.to_string()))
        .await
        .map_err(|_| VoiceRejection::Protocol)
}

/// The engine's MCP tools as realtime functions. `show_visual` needs a turn
/// of its own to draw into, which a voice call does not have.
fn function_definitions(tools: &zeron_mcp::Tools) -> Vec<Value> {
    tools
        .list()
        .into_iter()
        .filter(|tool| tool.name != "show_visual")
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect()
}

async fn call_tool(tools: &zeron_mcp::Tools, name: &str, arguments: &str) -> String {
    let mut args: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
    cap_waits(name, &mut args);
    let output = match tools.call(name, args).await {
        Ok(value) => value.to_string(),
        Err(message) => json!({ "error": message }).to_string(),
    };
    truncate(output, TOOL_OUTPUT_MAX)
}

/// Every wait a call asks for (a `wait: true` send, `wait_for_turn`, and the
/// same inside batches) ends after [`TOOL_WAIT_CAP_SECS`] at most.
fn cap_waits(name: &str, args: &mut Value) {
    fn walk(value: &mut Value, waits: bool) {
        match value {
            Value::Object(map) => {
                if waits || map.get("wait").and_then(Value::as_bool) == Some(true) {
                    let secs = map
                        .get("timeout_secs")
                        .and_then(Value::as_u64)
                        .unwrap_or(u64::MAX)
                        .min(TOOL_WAIT_CAP_SECS);
                    map.insert("timeout_secs".into(), secs.into());
                }
                for child in map.values_mut() {
                    walk(child, false);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, false);
                }
            }
            _ => {}
        }
    }
    walk(args, name == "wait_for_turn");
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("…(truncated)");
    }
    text
}

async fn connect(token: &str, conversation: Option<&str>) -> Result<Socket, VoiceRejection> {
    let mut url = format!("{ENDPOINT}?model={MODEL}");
    if let Some(conversation) =
        conversation.filter(|id| id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    {
        url.push_str("&conversation_id=");
        url.push_str(conversation);
    }
    let mut request = url
        .into_client_request()
        .map_err(|_| VoiceRejection::Protocol)?;
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        format!("Bearer {token}")
            .parse()
            .map_err(|_| VoiceRejection::SignInRequired)?,
    );
    match tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(request),
    )
    .await
    {
        Err(_) => Err(VoiceRejection::Protocol),
        Ok(Ok((socket, _))) => Ok(socket),
        Ok(Err(tungstenite::Error::Http(response)))
            if matches!(response.status().as_u16(), 401 | 403) =>
        {
            Err(VoiceRejection::SignInRequired)
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "could not reach Grok Voice");
            Err(VoiceRejection::Protocol)
        }
    }
}

async fn configure(
    socket: &mut Socket,
    voice: &str,
    functions: &[Value],
) -> Result<(), VoiceRejection> {
    let pcm = json!({ "type": "audio/pcm", "rate": 24_000 });
    send(
        socket,
        json!({
            "type": "session.update",
            "session": {
                "voice": voice,
                "instructions": INSTRUCTIONS,
                "turn_detection": { "type": "server_vad" },
                "audio": {
                    "input": { "format": pcm },
                    "output": { "format": pcm },
                },
                "tools": functions,
                "resumption": { "enabled": true },
            },
        }),
    )
    .await
}

/// A fresh access token. Wizard owns `~/.wizard/xai_oauth.json` and refreshes
/// it under its lock (`wizard xai-token`); an older Wizard without that
/// command leaves the stored token, used only while it is still fresh, since
/// its refresh token is single-use and belongs to Wizard.
async fn access_token() -> Result<String, VoiceRejection> {
    let wizard = tokio::task::spawn_blocking(|| zeron_harness::AcpHarness::wizard().cli_path())
        .await
        .ok()
        .flatten();
    if let Some(wizard) = wizard {
        let mut command = tokio::process::Command::new(wizard);
        command
            .arg("xai-token")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        if let Ok(Ok(output)) =
            tokio::time::timeout(Duration::from_secs(20), command.output()).await
            && output.status.success()
            && let Some(token) = serde_json::from_slice::<Value>(&output.stdout)
                .ok()
                .and_then(|reply| reply["accessToken"].as_str().map(str::to_owned))
                .filter(|token| !token.is_empty())
        {
            return Ok(token);
        }
    }
    tokio::task::spawn_blocking(stored_token)
        .await
        .ok()
        .flatten()
        .ok_or(VoiceRejection::SignInRequired)
}

fn stored_token() -> Option<String> {
    let tokens: Value = serde_json::from_str(&std::fs::read_to_string(token_path()).ok()?).ok()?;
    let token = tokens["access_token"].as_str()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    (jwt_expiry(token)? > now + TOKEN_LEEWAY_SECS).then(|| token.to_owned())
}

fn jwt_expiry(token: &str) -> Option<i64> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?["exp"].as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_are_capped_everywhere_a_call_can_ask_for_one() {
        let mut send = json!({ "chat": "a", "text": "go", "wait": true });
        cap_waits("send_message", &mut send);
        assert_eq!(send["timeout_secs"], TOOL_WAIT_CAP_SECS);
        let mut quick = json!({ "chat": "a", "wait": true, "timeout_secs": 5 });
        cap_waits("send_message", &mut quick);
        assert_eq!(quick["timeout_secs"], 5);
        let mut fire = json!({ "chat": "a", "text": "go" });
        cap_waits("send_message", &mut fire);
        assert!(fire.get("timeout_secs").is_none());
        let mut wait = json!({ "chat": "a", "timeout_secs": 900 });
        cap_waits("wait_for_turn", &mut wait);
        assert_eq!(wait["timeout_secs"], TOOL_WAIT_CAP_SECS);
        let mut batch = json!({ "items": [{ "prompt": "x", "wait": true }, { "prompt": "y" }] });
        cap_waits("create_chats", &mut batch);
        assert_eq!(batch["items"][0]["timeout_secs"], TOOL_WAIT_CAP_SECS);
        assert!(batch["items"][1].get("timeout_secs").is_none());
    }

    #[test]
    fn long_tool_output_is_cut_on_a_character_boundary() {
        let text = "é".repeat(10);
        let cut = truncate(text, 5);
        assert!(cut.starts_with("éé"));
        assert!(cut.ends_with("(truncated)"));
        assert_eq!(truncate("short".into(), 50), "short");
    }

    #[test]
    fn token_expiry_reads_the_jwt_exp_claim() {
        let payload = URL_SAFE.encode(br#"{"sub":"u","exp":1792125004}"#);
        assert_eq!(jwt_expiry(&format!("h.{payload}.s")), Some(1_792_125_004));
        assert_eq!(jwt_expiry("garbage"), None);
    }

    use base64::engine::general_purpose::URL_SAFE;

    #[derive(Default)]
    struct Recorder {
        played: std::sync::Mutex<usize>,
        flushes: std::sync::Mutex<usize>,
    }
    impl Playout for Recorder {
        fn play(&self, pcm: &[u8]) {
            *self.played.lock().unwrap() += pcm.len();
        }
        fn flush(&self) {
            *self.flushes.lock().unwrap() += 1;
        }
    }

    #[tokio::test]
    async fn barge_in_drops_the_rest_of_a_reply_and_tool_results_continue_the_turn() {
        let (events, mut seen) = mpsc::channel(64);
        let mut call = Call::new(events, "eve");
        let audio = Recorder::default();
        let (mut sink, mut sent) = futures::channel::mpsc::unbounded::<Message>();
        // No engine behind these tools: the call answers with its error,
        // which is still an answer the model gets back.
        let tools = Arc::new(zeron_mcp::Tools::new(Arc::new(zeron_mcp::Zeron::new(
            String::new(),
            zeron_mcp::Origin::default(),
        ))));
        let (tool_done, mut tools_done) = mpsc::channel(4);
        let delta = STANDARD.encode([0u8; 960]);
        let script = [
            json!({ "type": "conversation.created", "conversation": { "id": "conv-1" } }),
            json!({ "type": "session.updated" }),
            json!({ "type": "response.created", "response": { "id": "r1" } }),
            json!({ "type": "response.output_audio.delta", "response_id": "r1", "item_id": "a1", "delta": delta }),
            json!({ "type": "input_audio_buffer.speech_started", "item_id": "u1" }),
            json!({ "type": "response.output_audio.delta", "response_id": "r1", "item_id": "a1", "delta": delta }),
            json!({ "type": "response.output_audio_transcript.delta", "response_id": "r1", "item_id": "a1", "delta": "late" }),
            json!({ "type": "response.done", "response": { "id": "r1" } }),
            json!({ "type": "conversation.item.input_audio_transcription.completed", "item_id": "u1", "transcript": " List my chats. " }),
            json!({ "type": "response.created", "response": { "id": "r2" } }),
            json!({ "type": "response.function_call_arguments.done", "response_id": "r2", "call_id": "c1", "name": "list_chats", "arguments": "{}" }),
            json!({ "type": "response.done", "response": { "id": "r2" } }),
        ];
        for event in script {
            call.handle(event, &audio, &mut sink, &tools, &tool_done)
                .await
                .unwrap();
        }
        assert_eq!(call.conversation.as_deref(), Some("conv-1"));
        assert_eq!(call.snapshot.phase, VoicePhase::Active);
        assert_eq!(
            *audio.played.lock().unwrap(),
            960,
            "nothing after the interruption plays"
        );
        assert_eq!(*audio.flushes.lock().unwrap(), 1);
        assert_eq!(call.snapshot.work, VoiceWork::Working);

        let result = tools_done.recv().await.unwrap();
        assert_eq!(result.call_id, "c1");
        call.tool_finished(result, &mut sink).await.unwrap();
        assert_eq!(call.snapshot.work, VoiceWork::Idle);

        let mut messages = Vec::new();
        while let Ok(Message::Text(text)) = sent.try_recv() {
            messages.push(serde_json::from_str::<Value>(&text).unwrap());
        }
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[0]["item"]["type"], "function_call_output");
        assert_eq!(messages[0]["item"]["call_id"], "c1");
        assert!(
            messages[0]["item"]["output"]
                .as_str()
                .unwrap()
                .contains("error")
        );
        assert_eq!(messages[1]["type"], "response.create");

        // Captions: the user's turn opened on speech, and its final is trimmed;
        // the interrupted reply's late words never reached them.
        let mut partials = Vec::new();
        let mut finals = Vec::new();
        while let Ok(event) = seen.try_recv() {
            match event {
                VoiceEvent::Partial { text, .. } => partials.push(text),
                VoiceEvent::Final { transcript } => finals.push(transcript.text),
                _ => {}
            }
        }
        assert!(!partials.iter().any(|text| text == "late"), "{partials:?}");
        assert_eq!(finals, vec!["List my chats."]);
    }

    #[tokio::test]
    async fn tool_results_wait_for_a_streaming_reply_before_asking_for_more() {
        let (events, _seen) = mpsc::channel(64);
        let mut call = Call::new(events, "eve");
        let (mut sink, mut sent) = futures::channel::mpsc::unbounded::<Message>();
        call.pending.insert(
            "r1".into(),
            PendingTools {
                outstanding: 1,
                outputs: Vec::new(),
                done: true,
            },
        );
        call.running_tools = 1;
        // The user spoke meanwhile and the model is answering that.
        call.response = Some("r2".into());
        call.tool_finished(
            ToolResult {
                response: "r1".into(),
                call_id: "c1".into(),
                output: "{}".into(),
            },
            &mut sink,
        )
        .await
        .unwrap();
        let first: Value = match sent.try_recv() {
            Ok(Message::Text(text)) => serde_json::from_str(&text).unwrap(),
            other => panic!("{other:?}"),
        };
        assert_eq!(first["item"]["type"], "function_call_output");
        assert!(
            sent.try_recv().is_err(),
            "no response.create while r2 streams"
        );
        assert!(call.continue_after_response);
    }

    /// Talks to the real Grok Voice with this machine's Wizard sign-in:
    /// `cargo test -p zeron-ui --lib voice::xai::tests::live -- --ignored`.
    #[tokio::test]
    #[ignore = "needs network and an xAI sign-in"]
    async fn live_text_turn_streams_speech_and_a_transcript() {
        let token = access_token().await.expect("signed in to xAI");
        let tools = Arc::new(zeron_mcp::Tools::new(Arc::new(zeron_mcp::Zeron::new(
            String::new(),
            zeron_mcp::Origin::default(),
        ))));
        let mut socket = connect(&token, None).await.expect("connects");
        configure(&mut socket, "rex", &function_definitions(&tools))
            .await
            .unwrap();
        let (events, mut seen) = mpsc::channel(256);
        let mut call = Call::new(events, "rex");
        let audio = Recorder::default();
        let (tool_done, _tools_done) = mpsc::channel(4);
        let mut asked = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, socket.next()).await {
            let Message::Text(text) = message else {
                continue;
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            let done = event["type"] == "response.done";
            call.handle(event, &audio, &mut socket, &tools, &tool_done)
                .await
                .unwrap();
            if call.configured && !asked {
                asked = true;
                send(&mut socket, json!({
                    "type": "conversation.item.create",
                    "item": { "type": "message", "role": "user",
                              "content": [{ "type": "input_text", "text": "Say hello in three words." }] },
                }))
                .await
                .unwrap();
                send(&mut socket, json!({ "type": "response.create" }))
                    .await
                    .unwrap();
            }
            if done && asked {
                break;
            }
        }
        assert!(call.conversation.is_some());
        assert!(
            *audio.played.lock().unwrap() > 24_000,
            "at least half a second of speech"
        );
        let mut said = None;
        while let Ok(event) = seen.try_recv() {
            if let VoiceEvent::Final { transcript } = event
                && transcript.role == VoiceRole::Assistant
            {
                said = Some(transcript.text);
            }
        }
        assert!(said.is_some_and(|text| !text.is_empty()));
    }
}
