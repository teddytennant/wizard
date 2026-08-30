//! Telegram bot gateway: long-poll `getUpdates`, dispatch each inbound text
//! (or caption / photo / image document) message to one agent turn, and reply
//! via `sendMessage`.
//!
//! The bot token comes from `~/.wizard/credentials.toml` (stored under
//! `telegram`) first, then the env var named in
//! [`GatewayConfig::token_env`](crate::config::GatewayConfig::token_env) (or
//! `WIZARD_TELEGRAM_TOKEN` by default), so a gateway launched from
//! cron/systemd works without an environment at all. (Provider API keys
//! resolve the other way round: there the env var overrides the stored key,
//! because a human is present to export one for a single run. A gateway is
//! started by an init system, so the pasted token wins.) Create a bot with
//! [@BotFather](https://t.me/BotFather) to obtain a token. Onboarding stores
//! the token via `credentials::store`.
//!
//! Errors from this module go through [`redact`] before they can reach a log:
//! the token is in every request URL.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::{Gateway, Inbound, is_authorized};
use crate::config::GatewayConfig;
use crate::platform::{paths, secrets};

/// Long-poll timeout (seconds) passed to `getUpdates`. The HTTP client's own
/// timeout is set comfortably above this.
const LONG_POLL_SECS: u64 = 30;

/// Placeholder text for a photo/document with no caption so the agent still
/// runs a turn and can open the attached file.
const PHOTO_ONLY_PROMPT: &str = "Please look at the attached image.";

/// Reply sent for message types we do not handle (stickers, voice, etc.).
const UNSUPPORTED_REPLY: &str =
    "unsupported message type — send text, a photo, or an image document";

/// How many times one outbound call is attempted before the reply is given up
/// on.
///
/// Telegram rate-limits a bot to roughly one message per second *per chat*, and
/// [`super::send_reply`] sends the chunks of a long answer back to back, so a
/// 429 on chunk two of six is the ordinary case rather than the exotic one. The
/// old code returned that 429 to the caller, which logged it and moved on: the
/// answer arrived with holes in it, or — for a single-chunk reply — did not
/// arrive at all, which from the chat is indistinguishable from a bot that has
/// stopped working. Four attempts covers a flood-wait long enough to matter and
/// still ends.
const SEND_ATTEMPTS: u32 = 4;

/// Ladder bounds for the outbound retry, in seconds. Deliberately not the
/// config's LLM ladder: a chat message that has not landed in half a minute is
/// stale, whereas a model call is worth waiting minutes for.
const SEND_RETRY_BASE_SECS: u64 = 1;
const SEND_RETRY_MAX_SECS: u64 = 8;

/// Longest total wait one outbound call may spend on retries.
///
/// The budget exists because [`super::Pump::absorb`] sends from *inside* the
/// select that keeps the running turn alive, so every second spent sleeping on
/// a 429 is a second the turn is not being polled and a `/stop` is not being
/// heard. Telegram answers a serious flood-wait with a `retry_after` of minutes
/// or hours; honouring one of those literally would wedge the gateway for that
/// long, which is precisely the failure this whole file is about. Past the
/// budget the chunk is given up on and said to be given up on.
const SEND_RETRY_BUDGET: Duration = Duration::from_secs(10);

/// A connected Telegram bot. Holds the API base URL (with the token embedded)
/// and the update offset cursor so each update is processed once.
pub struct Telegram {
    http: reqwest::Client,
    /// `https://api.telegram.org/bot<token>` — the token is kept here and
    /// never logged.
    api_base: String,
    /// `https://api.telegram.org/file/bot<token>`: the base the downloaded
    /// file itself is fetched from, which is a different host path from the
    /// method base above. Held whole rather than rebuilt from a bare copy of
    /// the token so the download half has a seam a test can point at a
    /// loopback server: without one, every test of what lands on disk would
    /// have to reach the real api.telegram.org. Never logged.
    file_base: String,
    /// Next `getUpdates` offset: one past the highest update id seen.
    offset: i64,
    /// Updates Telegram has already handed over — and so will never hand over
    /// again — that have not yet been turned into [`Inbound`]s.
    ///
    /// This buffer is the whole of the cancel-safety story, and it exists
    /// because [`Telegram::poll`] is *routinely cancelled*: the serve loop
    /// selects the poll against the running turn ([`super::Pump::run_turn`]),
    /// so the moment a turn finishes the in-flight poll future is dropped
    /// wherever it happens to be. `getUpdates` advances `offset` as soon as the
    /// batch is decoded, and conversion below it awaits — a 20 MB photo
    /// download can take seconds. A drop in that window used to mean the
    /// updates were confirmed to Telegram and then thrown away: messages the
    /// person had sent, gone, with the bot looking perfectly alive. Staging
    /// them here first, with no await between the decode and the staging, makes
    /// the loss impossible; the worst a cancellation can now cost is repeating
    /// one attachment download.
    pending: VecDeque<Update>,
    /// Messages already converted and not yet handed to the caller. Same
    /// reasoning as [`Telegram::pending`], one step further along: a
    /// cancellation after the third of four downloads must not discard the
    /// three that succeeded.
    ready: Vec<Inbound>,
    /// Directory under which downloaded attachments land
    /// (`~/.wizard/gateway-attachments` or a temp fallback).
    attachments_dir: PathBuf,
    /// `gateway.allowed_chat_ids`, copied at connect time. Held here (and not
    /// only in [`serve`](super::serve)) because the transport is where the
    /// side effects are: a message from a chat that is not on this list must
    /// cost nothing, so the check happens before any download or reply.
    allowed_chat_ids: Vec<i64>,
}

impl Telegram {
    /// Connect using the stored `telegram` credential, falling back to the
    /// env var named in `config` (the reverse of provider-key precedence, for
    /// the reason given in the module doc). A missing or empty token is an
    /// actionable error naming both sources and onboarding.
    pub fn connect(config: &GatewayConfig) -> Result<Self> {
        let env_name = config.token_env();
        let token = crate::credentials::get(crate::credentials::GATEWAY_TOKEN)
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                std::env::var(env_name)
                    .ok()
                    .filter(|t| !t.trim().is_empty())
            });
        let token = token.with_context(|| {
            format!(
                "Telegram bot token not set. Paste it during `wizard --onboard` (Telegram), \
                 store it under [keys] telegram = \"...\" in ~/.wizard/credentials.toml \
                 (mode 0600), or export {env_name}=<token> (create a bot via @BotFather)"
            )
        })?;
        let token = token.trim().to_string();

        let http = reqwest::Client::builder()
            // Allow the full long-poll window plus slack before timing out.
            .timeout(Duration::from_secs(LONG_POLL_SECS + 30))
            .build()
            // Builder construction only fails when the TLS backend cannot
            // initialize; fall back to the default client rather than panic.
            .unwrap_or_default();

        let attachments_dir = attachments_dir();
        if let Err(err) = secrets::create_private_dir(&attachments_dir) {
            tracing::warn!(
                "could not create attachments dir {}: {err:#}",
                attachments_dir.display()
            );
        }

        Ok(Self {
            http,
            api_base: format!("https://api.telegram.org/bot{token}"),
            file_base: format!("https://api.telegram.org/file/bot{token}"),
            offset: 0,
            pending: VecDeque::new(),
            ready: Vec::new(),
            attachments_dir,
            allowed_chat_ids: config.allowed_chat_ids.clone(),
        })
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/{method}", self.api_base)
    }

    /// One outbound Bot API call, retried while Telegram says it is worth
    /// retrying and the budget holds.
    ///
    /// Every outbound method goes through here so there is exactly one place
    /// that knows what a 429 means. Before it existed, `sendMessage` mapped
    /// straight through `error_for_status`, and a rate limit — the single most
    /// likely failure a chat bot meets, because Telegram allows about one
    /// message per second per chat and [`super::send_reply`] sends the chunks
    /// of a long answer back to back — came out as an ordinary error that the
    /// caller logged and dropped. The reader saw an answer with a hole in it,
    /// or, for a one-chunk reply, nothing at all.
    ///
    /// A permanent refusal is *not* retried and is reported as such
    /// ([`CallFailure::Refused`]): a 400 from a bad `parse_mode` will be a 400
    /// forever, and the caller that can degrade to plain text needs to be told
    /// which kind of failure it had rather than watching four identical
    /// rejections go by.
    async fn call(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> std::result::Result<(), CallFailure> {
        self.call_within(method, body, SEND_ATTEMPTS).await
    }

    /// [`Telegram::call`] with an explicit attempt budget, for the one caller
    /// that must not spend any: the typing indicator is awaited by the serve
    /// loop *before* the turn it announces, so a rate-limited `sendChatAction`
    /// retried for ten seconds would delay the actual work by ten seconds
    /// in order to say that work was starting.
    async fn call_within(
        &self,
        method: &str,
        body: &serde_json::Value,
        attempts: u32,
    ) -> std::result::Result<(), CallFailure> {
        let url = self.method_url(method);
        let started = std::time::Instant::now();
        let mut last = String::new();
        for attempt in 0..attempts.max(1) {
            let response = self.http.post(&url).json(body).send().await.map_err(redact);
            let outcome = match response {
                Ok(response) => {
                    let status = response.status().as_u16();
                    // The body carries `parameters.retry_after`, which is the
                    // only number in a 429 worth having. Unreadable bodies
                    // classify on the status alone.
                    let text = response.text().await.map_err(redact).unwrap_or_default();
                    classify_call(status, &text, attempt)
                }
                // A dropped connection or a DNS blip mid-send is exactly the
                // sort of thing a second attempt fixes, and never a reason to
                // decide the reply was undeliverable.
                Err(err) => Attempt::Again {
                    wait: crate::llm::retry_delay(
                        attempt,
                        SEND_RETRY_BASE_SECS,
                        SEND_RETRY_MAX_SECS,
                        None,
                    ),
                    why: format!("{:#}", anyhow::Error::new(err)),
                },
            };
            match outcome {
                Attempt::Accepted => return Ok(()),
                Attempt::Refused {
                    status,
                    description,
                } => {
                    return Err(CallFailure::Refused {
                        method: method.to_string(),
                        status,
                        description,
                    });
                }
                Attempt::Again { wait, why } => {
                    last = why;
                    // The budget, not just the attempt count: Telegram answers
                    // a serious flood-wait with a `retry_after` of minutes, and
                    // sleeping that out here would hold up the select that
                    // keeps the running turn alive and `/stop` audible.
                    let spent = started.elapsed();
                    if attempt + 1 >= attempts.max(1) || spent + wait > SEND_RETRY_BUDGET {
                        break;
                    }
                    tracing::debug!(
                        "Telegram {method} needs another go in {:.1}s ({last})",
                        wait.as_secs_f64()
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
        Err(CallFailure::Transient {
            method: method.to_string(),
            why: last,
        })
    }

    /// `getMe`: prove the token authenticates and name the bot it belongs to.
    ///
    /// The point of calling this at all is *when* it fails. Without it a typo
    /// in a pasted token surfaces at the first `getUpdates` of a running
    /// gateway — as a retried poll error in a journal — instead of at the
    /// moment the token was typed, which is the only moment the person can fix
    /// it. `wizard gateway setup` calls it before it offers to change
    /// anything.
    pub async fn get_me(&self) -> Result<BotIdentity> {
        let url = self.method_url("getMe");
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(redact)
            .context("requesting Telegram getMe")?;
        // 401 is *the* expected failure here (a mistyped or revoked token), and
        // reqwest's own rendering of it says only "HTTP status client error".
        if response.status().as_u16() == 401 {
            anyhow::bail!(
                "Telegram rejected this bot token (401 Unauthorized). Copy the whole \
                 token @BotFather gave you — it looks like `123456789:AA…` — and try again"
            );
        }
        let response = response
            .error_for_status()
            .map_err(redact)
            .context("Telegram getMe returned an error status")?;
        let body: GetMe = response
            .json()
            .await
            .map_err(redact)
            .context("decoding the Telegram getMe response")?;
        let user = body
            .result
            .filter(|_| body.ok)
            .context("Telegram getMe returned ok=false")?;
        Ok(BotIdentity::from_user(&user))
    }

    /// One `getUpdates` request, advancing the cursor past everything it
    /// returns. Shared by [`Telegram::poll`] and the discovery calls below so
    /// there is one place the token-bearing URL is built, one set of redaction
    /// boundaries, and one cursor.
    async fn get_updates(&mut self, timeout_secs: u64) -> Result<Vec<Update>> {
        let url = self.method_url("getUpdates");
        let response = self
            .http
            .get(&url)
            .query(&[
                ("timeout", timeout_secs.to_string()),
                ("offset", self.offset.to_string()),
                // Only message updates; drops callback_query / channel_post noise.
                ("allowed_updates", r#"["message"]"#.to_string()),
            ])
            .send()
            .await
            .map_err(redact)
            .context("requesting Telegram updates")?;
        // Telegram allows exactly one long-poller per bot and answers 409 to
        // the second. Named, because the situation it describes is ordinary —
        // running `wizard gateway setup` while the service is up — and
        // "HTTP status client error (409 Conflict)" names nothing.
        if response.status().as_u16() == 409 {
            anyhow::bail!(
                "another process is already long-polling this bot (Telegram 409 Conflict). \
                 Stop the running gateway first — `wizard gateway stop`, or Ctrl-C the \
                 `wizard --gateway` in the other terminal"
            );
        }
        let response = response
            .error_for_status()
            .map_err(redact)
            .context("Telegram getUpdates returned an error status")?;
        let body: GetUpdates = response
            .json()
            .await
            .map_err(redact)
            .context("decoding Telegram getUpdates response")?;
        if !body.ok {
            anyhow::bail!("Telegram getUpdates returned ok=false");
        }
        // From here to the `Ok` there is no `.await`, and there must never be
        // one: the moment the cursor moves past an update, Telegram will not
        // hand it over again, so anything that could drop this future between
        // the advance and the caller taking the batch is a message the operator
        // sent and nobody will ever see. `Telegram::poll` closes the other half
        // of the same window by staging the batch before it converts anything.
        for update in &body.result {
            // Advance the cursor for every update, even ones the caller
            // ignores, so they are not redelivered on the next poll.
            if update.update_id >= self.offset {
                self.offset = update.update_id + 1;
            }
        }
        Ok(body.result)
    }

    /// Consume and discard everything already queued for this bot, returning
    /// how many updates were dropped.
    ///
    /// [`Telegram::next_chat`] would otherwise answer with whatever is oldest
    /// in the backlog, and the backlog of a fresh bot is not reliably the
    /// operator's own message: a bot username is public, and anyone who
    /// messaged it while it was unattended is sitting in that queue. Offering
    /// to allow-list a stranger's chat id — with the id presented as "the
    /// message that just arrived" — is the one way this flow could hand the
    /// machine to somebody. So discovery starts from empty and reports only a
    /// message that arrives after the operator has been told to send one.
    pub async fn drain_pending(&mut self) -> Result<usize> {
        let mut dropped = 0;
        // `timeout=0` returns whatever is queued immediately, up to 100
        // updates. The bound stops a bot with an enormous backlog from
        // spinning here forever; a few thousand stale updates is already far
        // past "somebody messaged me once".
        for _ in 0..20 {
            let batch = self.get_updates(0).await?;
            if batch.is_empty() {
                return Ok(dropped);
            }
            dropped += batch.len();
        }
        Ok(dropped)
    }

    /// Wait up to `wait` for the next message from *any* chat and report where
    /// it came from. `Ok(None)` is the timeout.
    ///
    /// Deliberately not filtered through the allow-list: the entire point is
    /// that the chat is not on it yet. This never runs a turn, never downloads
    /// an attachment and never replies — it reads an envelope and stops — so
    /// the fail-closed rule in [`super::is_authorized`] is untouched. What the
    /// caller does with the id is a separate, consented step.
    pub async fn next_chat(&mut self, wait: Duration) -> Result<Option<ChatSighting>> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            // Never ask Telegram to hold the connection past our own deadline.
            let secs = remaining.as_secs().clamp(1, LONG_POLL_SECS);
            for update in self.get_updates(secs).await? {
                if let Some(message) = update.message {
                    return Ok(Some(ChatSighting::from_message(&message)));
                }
            }
        }
    }

    /// Download a Telegram file by `file_id` into `attachments_dir` and return
    /// the local path. Uses `getFile` then fetches
    /// `https://api.telegram.org/file/bot<token>/<file_path>`.
    async fn download_file(&self, file_id: &str) -> Result<PathBuf> {
        let url = self.method_url("getFile");
        let response = self
            .http
            .get(&url)
            .query(&[("file_id", file_id)])
            .send()
            .await
            .map_err(redact)
            .context("requesting Telegram getFile")?
            .error_for_status()
            .map_err(redact)
            .context("Telegram getFile returned an error status")?;
        let body: GetFile = response
            .json()
            .await
            .map_err(redact)
            .context("decoding Telegram getFile response")?;
        if !body.ok {
            anyhow::bail!("Telegram getFile returned ok=false");
        }
        let file_path = body
            .result
            .and_then(|r| r.file_path)
            .context("Telegram getFile response missing file_path")?;

        let download_url = format!("{}/{file_path}", self.file_base);
        let bytes = self
            .http
            .get(&download_url)
            .send()
            .await
            .map_err(redact)
            .context("downloading Telegram file")?
            .error_for_status()
            .map_err(redact)
            .context("Telegram file download returned an error status")?
            .bytes()
            .await
            .map_err(redact)
            .context("reading Telegram file body")?;

        secrets::create_private_dir(&self.attachments_dir)
            .context("creating the gateway attachments directory")?;

        let local = self
            .attachments_dir
            .join(attachment_file_name(&file_path, file_id));
        write_attachment(&local, &bytes).context("writing the downloaded attachment")?;
        Ok(local)
    }

    /// Convert a Telegram message into zero or one [`Inbound`]. Unsupported
    /// types yield a short rejection reply (best-effort) and no inbound.
    ///
    /// The allow-list is checked first, before anything else in this function
    /// runs. Everything below it costs the operator something an unauthorized
    /// sender must not be able to spend: `download_file` writes
    /// attacker-controlled bytes into `~/.wizard/gateway-attachments` (nothing
    /// ever deletes them, so a stranger sending 20 MB photos in a loop fills
    /// the disk), and the unsupported-type reply turns any sticker into an
    /// outbound API call that confirms the bot is live. Refusing at the top
    /// is what makes an empty `allowed_chat_ids` actually refuse the work
    /// rather than only the agent turn.
    async fn message_to_inbound(&self, message: &Message) -> Option<Inbound> {
        let chat_id = message.chat.id;
        if !is_authorized(chat_id, &self.allowed_chat_ids) {
            // The id still goes up so `serve` can log it and send the one
            // vague refusal; see `Inbound::refused`.
            return Some(Inbound::refused(chat_id));
        }
        let caption = message.caption.clone().filter(|c| !c.trim().is_empty());

        let Some((text, fetch)) = classify_message(message) else {
            // Stickers, voice, video notes, etc.: acknowledge rather than silence.
            tracing::info!("unsupported Telegram message type from chat {chat_id}");
            if let Err(err) = self.send(chat_id, UNSUPPORTED_REPLY).await {
                tracing::warn!("failed to send unsupported-type reply: {err:#}");
            }
            return None;
        };
        let Some(file_id) = fetch else {
            return Some(Inbound {
                chat_id,
                text,
                attachments: Vec::new(),
            });
        };
        match self.download_file(&file_id).await {
            Ok(path) => Some(Inbound {
                chat_id,
                text,
                attachments: vec![path],
            }),
            Err(err) => {
                tracing::warn!("failed to download Telegram attachment: {err:#}");
                // Still deliver caption-only so the agent can respond.
                caption.map(|text| Inbound {
                    chat_id,
                    text,
                    attachments: Vec::new(),
                })
            }
        }
    }

    /// Turn everything on [`Telegram::pending`] into [`Inbound`]s, moving each
    /// update off the queue only once its message is safely on
    /// [`Telegram::ready`].
    ///
    /// The order matters more than it looks. Conversion is where this
    /// transport's `.await`s live — a `getFile`, a multi-megabyte download, the
    /// unsupported-type reply — and the whole future is dropped whenever the
    /// turn it is racing finishes. Popping the update first and converting
    /// second would lose the message on every such drop. Popping second means
    /// the worst case is repeating one download and, for an unsupported
    /// message type, sending the "unsupported message type" line twice: a
    /// duplicate is a nuisance, a silently swallowed message is the bug this
    /// whole module is about.
    async fn convert_pending(&mut self) {
        while let Some(update) = self.pending.front().cloned() {
            let converted = match &update.message {
                Some(message) => self.message_to_inbound(message).await,
                // An update with no message (an edit, a reaction — Telegram
                // sends these despite `allowed_updates`) is nothing to deliver
                // and everything to forget.
                None => None,
            };
            // Synchronous from here to the top of the loop: nothing can be
            // cancelled between forgetting the update and keeping its message.
            self.pending.pop_front();
            self.ready.extend(converted);
        }
    }
}

/// Pure classification of a message: the text to deliver plus the Telegram
/// file id to download (if any). `None` means nothing routable (sticker,
/// voice, ...).
fn classify_message(message: &Message) -> Option<(String, Option<String>)> {
    let caption = message.caption.clone().filter(|c| !c.trim().is_empty());
    let text = message.text.clone().filter(|t| !t.trim().is_empty());

    // Pure text.
    if let Some(text) = text {
        return Some((text, None));
    }

    // Photo: Telegram sends several sizes; take the largest (last).
    if let Some(photos) = message.photo.as_ref().filter(|p| !p.is_empty()) {
        let largest = &photos[photos.len() - 1];
        let text = caption.unwrap_or_else(|| PHOTO_ONLY_PROMPT.to_string());
        return Some((text, Some(largest.file_id.clone())));
    }

    // Image document (or any document with a caption / image mime).
    if let Some(doc) = message.document.as_ref() {
        let is_image = doc
            .mime_type
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
            || doc.file_name.as_deref().is_some_and(is_image_filename);
        if is_image || caption.is_some() {
            let text = caption.unwrap_or_else(|| {
                if is_image {
                    PHOTO_ONLY_PROMPT.to_string()
                } else {
                    format!(
                        "Please look at the attached file ({}).",
                        doc.file_name.as_deref().unwrap_or("document")
                    )
                }
            });
            return Some((text, Some(doc.file_id.clone())));
        }
    }

    // Caption-only (no media we recognized) — still useful.
    caption.map(|text| (text, None))
}

#[async_trait]
impl Gateway for Telegram {
    fn label(&self) -> &str {
        "telegram"
    }

    /// One long poll, or whatever a previous cancelled poll left owing.
    ///
    /// Telegram is only asked for more when this transport owes the caller
    /// nothing, so a poll that was dropped mid-conversion (which happens every
    /// time a turn finishes while one is in flight — see
    /// [`Telegram::pending`]) resumes exactly where it stopped instead of
    /// blocking for another long-poll window while messages the operator
    /// already sent sit unhandled in memory.
    async fn poll(&mut self) -> Result<Vec<Inbound>> {
        if self.pending.is_empty() && self.ready.is_empty() {
            let updates = self.get_updates(LONG_POLL_SECS).await?;
            // No await between `get_updates` returning and the batch being
            // staged, which is what makes the cursor advance inside it safe.
            self.pending.extend(updates);
        }
        self.convert_pending().await;
        Ok(std::mem::take(&mut self.ready))
    }

    async fn send(&self, chat_id: i64, text: &str) -> Result<()> {
        self.call(
            "sendMessage",
            &serde_json::json!({ "chat_id": chat_id, "text": text }),
        )
        .await
        .map_err(anyhow::Error::new)
        .context("sending Telegram message")
    }

    /// HTML, falling back to plain text if Telegram will not parse it.
    ///
    /// The fallback is the whole design. A malformed conversion is not a
    /// cosmetic bug here — `sendMessage` answers 400 and the reply simply never
    /// arrives, which from the chat is indistinguishable from the agent having
    /// nothing to say. So the formatted attempt is the optimistic path and the
    /// literal text is the guaranteed one, and a conversion bug costs
    /// formatting rather than the answer.
    async fn send_rich(&self, chat_id: i64, text: &str) -> Result<()> {
        let html = super::format::to_telegram_html(text);
        let formatted = self
            .call(
                "sendMessage",
                &serde_json::json!({
                    "chat_id": chat_id,
                    "text": html,
                    "parse_mode": "HTML",
                    // A reply is prose about code, not a link preview: the first
                    // URL in it should not expand into a card that buries the text.
                    "link_preview_options": { "is_disabled": true },
                }),
            )
            .await;
        match formatted {
            Ok(()) => Ok(()),
            // A refusal is the markup's fault far more often than the words':
            // a stray `<` the converter did not escape, an entity Telegram
            // dislikes. The same text without a `parse_mode` cannot be refused
            // for that reason, so try it before giving up on the message.
            // Rate limits and outages are *not* retried here: `call` already
            // spent its budget on them, and repeating the whole message
            // unformatted would only spend it twice.
            Err(CallFailure::Refused {
                status,
                description,
                ..
            }) => {
                tracing::debug!(
                    "Telegram refused the formatted message ({status}: {description}); \
                     resending as plain text"
                );
                self.send(chat_id, text).await.with_context(|| {
                    format!("after Telegram refused the formatted message ({status})")
                })
            }
            Err(failure) => Err(anyhow::Error::new(failure)),
        }
    }

    /// `setMyCommands`, so typing `/` in any allowed chat offers the list.
    async fn advertise_commands(&self, commands: &[super::AdvertisedCommand]) -> Result<()> {
        let payload: Vec<serde_json::Value> = commands
            .iter()
            .map(|command| {
                serde_json::json!({
                    "command": command.name,
                    "description": command.description,
                })
            })
            .collect();
        self.call("setMyCommands", &serde_json::json!({ "commands": payload }))
            .await
            .map_err(anyhow::Error::new)
            .context("publishing the Telegram command list")
    }

    /// The typing indicator, and exactly one attempt at it.
    ///
    /// Cosmetic by definition, and awaited before the turn it announces, so it
    /// gets no retries at all: the alternative is a rate-limited
    /// `sendChatAction` making the reader wait longer for the answer in order
    /// to be told the answer is coming.
    async fn typing(&self, chat_id: i64) -> Result<()> {
        self.call_within(
            "sendChatAction",
            &serde_json::json!({ "chat_id": chat_id, "action": "typing" }),
            1,
        )
        .await
        .map_err(anyhow::Error::new)
        .context("sending Telegram chat action")
    }
}

/// What one attempt at an outbound Bot API call came back as.
#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    /// Telegram took it.
    Accepted,
    /// Telegram refused this exact payload and will refuse the identical one
    /// again: a `parse_mode` it could not parse, a chat that has blocked the
    /// bot, a chat id that does not exist. Retrying is pure delay; the only
    /// useful response is to send something *different* (plain text instead of
    /// HTML) or to stop.
    Refused { status: u16, description: String },
    /// Worth another go once `wait` has passed — a 429 with the deadline
    /// Telegram itself stated, or a 5xx while it is having an incident.
    Again { wait: Duration, why: String },
}

/// Why an outbound call did not land, in the two shapes a caller acts on
/// differently.
///
/// Split because [`Telegram::send_rich`] can *do* something about a refusal —
/// resend the same words without the markup — and can do nothing at all about a
/// rate limit that outlasted its budget except say so. Collapsing both into one
/// anyhow error is how the HTML fallback used to be reached for a 400 and never
/// for anything else, with no way to tell the difference at the call site.
#[derive(Debug)]
enum CallFailure {
    /// Telegram refused the payload itself. See [`Attempt::Refused`].
    Refused {
        method: String,
        status: u16,
        description: String,
    },
    /// A fault, or a rate limit the retry budget could not outlast.
    Transient { method: String, why: String },
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallFailure::Refused {
                method,
                status,
                description,
            } => match description.is_empty() {
                true => write!(f, "Telegram {method} refused the request ({status})"),
                false => write!(
                    f,
                    "Telegram {method} refused the request ({status}: {description})"
                ),
            },
            CallFailure::Transient { method, why } => {
                write!(f, "Telegram {method} did not go through: {why}")
            }
        }
    }
}

impl std::error::Error for CallFailure {}

/// Decide what to do about one Bot API response, from its status and body
/// alone.
///
/// Pure, and separate from [`Telegram::call`], because this is the rule that
/// decides whether a reply is delivered or quietly lost and there is no way to
/// exercise it against the real API: reproducing a 429 means actually flooding
/// Telegram. The interesting cases are all here — the `retry_after` inside the
/// body, a 429 with no body at all, a 5xx that is worth waiting out, and the
/// 400 that must *not* be retried because the whole reply depends on the caller
/// noticing and degrading instead.
fn classify_call(status: u16, body: &str, attempt: u32) -> Attempt {
    if (200..300).contains(&status) {
        return Attempt::Accepted;
    }
    // Capped at the budget rather than merely aimed below it. The shared ladder
    // adds jitter *on top of* a stated deadline, so a `retry_after` that was
    // already clamped to the budget comes back a little over it, and one sleep
    // longer than the whole budget is the thing this must not be able to
    // produce: it happens inside the select that keeps the running turn alive.
    let ladder = |stated| {
        crate::llm::retry_delay(attempt, SEND_RETRY_BASE_SECS, SEND_RETRY_MAX_SECS, stated)
            .min(SEND_RETRY_BUDGET)
    };
    let described = api_error(body);
    match status {
        429 => Attempt::Again {
            wait: ladder(stated_retry_after(body)),
            why: match described.is_empty() {
                true => "429 Too Many Requests".to_string(),
                false => format!("429 Too Many Requests ({described})"),
            },
        },
        // Telegram's own outages, and the 502s its edge emits under load. The
        // same message will very likely go through in a moment.
        500..=599 => Attempt::Again {
            wait: ladder(None),
            why: format!("{status} from Telegram"),
        },
        _ => Attempt::Refused {
            status,
            description: described,
        },
    }
}

/// The `retry_after` Telegram states inside a 429 body
/// (`{"ok":false,"error_code":429,"parameters":{"retry_after":7}}`).
///
/// Read from the body rather than from a header because that is where the Bot
/// API puts it — there is no `Retry-After` header on a Bot API 429 — and
/// guessing a ladder value when the server named an exact one is how a bot gets
/// its rate limit extended instead of served.
fn stated_retry_after(body: &str) -> Option<Duration> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let seconds = parsed.get("parameters")?.get("retry_after")?.as_u64()?;
    // Clamped for the same reason `crate::llm` clamps its own: a hostile or
    // confused value must not be able to park an outbound call for a day.
    Some(Duration::from_secs(seconds).min(SEND_RETRY_BUDGET))
}

/// Telegram's own `description` for a failed call, through [`one_line`] so a
/// server-supplied string cannot carry control characters into a log.
/// Empty when the body is not the JSON envelope the Bot API documents.
fn api_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|parsed| {
            parsed
                .get("description")
                .and_then(|value| value.as_str())
                .map(one_line)
        })
        .unwrap_or_default()
}

/// Strip the request URL out of a reqwest error before it can reach a log, a
/// console line, or a reply.
///
/// Every Telegram URL embeds the bot token (`.../bot<token>/getUpdates`), and
/// `reqwest::Error`'s `Display` appends ` for url (<url>)`. The gateway loop
/// prints poll failures with `{err:#}`, so without this a single DNS blip or
/// 502 would write a working bot token into the journal, where it long
/// outlives the incident. Applied at *every* reqwest boundary, not only the
/// ones that attach a URL in the current version: which constructors carry a
/// URL is reqwest's implementation detail, and this is the cheapest possible
/// insurance against it changing.
fn redact(err: reqwest::Error) -> reqwest::Error {
    err.without_url()
}

/// Directory for downloaded gateway attachments. Prefers
/// `~/.wizard/gateway-attachments`; falls back to the shared system temp dir.
///
/// The fallback is the reason both this directory and the files inside it are
/// created through [`crate::platform::secrets`] rather than by hand: when
/// there is no state tree at all (a systemd user unit with `ProtectHome=yes`
/// and no `WIZARD_HOME` — which `docs/gateway.md` names as a host where no
/// state directory resolves, not as a deployment it recommends — running on a
/// token from the environment) every photo an operator sends the bot lands in
/// a directory shared with every local account, under a name that
/// is predictable from the clock. `create_private_dir` asks for 0700 at
/// creation time rather than chmod'ing afterwards, so there is no window in
/// which the directory sits at the process umask with a download already
/// arriving in it, and [`write_attachment`] is `O_EXCL`, so a name somebody
/// else got to first fails the download instead of redirecting it.
///
/// A failed chmod is a warning rather than an error, the policy
/// `platform::secrets` documents for the state tree: `WIZARD_HOME` on
/// exFAT/FAT32, CIFS/NFS without POSIX modes, or WSL DrvFs cannot express the
/// mode at all, and refusing to run there would trade a working gateway for a
/// bit those filesystems will never carry.
fn attachments_dir() -> PathBuf {
    paths::state_dir()
        .map(|dir| dir.join("gateway-attachments"))
        .unwrap_or_else(|_| paths::temp_dir().join("wizard-gateway-attachments"))
}

/// Local file name for one downloaded attachment: a unique prefix plus the
/// sanitized remote basename (or the file id, when the remote path has no
/// usable one).
///
/// The prefix has to make the name *unique*, not merely unlikely to repeat.
/// [`write_attachment`] is `O_EXCL`, so a name that is already taken fails the
/// download rather than overwriting, and a millisecond on its own is not
/// unique: Telegram delivers an album as several photos in one `getUpdates`
/// batch, all with the same `file_N.jpg` shape. The pid separates two gateways
/// sharing one directory, the counter separates two downloads inside one
/// process, and the millisecond keeps the directory in `ls` order for a human
/// cleaning it out (nothing else ever deletes from it).
fn attachment_file_name(remote_path: &str, file_id: &str) -> String {
    /// Process-wide download counter; see the note on uniqueness above.
    static SEQ: AtomicU64 = AtomicU64::new(0);

    // Prefer the remote basename; fall back to a unique id-based name.
    let name = Path::new(remote_path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .map(|n| n.to_string())
        .unwrap_or_else(|| format!("tg-{file_id}"));
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "{millis}-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        sanitize_filename(&name)
    )
}

/// Write one downloaded attachment into a freshly created owner-only file.
///
/// [`crate::platform::secrets::create_private_file`] is `O_EXCL`, and that is
/// the point rather than an implementation detail. On the temp-dir fallback
/// described on [`attachments_dir`] the destination directory is shared with
/// every local account and the name is predictable, so `.create(true)
/// .truncate(true)` was an arbitrary-file-overwrite primitive: another user
/// pre-plants the name as a symlink to `~/.bashrc` (or
/// `~/.wizard/config.toml`) and the operator's own next photo truncates the
/// target and refills it with attacker-chosen bytes, running as the operator.
/// Refusing a name that already exists turns that into a failed download.
///
/// It closes the disclosure half too: a mode passed to `open` applies only to
/// a file being *created*, so a 0666 file planted at the same name used to be
/// written into and left world-readable, which is exactly the exposure the
/// 0600 was added for.
fn write_attachment(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let mut file = secrets::create_private_file(path)?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))
}

/// Keep only a safe basename so a hostile `file_path` cannot escape the
/// attachments directory.
fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

fn is_image_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".bmp")
}

/// Top-level `getUpdates` response.
#[derive(Debug, Deserialize)]
struct GetUpdates {
    ok: bool,
    #[serde(default)]
    result: Vec<Update>,
}

/// One update in a `getUpdates` batch.
#[derive(Debug, Clone, Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
}

/// A Telegram message (only the fields Wizard uses).
#[derive(Debug, Clone, Deserialize)]
struct Message {
    chat: Chat,
    /// Who sent it. Read by [`ChatSighting`] so `wizard gateway setup` can say
    /// *whose* message it just saw, and by nothing else — in particular not by
    /// [`super::is_authorized`], which authorises a chat id and consults no
    /// other field. Telegram omits it for channel posts.
    #[serde(default)]
    from: Option<User>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    photo: Option<Vec<PhotoSize>>,
    #[serde(default)]
    document: Option<Document>,
}

/// A Telegram user, for display only (see [`Message::from`]).
#[derive(Debug, Clone, Deserialize)]
struct User {
    #[serde(default)]
    first_name: Option<String>,
    #[serde(default)]
    last_name: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

/// `getMe` response.
#[derive(Debug, Deserialize)]
struct GetMe {
    ok: bool,
    #[serde(default)]
    result: Option<User>,
}

/// Who the bot token belongs to, as `getMe` reports it. Both fields are
/// already through [`one_line`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotIdentity {
    /// The `@name` people message, without the `@`. Empty if Telegram somehow
    /// returns a bot with no username (it does not, but the field is optional
    /// in the schema).
    pub username: String,
    /// The bot's display name.
    pub name: String,
}

impl BotIdentity {
    fn from_user(user: &User) -> Self {
        Self {
            username: one_line(user.username.as_deref().unwrap_or_default()),
            name: one_line(user.first_name.as_deref().unwrap_or_default()),
        }
    }

    /// `@wizard_bot (Wizard)`, or just one half when the other is missing.
    pub fn label(&self) -> String {
        match (self.username.is_empty(), self.name.is_empty()) {
            (false, false) => format!("@{} ({})", self.username, self.name),
            (false, true) => format!("@{}", self.username),
            (true, false) => self.name.clone(),
            (true, true) => "this bot".to_string(),
        }
    }
}

/// Where one observed message came from: everything `wizard gateway setup`
/// needs to tell an operator which chat it is about to allow. Free text is
/// already through [`one_line`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatSighting {
    /// The id that would go into `gateway.allowed_chat_ids`.
    pub chat_id: i64,
    /// Telegram's `chat.type`: `private`, `group`, `supergroup`, `channel`.
    /// Empty when it was not sent.
    pub kind: String,
    /// The sender's name, when the message carried one.
    pub from: Option<String>,
}

impl ChatSighting {
    fn from_message(message: &Message) -> Self {
        Self {
            chat_id: message.chat.id,
            kind: one_line(message.chat.kind.as_deref().unwrap_or_default()),
            from: message.from.as_ref().and_then(sender_name),
        }
    }
}

/// A sender's display name: "First Last (@handle)", falling back to whichever
/// half exists. `None` when Telegram sent neither.
fn sender_name(user: &User) -> Option<String> {
    let full = [user.first_name.as_deref(), user.last_name.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let full = one_line(&full);
    let handle = one_line(user.username.as_deref().unwrap_or_default());
    match (full.is_empty(), handle.is_empty()) {
        (true, true) => None,
        (true, false) => Some(format!("@{handle}")),
        (false, true) => Some(full),
        (false, false) => Some(format!("{full} (@{handle})")),
    }
}

/// Longest display string kept from a Telegram-supplied name.
const DISPLAY_CHARS: usize = 64;

/// Reduce a name chosen by whoever is on the other end to something that can
/// be handed to a terminal.
///
/// Everything this module reads back is attacker-chosen: a stranger can set
/// their Telegram first name to an ESC sequence and message the bot while
/// `wizard gateway setup` is waiting. That output line sits directly above
/// "Add chat … ? [y/N]", so unescaped bytes there could repaint the question
/// being answered. Control characters become a space, runs of whitespace
/// collapse, and the result is clipped. Same defence, and the same reasoning,
/// as the registry listing in [`crate::registry_client`].
fn one_line(text: &str) -> String {
    let mut out = String::new();
    let mut kept = 0usize;
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_control() || ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if kept == DISPLAY_CHARS {
            out.push('…');
            break;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
        kept += 1;
    }
    out
}

/// One size of a photo array (Telegram sends several; we take the last).
#[derive(Debug, Clone, Deserialize)]
struct PhotoSize {
    file_id: String,
}

/// A document attachment.
#[derive(Debug, Clone, Deserialize)]
struct Document {
    file_id: String,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

/// The chat a message belongs to.
#[derive(Debug, Clone, Deserialize)]
struct Chat {
    id: i64,
    /// `private`, `group`, `supergroup` or `channel`. Reported by
    /// [`ChatSighting`] so setup can say what it is about to allow; never part
    /// of the authorization decision, which is the id and only the id.
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

/// `getFile` response.
#[derive(Debug, Deserialize)]
struct GetFile {
    ok: bool,
    #[serde(default)]
    result: Option<FileInfo>,
}

#[derive(Debug, Deserialize)]
struct FileInfo {
    #[serde(default)]
    file_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A stand-in for the Telegram API that counts every request it is sent
    /// and answers each one with `{"ok":false}`. Returns the base URL and the
    /// counter. Nothing leaves the machine: `ok=false` makes `download_file`
    /// bail before it would reach the real file-download host, and the count
    /// is the actual subject of the assertions.
    async fn counting_api() -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binding a loopback port");
        let addr = listener.local_addr().expect("local addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                // Counted on accept, so the count is settled by the time the
                // client sees its response: no polling or sleeps in the test.
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    const BODY: &str = r#"{"ok":false}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{BODY}",
                        BODY.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    /// A stand-in for the Telegram API that actually serves a file: `getFile`
    /// answers with `remote_path`, and the download base under `/file/bot…`
    /// answers with `bytes`. Returns the base URL.
    ///
    /// Both halves are on the one loopback listener, which is why [`Telegram`]
    /// holds `file_base` whole instead of rebuilding the download URL from a
    /// bare token: the file body is fetched from a different host path, and
    /// with the URL hard-coded no test could see what reaches the disk without
    /// reaching the real api.telegram.org.
    async fn file_serving_api(remote_path: &'static str, bytes: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binding a loopback port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..read]).into_owned();
                    let response = if request.contains("/getFile") {
                        let body =
                            format!(r#"{{"ok":true,"result":{{"file_path":"{remote_path}"}}}}"#);
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .into_bytes()
                    } else {
                        let mut head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n",
                            bytes.len()
                        )
                        .into_bytes();
                        head.extend_from_slice(bytes);
                        head
                    };
                    let _ = socket.write_all(&response).await;
                    let _ = socket.flush().await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn sticker_from(chat_id: i64) -> Message {
        // No text, no caption, no photo, no document: a sticker or voice
        // note, i.e. the "unsupported message type" path.
        Message {
            chat: Chat {
                id: chat_id,
                kind: Some("private".to_string()),
            },
            from: None,
            text: None,
            caption: None,
            photo: None,
            document: None,
        }
    }

    fn photo_from(chat_id: i64) -> Message {
        Message {
            chat: Chat {
                id: chat_id,
                kind: Some("private".to_string()),
            },
            from: None,
            text: None,
            caption: Some("look at this".to_string()),
            photo: Some(vec![PhotoSize {
                file_id: "big-photo".to_string(),
            }]),
            document: None,
        }
    }

    /// Adversarial: the allow-list is only fail-closed if it refuses *before*
    /// the transport spends anything on the message. This used to check the id
    /// in `serve`, by which point `getFile` had run, 20 MB of attacker-chosen
    /// bytes were sitting in `~/.wizard/gateway-attachments` (nothing ever
    /// deletes them), and every sticker had already drawn an outbound
    /// `sendMessage` that confirms the bot is live.
    #[tokio::test]
    async fn an_unauthorized_chat_causes_no_download_and_no_transport_reply() {
        const TOKEN: &str = "7654321:AA-not-a-real-bot-token";
        let attachments = tempfile::tempdir().expect("tempdir");
        let (api, hits) = counting_api().await;
        let telegram = Telegram {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("building a client"),
            api_base: format!("{api}/bot{TOKEN}"),
            file_base: format!("{api}/file/bot{TOKEN}"),
            offset: 0,
            pending: VecDeque::new(),
            ready: Vec::new(),
            attachments_dir: attachments.path().to_path_buf(),
            allowed_chat_ids: vec![42],
        };

        // A stranger's photo: refused with the id intact (so `serve` can log
        // it) and nothing else.
        let inbound = telegram
            .message_to_inbound(&photo_from(999))
            .await
            .expect("the chat id is still reported upwards");
        assert_eq!(inbound, Inbound::refused(999));
        assert!(inbound.attachments.is_empty(), "{inbound:?}");
        assert!(
            inbound.text.is_empty(),
            "an unauthorized sender's text must not reach the agent: {inbound:?}"
        );

        // A stranger's sticker: no "unsupported message type" reply either.
        assert_eq!(
            telegram.message_to_inbound(&sticker_from(999)).await,
            Some(Inbound::refused(999))
        );

        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "an unauthorized chat must not drive a single Telegram request"
        );
        assert_eq!(
            std::fs::read_dir(attachments.path())
                .expect("reading the attachments dir")
                .count(),
            0,
            "nothing may be written to disk for an unauthorized chat"
        );

        // Controls, so the two assertions above are measuring something: the
        // same messages from the allowed chat do reach the API.
        assert!(
            telegram
                .message_to_inbound(&sticker_from(42))
                .await
                .is_none(),
            "an allowed sticker is answered, not routed"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the unsupported-type reply");
        let inbound = telegram
            .message_to_inbound(&photo_from(42))
            .await
            .expect("an allowed photo still becomes an inbound");
        assert_eq!(inbound.text, "look at this");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "the getFile call");
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .unwrap_or_else(|err| panic!("stat {}: {err}", path.display()))
            .permissions()
            .mode()
            & 0o777
    }

    /// The production download path, end to end against a loopback API. The
    /// attachments directory does not exist when the download starts, which is
    /// what makes this the test that pins the wiring: `download_file` has to
    /// create the tree through `platform::secrets` (0700, asked for at
    /// creation time rather than chmod'd afterwards, so there is no window at
    /// the process umask) and write into it through [`write_attachment`]
    /// (0600, `O_EXCL`, a fresh name per download). The tests below reach
    /// those helpers directly; only this one can fail when `download_file`
    /// stops calling them.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_download_creates_a_private_tree_and_never_reuses_a_name() {
        const TOKEN: &str = "7654321:AA-not-a-real-bot-token";
        const BYTES: &[u8] = b"\x89PNG-not-really-a-png";

        let temp = tempfile::tempdir().expect("tempdir");
        // Absent, and nested: the download has to create the whole chain, and
        // create every link of it private.
        let dir = temp.path().join("state").join("gateway-attachments");
        let api = file_serving_api("photos/file_0.jpg", BYTES).await;
        let telegram = Telegram {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("building a client"),
            api_base: format!("{api}/bot{TOKEN}"),
            file_base: format!("{api}/file/bot{TOKEN}"),
            offset: 0,
            pending: VecDeque::new(),
            ready: Vec::new(),
            attachments_dir: dir.clone(),
            allowed_chat_ids: vec![42],
        };

        let first = telegram
            .download_file("big-photo")
            .await
            .expect("the download lands");
        assert_eq!(std::fs::read(&first).expect("read back"), BYTES);
        assert_eq!(mode_of(&first), 0o600, "the attachment is owner-only");
        assert_eq!(mode_of(&dir), 0o700, "the directory is owner-only");
        assert_eq!(
            mode_of(&temp.path().join("state")),
            0o700,
            "and so is every parent the download created"
        );
        assert_eq!(first.parent(), Some(dir.as_path()));
        assert!(
            first
                .file_name()
                .and_then(|name| name.to_str())
                .expect("a file name")
                .ends_with("-file_0.jpg"),
            "the remote basename stays readable: {}",
            first.display()
        );

        // The same remote name again, inside the same millisecond: with an
        // O_EXCL write this is the case that has to keep working, so the
        // second download must land beside the first rather than fail on the
        // name or overwrite what is already there.
        let second = telegram
            .download_file("big-photo")
            .await
            .expect("a second download of the same remote name lands too");
        assert_ne!(first, second, "two downloads must not share a name");
        assert_eq!(std::fs::read(&first).expect("read back"), BYTES);
        assert_eq!(
            std::fs::read_dir(&dir).expect("read_dir").count(),
            2,
            "both downloads are on disk"
        );
    }

    /// Adversarial: the attachments tree is the one `~/.wizard` subtree the
    /// gateway creates itself, and its fallback when there is no state dir is
    /// the *shared* system temp dir: a systemd user unit with
    /// `ProtectHome=yes` and no `WIZARD_HOME`, which is the deployment
    /// docs/gateway.md recommends. At the default umask that published every
    /// photo an operator sent the bot to every local account, and the agent
    /// prompt names the absolute path.
    #[cfg(unix)]
    #[test]
    fn downloaded_attachments_are_owner_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("gateway-attachments");
        secrets::create_private_dir(&dir).expect("creates the directory");
        assert_eq!(mode_of(&dir), 0o700, "the directory is owner-only");

        let file = dir.join("1700000000-photo.jpg");
        write_attachment(&file, b"not really a jpeg").expect("writes the attachment");
        assert_eq!(mode_of(&file), 0o600, "the file is owner-only");
        assert_eq!(
            std::fs::read(&file).expect("read back"),
            b"not really a jpeg"
        );

        // `download_file` calls it again per download, so it has to be
        // idempotent, and it must not loosen a directory it did not create.
        secrets::create_private_dir(&dir).expect("creating an existing directory is fine");
        assert_eq!(mode_of(&dir), 0o700);
    }

    /// Adversarial, and the reason [`write_attachment`] is `O_EXCL`: on the
    /// temp-dir fallback the destination directory is shared with every local
    /// account and the name is predictable from the clock, so an open with
    /// `.create(true).truncate(true)` was an arbitrary-file-overwrite
    /// primitive. Another local user pre-plants the name as a symlink to
    /// `~/.bashrc`, the operator's own next photo follows it, and the target
    /// is truncated and refilled with attacker-chosen bytes running as the
    /// operator.
    ///
    /// The write must fail on the planted name, and the thing the link points
    /// at must be untouched: not created, not truncated, not rewritten.
    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_fails_the_write_instead_of_being_followed() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("wizard-gateway-attachments");
        secrets::create_private_dir(&dir).expect("creates the directory");

        // The operator's own file, standing in for ~/.bashrc.
        let victim = temp.path().join("bashrc");
        std::fs::write(&victim, b"# the operator's real file").expect("write the victim");
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        // The attacker's guess at the next attachment name.
        let planted = dir.join("1700000000-4242-0-file_0.jpg");
        std::os::unix::fs::symlink(&victim, &planted).expect("plant the symlink");

        let err = write_attachment(&planted, b"attacker-chosen bytes")
            .expect_err("a name that already exists must fail the download");
        let io = err
            .downcast_ref::<std::io::Error>()
            .unwrap_or_else(|| panic!("expected an io error, got: {err:#}"));
        assert_eq!(
            io.kind(),
            std::io::ErrorKind::AlreadyExists,
            "the refusal must be O_EXCL, not some later failure: {err:#}"
        );

        assert_eq!(
            std::fs::read(&victim).expect("the victim still exists"),
            b"# the operator's real file",
            "the symlink target must not be written through"
        );
        assert_eq!(
            mode_of(&victim),
            0o644,
            "the symlink target's own mode must not be touched either"
        );
        assert!(
            std::fs::symlink_metadata(&planted)
                .expect("stat the planted name")
                .file_type()
                .is_symlink(),
            "the planted link must be left exactly as it was found"
        );

        // A dangling link is the same story: O_EXCL fails on the link itself,
        // so the write cannot create the target behind it either.
        let absent = temp.path().join("not-yet-there");
        let dangling = dir.join("1700000000-4242-1-file_1.jpg");
        std::os::unix::fs::symlink(&absent, &dangling).expect("plant a dangling symlink");
        assert!(
            write_attachment(&dangling, b"attacker-chosen bytes").is_err(),
            "a dangling planted link must fail the write too"
        );
        assert!(
            !absent.exists(),
            "the symlink target must not be created behind the link"
        );

        // Not a symlink, just a name somebody else got there first with: a
        // world-readable file must not be written into and left that way,
        // which is what a mode passed to `open` silently allows (it applies
        // only to a file being created).
        let squatted = dir.join("1700000000-4242-2-file_2.jpg");
        std::fs::write(&squatted, b"squatter").expect("plant a plain file");
        std::fs::set_permissions(&squatted, std::fs::Permissions::from_mode(0o666)).expect("chmod");
        assert!(write_attachment(&squatted, b"photo bytes").is_err());
        assert_eq!(std::fs::read(&squatted).expect("read back"), b"squatter");

        // The control, so the assertions above are measuring the guard and not
        // a write that never works: a free name is written, owner-only.
        let free = dir.join("1700000000-4242-3-file_3.jpg");
        write_attachment(&free, b"photo bytes").expect("a free name still writes");
        assert_eq!(std::fs::read(&free).expect("read back"), b"photo bytes");
        assert_eq!(mode_of(&free), 0o600);
    }

    /// The other half of the `O_EXCL` bargain: once an existing name is a hard
    /// failure, the generated name has to be genuinely unique or an ordinary
    /// album of photos starts losing downloads. Telegram delivers several
    /// photos in one `getUpdates` batch, all named `file_N.jpg`, and they
    /// arrive well inside the same millisecond, which is all the old name had.
    #[test]
    fn attachment_names_are_unique_within_a_millisecond() {
        let names: Vec<String> = (0..64)
            .map(|_| attachment_file_name("photos/file_0.jpg", "AgACAgQAAx"))
            .collect();
        let unique: std::collections::BTreeSet<&String> = names.iter().collect();
        assert_eq!(
            unique.len(),
            names.len(),
            "two downloads of one remote name collided: {names:?}"
        );
        assert!(
            names[0].ends_with("-file_0.jpg"),
            "the remote basename is still readable in the local name: {}",
            names[0]
        );

        // No remote basename: the file id carries the name instead.
        let fallback = attachment_file_name("", "AgACAgQAAx-Q");
        assert!(
            fallback.ends_with("-tg-AgACAgQAAx-Q"),
            "the file-id fallback names the file: {fallback}"
        );
        // And a hostile remote path cannot climb out of the attachments dir.
        let hostile = attachment_file_name("../../../../etc/cron.d/wizard", "id");
        assert_eq!(
            Path::new(&hostile).components().count(),
            1,
            "the local name must stay a single component: {hostile}"
        );
        assert!(hostile.ends_with("-wizard"), "{hostile}");
    }

    #[test]
    fn connect_errors_without_token() {
        // Use a uniquely-named env var that is guaranteed unset.
        let config = GatewayConfig {
            kind: crate::config::GatewayKind::Telegram,
            token_env: Some("WIZARD_TEST_TELEGRAM_TOKEN_ABSENT".to_string()),
            allowed_chat_ids: Vec::new(),
        };
        let err = match Telegram::connect(&config) {
            Ok(_) => panic!("missing token must error"),
            Err(err) => err,
        };
        let message = format!("{err:#}");
        assert!(
            message.contains("WIZARD_TEST_TELEGRAM_TOKEN_ABSENT"),
            "error should name the env var: {message}"
        );
        assert!(
            message.contains("credentials.toml") || message.contains("onboard"),
            "error should mention credentials or onboarding: {message}"
        );
    }

    /// Adversarial: every Telegram URL embeds the bot token, and
    /// `reqwest::Error`'s `Display` appends the URL it failed on. The gateway
    /// loop prints poll failures with `{err:#}`, so an unredacted transport
    /// error hands a working bot token to anyone who can read the journal.
    #[tokio::test]
    async fn transport_errors_never_carry_the_bot_token() {
        const TOKEN: &str = "7654321:AA-not-a-real-bot-token";
        // Port 1 on loopback refuses instantly; nothing leaves the machine.
        let url = format!("http://127.0.0.1:1/bot{TOKEN}/getUpdates");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("building a client");
        let err = http
            .get(&url)
            .send()
            .await
            .expect_err("nothing listens on port 1");

        // The premise, asserted rather than assumed: reqwest really does put
        // the URL (and therefore the token) in the message.
        assert!(
            format!("{err}").contains(TOKEN),
            "reqwest stopped appending the URL; redaction is still right, but \
             this test needs a new premise: {err}"
        );

        let redacted = redact(err);
        assert!(!format!("{redacted}").contains(TOKEN), "{redacted}");
        assert!(!format!("{redacted:?}").contains(TOKEN), "{redacted:?}");

        // And through the anyhow wrapping the gateway actually prints.
        let wrapped: Result<()> = Err(redacted).context("requesting Telegram updates");
        let printed = format!("{:#}", wrapped.expect_err("still an error"));
        assert!(!printed.contains(TOKEN), "{printed}");
        assert!(printed.contains("requesting Telegram updates"), "{printed}");
    }

    /// The fallible reqwest calls in this file. `.json(...)` with arguments is
    /// the infallible request *builder*, so only the bare spelling counts.
    ///
    /// `.text()` joined the list when [`Telegram::call`] started reading error
    /// bodies to find the `retry_after` inside a 429. It is a boundary like any
    /// other — it can fail mid-body and the error it produces carries the
    /// request URL, and that URL is the bot token — and the scan not knowing
    /// about it would have been a hole that the tripwire below could not see.
    const BOUNDARIES: [&str; 5] = [
        ".send()",
        ".error_for_status()",
        ".json()",
        ".bytes()",
        ".text()",
    ];

    /// Scan Rust source for reqwest boundaries that are not followed by the
    /// redaction, returning `(boundaries seen, unredacted ones)`.
    ///
    /// Lines are trimmed and concatenated first, so the same rule covers both
    /// spellings a call can be written in: the multi-line chain this file uses
    /// today, and the ordinary one-liner
    /// (`self.http.get(url).send().await.context(...)?`) somebody adds next.
    /// Matching whole trimmed lines, which is what this used to do, silently
    /// ignored the second form: a new chained call was neither counted nor
    /// required to be redacted, and the tripwire below still passed.
    fn unredacted_boundaries(source: &str) -> (usize, Vec<String>) {
        let joined: String = source
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .collect();

        let mut seen = 0;
        let mut unredacted = Vec::new();
        for needle in BOUNDARIES {
            let mut from = 0;
            while let Some(offset) = joined[from..].find(needle) {
                let end = from + offset + needle.len();
                from = end;
                seen += 1;
                // `.await` may sit between the call and its `map_err`; nothing
                // else may.
                let tail = joined[end..]
                    .strip_prefix(".await")
                    .unwrap_or(&joined[end..]);
                if !tail.starts_with(".map_err(redact)") {
                    let context: String = joined[end..].chars().take(40).collect();
                    unredacted.push(format!("{needle} …{context}"));
                }
            }
        }
        (seen, unredacted)
    }

    /// Adversarial: [`redact`] only protects the boundaries it is written on.
    /// The test above proves the helper works; nothing proved it was *applied*
    /// everywhere, so a request added later without `.map_err(redact)` would
    /// re-open the leak with the whole suite green. There is no runtime seam
    /// for "every boundary" (each one is a separate reqwest call that has to
    /// fail to be observed), so the property is pinned structurally against
    /// this file's own source: every fallible reqwest call in the production
    /// half must be followed by the redaction, `.await` aside.
    #[test]
    fn every_reqwest_boundary_in_this_file_is_redacted() {
        const SOURCE: &str = include_str!("telegram.rs");
        // The test half is excluded: it constructs unredacted errors on
        // purpose, which is the whole premise of the test above.
        let production = SOURCE
            .split("#[cfg(test)]")
            .next()
            .expect("split always yields a first part");

        let (found, unredacted) = unredacted_boundaries(production);
        assert!(
            unredacted.is_empty(),
            "these reqwest boundaries can carry the bot token into a log; \
             follow each with .map_err(redact): {unredacted:#?}"
        );
        // The count this file has today. A tripwire, not a target: if it drops,
        // the scan has stopped matching the code it is supposed to police, and
        // if a request is legitimately removed the number moves with it.
        assert_eq!(
            found, 14,
            "the scan found {found} boundaries instead of 14, so either a request \
             was added or removed (update this number) or the scan has stopped \
             matching the code"
        );

        // The scan itself, proved against a sample: the chained spelling is the
        // one the old line-based version could not see at all.
        let (seen, missed) = unredacted_boundaries(
            "let ok = self.http.get(&self.method_url(\"getMe\")).send().await\
             .context(\"checking the bot token\")?;",
        );
        assert_eq!(seen, 1, "a chained boundary is still a boundary");
        assert_eq!(missed.len(), 1, "and an unredacted one is reported");
        let (seen, missed) = unredacted_boundaries(
            "let ok = self.http.get(&url).send().await.map_err(redact)\
             .context(\"checking the bot token\")?;",
        );
        assert_eq!((seen, missed.len()), (1, 0), "redacted chains pass");
    }

    /// What one request to the fake API is answered with: a delay, a status,
    /// and a body. The delay is what lets a test cancel a poll at a chosen
    /// point rather than hoping for a race.
    struct Reply {
        after: Duration,
        status: u16,
        body: String,
    }

    impl Reply {
        fn now(status: u16, body: &str) -> Self {
            Self {
                after: Duration::ZERO,
                status,
                body: body.to_string(),
            }
        }

        fn ok(body: &str) -> Self {
            Self::now(200, body)
        }

        fn after(mut self, after: Duration) -> Self {
            self.after = after;
            self
        }
    }

    /// A loopback stand-in for the Bot API driven by `handler`, which is given
    /// the number of requests seen before this one and the whole request text
    /// (head and body, so a test can assert on what was actually sent).
    ///
    /// Returns the base URL and the log of every request, so a test can prove
    /// what was *not* sent — that a resumed poll does not ask `getUpdates`
    /// again, that a refused message is not retried — which is most of what
    /// there is to check about a transport that is allowed to retry.
    async fn fake_api<H>(handler: H) -> (String, Arc<Mutex<Vec<String>>>)
    where
        H: Fn(usize, &str) -> Reply + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binding a loopback port");
        let addr = listener.local_addr().expect("local addr");
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let handler = Arc::new(handler);
        let seen = Arc::clone(&log);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 16384];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..read]).into_owned();
                    let index = {
                        let mut log = seen.lock().expect("request log");
                        log.push(request.clone());
                        log.len() - 1
                    };
                    let reply = handler(index, &request);
                    if !reply.after.is_zero() {
                        tokio::time::sleep(reply.after).await;
                    }
                    let response = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{}",
                        reply.status,
                        reply.body.len(),
                        reply.body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        (format!("http://{addr}"), log)
    }

    /// A transport pointed at `api`, allowing chat 42, with attachments landing
    /// in `dir`.
    fn telegram_against(api: &str, dir: PathBuf) -> Telegram {
        const TOKEN: &str = "7654321:AA-not-a-real-bot-token";
        Telegram {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("building a client"),
            api_base: format!("{api}/bot{TOKEN}"),
            file_base: format!("{api}/file/bot{TOKEN}"),
            offset: 0,
            pending: VecDeque::new(),
            ready: Vec::new(),
            attachments_dir: dir,
            allowed_chat_ids: vec![42],
        }
    }

    /// The defect this transport's buffering exists for, reproduced: a poll is
    /// cancelled while it is downloading an attachment, and the message it was
    /// carrying survives.
    ///
    /// This is not a hypothetical cancellation. [`super::Pump::run_turn`]
    /// selects the poll against the running turn, so the poll future is dropped
    /// *every time a turn finishes* — and a turn finishing while a poll is
    /// mid-download is the ordinary case, because the poll only has anything to
    /// download when somebody messaged the bot during the turn. `getUpdates`
    /// had already advanced the cursor past the update by then, so Telegram
    /// would never send it again: the message was gone, the bot was fine, and
    /// nothing anywhere said so.
    ///
    /// The second poll must also not go back to `getUpdates` first. Blocking
    /// for another thirty-second long-poll window while a message that already
    /// arrived sits unhandled in memory is the same bug wearing a hat.
    #[tokio::test]
    async fn a_poll_cancelled_mid_download_keeps_the_message_and_resumes_it() {
        const BYTES: &[u8] = b"not really a png";
        let attachments = tempfile::tempdir().expect("tempdir");
        let (api, log) = fake_api(|index, request| {
            if request.contains("/getUpdates") {
                return Reply::ok(
                    r#"{"ok":true,"result":[{"update_id":7,"message":{"chat":{"id":42},
                       "caption":"look at this","photo":[{"file_id":"big-photo"}]}}]}"#,
                );
            }
            if request.contains("/getFile") {
                let reply = Reply::ok(r#"{"ok":true,"result":{"file_path":"photos/f.jpg"}}"#);
                // The first `getFile` never answers in time: that is the
                // window the cancellation lands in.
                return match index {
                    1 => reply.after(Duration::from_secs(60)),
                    _ => reply,
                };
            }
            Reply::ok(std::str::from_utf8(BYTES).expect("utf8 test bytes"))
        })
        .await;
        let mut telegram = telegram_against(&api, attachments.path().to_path_buf());

        // Dropped while `getFile` is still hanging.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), telegram.poll())
                .await
                .is_err(),
            "the poll must still be in the download when it is cancelled"
        );
        assert_eq!(
            telegram.offset, 8,
            "the cursor moved, so Telegram will never offer update 7 again"
        );
        assert_eq!(
            telegram.pending.len(),
            1,
            "which is exactly why the update has to be held here"
        );

        // The next poll finishes the job — and asks Telegram for nothing new
        // while it still owes the caller something.
        let batch = telegram.poll().await.expect("the resumed poll succeeds");
        assert_eq!(batch.len(), 1, "the message survived the cancellation");
        assert_eq!(batch[0].chat_id, 42);
        assert_eq!(batch[0].text, "look at this");
        assert_eq!(batch[0].attachments.len(), 1, "and so did its photo");

        let requests = log.lock().expect("request log");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("/getUpdates"))
                .count(),
            1,
            "a poll with messages in hand must not wait on a fresh long poll first"
        );
        assert!(telegram.pending.is_empty() && telegram.ready.is_empty());
    }

    /// A rate limit is not a lost reply. Telegram allows roughly one message
    /// per second per chat and [`super::send_reply`] sends the chunks of a long
    /// answer back to back, so 429 is the *ordinary* outcome of answering at
    /// length. It used to come back as a plain error that the serve loop logged
    /// and dropped, which is how a bot comes to answer half a question.
    #[tokio::test]
    async fn a_rate_limited_send_waits_the_stated_time_and_lands() {
        let attachments = tempfile::tempdir().expect("tempdir");
        let (api, log) = fake_api(|index, _| match index {
            0 => Reply::now(
                429,
                r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 1",
                   "parameters":{"retry_after":1}}"#,
            ),
            _ => Reply::ok(r#"{"ok":true,"result":{}}"#),
        })
        .await;
        let telegram = telegram_against(&api, attachments.path().to_path_buf());

        telegram
            .send(42, "the answer")
            .await
            .expect("a 429 is waited out, not given up on");
        assert_eq!(
            log.lock().expect("request log").len(),
            2,
            "exactly one retry, not a storm"
        );
    }

    /// The other half: a refusal is *not* retried, because the identical
    /// payload will be refused identically, and four copies of the same
    /// rejection is four times the delay before the caller gets to try
    /// something that might work.
    #[tokio::test]
    async fn a_refused_send_is_reported_once_rather_than_retried() {
        let attachments = tempfile::tempdir().expect("tempdir");
        let (api, log) = fake_api(|_, _| {
            Reply::now(
                400,
                r#"{"ok":false,"error_code":400,
                   "description":"Bad Request: chat not found"}"#,
            )
        })
        .await;
        let telegram = telegram_against(&api, attachments.path().to_path_buf());

        let err = telegram
            .send(42, "the answer")
            .await
            .expect_err("a 400 is a refusal");
        let printed = format!("{err:#}");
        assert!(printed.contains("chat not found"), "{printed}");
        assert!(printed.contains("400"), "{printed}");
        assert_eq!(
            log.lock().expect("request log").len(),
            1,
            "a permanent refusal is attempted exactly once"
        );
    }

    /// The formatting fallback, end to end: Telegram refuses the HTML and the
    /// same words go out again with no `parse_mode` at all.
    ///
    /// This is the difference between a cosmetic conversion bug and a lost
    /// answer. A `sendMessage` that 400s on a stray entity simply never
    /// arrives, and from the chat that is indistinguishable from the agent
    /// having had nothing to say.
    #[tokio::test]
    async fn a_message_telegram_will_not_parse_goes_out_as_plain_text() {
        let attachments = tempfile::tempdir().expect("tempdir");
        let (api, log) = fake_api(|_, request| {
            if request.contains("parse_mode") {
                return Reply::now(
                    400,
                    r#"{"ok":false,"error_code":400,
                       "description":"Bad Request: can't parse entities"}"#,
                );
            }
            Reply::ok(r#"{"ok":true,"result":{}}"#)
        })
        .await;
        let telegram = telegram_against(&api, attachments.path().to_path_buf());

        telegram
            .send_rich(42, "**bold** and a <tag>")
            .await
            .expect("the words still have to arrive");

        let requests = log.lock().expect("request log");
        assert_eq!(requests.len(), 2, "the formatted try, then the plain one");
        assert!(
            !requests[1].contains("parse_mode"),
            "the fallback must not carry the markup that was just refused: {}",
            requests[1]
        );
        assert!(
            requests[1].contains("**bold**"),
            "and it carries the literal text: {}",
            requests[1]
        );
    }

    /// The rule that decides whether a reply is delivered or lost, exercised
    /// where it can actually be exercised: reproducing a real 429 means
    /// flooding Telegram, so the classification is a pure function and this is
    /// the test that keeps it honest.
    #[test]
    fn a_rate_limit_is_waited_out_and_a_refusal_is_not() {
        assert_eq!(classify_call(200, "", 0), Attempt::Accepted);
        assert_eq!(classify_call(201, "", 3), Attempt::Accepted);

        // 429: retried, never past the budget, and honouring the number
        // Telegram put in the body rather than a guess.
        let body = r#"{"ok":false,"error_code":429,"description":"Too Many Requests",
                       "parameters":{"retry_after":7}}"#;
        match classify_call(429, body, 0) {
            Attempt::Again { wait, why } => {
                assert!(wait >= Duration::from_secs(7), "{wait:?}");
                assert!(wait <= SEND_RETRY_BUDGET, "{wait:?}");
                assert!(why.contains("429"), "{why}");
            }
            other => panic!("a 429 must be retried, got {other:?}"),
        }
        // A body with no `parameters` still retries, on the ladder alone.
        assert!(matches!(classify_call(429, "", 0), Attempt::Again { .. }));
        // And a hostile deadline cannot park the send for a day: the budget
        // caps it, because this wait happens inside the select that keeps the
        // running turn alive.
        match classify_call(429, r#"{"parameters":{"retry_after":86400}}"#, 0) {
            Attempt::Again { wait, .. } => assert!(wait <= SEND_RETRY_BUDGET, "{wait:?}"),
            other => panic!("still a retry, got {other:?}"),
        }

        // Telegram's own outages: worth waiting out.
        for status in [500, 502, 503] {
            assert!(
                matches!(classify_call(status, "", 1), Attempt::Again { .. }),
                "{status} is transient"
            );
        }

        // Everything else is the payload's fault and will not improve.
        let refusal = classify_call(
            400,
            r#"{"ok":false,"description":"Bad Request: can't parse entities"}"#,
            0,
        );
        assert_eq!(
            refusal,
            Attempt::Refused {
                status: 400,
                description: "Bad Request: can't parse entities".to_string(),
            }
        );
        assert!(matches!(
            classify_call(403, "", 0),
            Attempt::Refused { status: 403, .. }
        ));
    }

    /// A `description` comes from the far end, so it goes through [`one_line`]
    /// before it can reach a journal: the gateway prints send failures, and a
    /// server-supplied escape sequence in one of them would be repainting an
    /// operator's terminal from off the machine.
    #[test]
    fn a_server_supplied_description_cannot_carry_escape_sequences() {
        let hostile = serde_json::json!({ "description": "bad\u{1b}[2Jrequest" }).to_string();
        let described = api_error(&hostile);
        assert!(!described.contains('\u{1b}'), "{described:?}");
        assert_eq!(described, "bad [2Jrequest");

        // Not the documented envelope at all: no description, and no panic.
        assert_eq!(api_error(""), "");
        assert_eq!(api_error("<html>502 Bad Gateway</html>"), "");
        assert_eq!(stated_retry_after("<html>"), None);
        assert_eq!(stated_retry_after(r#"{"parameters":{}}"#), None);
        assert_eq!(
            stated_retry_after(r#"{"parameters":{"retry_after":3}}"#),
            Some(Duration::from_secs(3))
        );
    }

    #[test]
    fn parses_get_updates_payload() {
        let raw = r#"{
            "ok": true,
            "result": [
                {"update_id": 10, "message": {"chat": {"id": 555}, "text": "hi"}},
                {"update_id": 11, "message": {"chat": {"id": 555}}},
                {"update_id": 12}
            ]
        }"#;
        let body: GetUpdates = serde_json::from_str(raw).expect("valid payload");
        assert!(body.ok);
        assert_eq!(body.result.len(), 3);
        let texts: Vec<_> = body
            .result
            .into_iter()
            .filter_map(|u| u.message)
            .filter_map(|m| m.text.map(|t| (m.chat.id, t)))
            .collect();
        assert_eq!(texts, vec![(555, "hi".to_string())]);
    }

    #[test]
    fn parses_caption_only_message() {
        let raw = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 20,
                    "message": {
                        "chat": {"id": 42},
                        "caption": "describe this",
                        "photo": [
                            {"file_id": "small"},
                            {"file_id": "large"}
                        ]
                    }
                }
            ]
        }"#;
        let body: GetUpdates = serde_json::from_str(raw).expect("valid payload");
        let msg = body.result[0].message.as_ref().expect("message");
        assert_eq!(msg.chat.id, 42);
        assert_eq!(msg.caption.as_deref(), Some("describe this"));
        assert!(msg.text.is_none());
        assert_eq!(msg.photo.as_ref().map(|p| p.len()), Some(2));

        let (text, fetch) = classify_message(msg).expect("caption+photo becomes inbound");
        assert_eq!(text, "describe this");
        // The largest (last) photo size is the one fetched.
        assert_eq!(fetch.as_deref(), Some("large"));
    }

    #[test]
    fn parses_photo_without_caption() {
        let raw = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 21,
                    "message": {
                        "chat": {"id": 7},
                        "photo": [{"file_id": "only"}]
                    }
                }
            ]
        }"#;
        let body: GetUpdates = serde_json::from_str(raw).expect("valid payload");
        let msg = body.result[0].message.as_ref().expect("message");
        let (text, fetch) = classify_message(msg).expect("photo-only becomes inbound");
        assert_eq!(text, PHOTO_ONLY_PROMPT);
        assert_eq!(fetch.as_deref(), Some("only"));
    }

    #[test]
    fn parses_image_document_with_caption() {
        let raw = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 22,
                    "message": {
                        "chat": {"id": 9},
                        "caption": "scan this",
                        "document": {
                            "file_id": "doc1",
                            "file_name": "shot.png",
                            "mime_type": "image/png"
                        }
                    }
                }
            ]
        }"#;
        let body: GetUpdates = serde_json::from_str(raw).expect("valid payload");
        let msg = body.result[0].message.as_ref().expect("message");
        assert_eq!(
            msg.document.as_ref().unwrap().mime_type.as_deref(),
            Some("image/png")
        );
        let (text, fetch) = classify_message(msg).expect("document becomes inbound");
        assert_eq!(text, "scan this");
        assert_eq!(fetch.as_deref(), Some("doc1"));

        // The is_image/mime gate: a non-image document without a caption is
        // not routable.
        let plain = Message {
            chat: Chat {
                id: 9,
                kind: Some("private".to_string()),
            },
            from: None,
            text: None,
            caption: None,
            photo: None,
            document: Some(Document {
                file_id: "doc2".to_string(),
                file_name: Some("notes.txt".to_string()),
                mime_type: Some("text/plain".to_string()),
            }),
        };
        assert!(classify_message(&plain).is_none());
    }

    #[test]
    fn sanitize_filename_strips_path_components() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("ok-file_1.jpg"), "ok-file_1.jpg");
        assert_eq!(sanitize_filename("weird name!.png"), "weird_name_.png");
    }

    #[test]
    fn is_image_filename_detects_common_extensions() {
        assert!(is_image_filename("x.PNG"));
        assert!(is_image_filename("a.jpeg"));
        assert!(!is_image_filename("notes.txt"));
    }

    // -----------------------------------------------------------------------
    // The calls `wizard gateway setup` is built on
    // -----------------------------------------------------------------------

    /// A stand-in for the Telegram API driven by a script. `getMe` answers
    /// `me` with status `me_status`; each `getUpdates` answers with the next
    /// body in `updates`, and with an empty batch once the script runs out.
    /// The returned vector records every request line, so a test can see the
    /// offsets that were actually sent.
    async fn scripted_api(
        me_status: u16,
        me: &'static str,
        updates: Vec<String>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binding a loopback port");
        let addr = listener.local_addr().expect("local addr");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let script = Arc::new(std::sync::Mutex::new(updates.into_iter()));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let recorder = Arc::clone(&recorder);
                let script = Arc::clone(&script);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..read]).into_owned();
                    let line = request.lines().next().unwrap_or_default().to_string();
                    let is_get_me = line.contains("/getMe");
                    recorder.lock().expect("recorder").push(line);

                    let (status, body) = if is_get_me {
                        (me_status, me.to_string())
                    } else {
                        let next = script.lock().expect("script").next();
                        (
                            200,
                            next.unwrap_or_else(|| r#"{"ok":true,"result":[]}"#.to_string()),
                        )
                    };
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    const FAKE_TOKEN: &str = "7654321:AA-not-a-real-bot-token";

    fn client_for(api: &str) -> Telegram {
        Telegram {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("building a client"),
            api_base: format!("{api}/bot{FAKE_TOKEN}"),
            file_base: format!("{api}/file/bot{FAKE_TOKEN}"),
            offset: 0,
            pending: VecDeque::new(),
            ready: Vec::new(),
            attachments_dir: std::env::temp_dir(),
            allowed_chat_ids: Vec::new(),
        }
    }

    /// `getMe` is what turns a mistyped token into an error at the moment it
    /// was typed. The success path names the bot the operator is about to be
    /// told to message; the 401 path says so in words, and — like every other
    /// error in this file — without the token in it.
    #[tokio::test]
    async fn get_me_names_the_bot_and_a_rejected_token_says_so_without_quoting_it() {
        let (api, _) = scripted_api(
            200,
            r#"{"ok":true,"result":{"first_name":"Wizard","username":"wizard_bot"}}"#,
            Vec::new(),
        )
        .await;
        let bot = client_for(&api).get_me().await.expect("getMe succeeds");
        assert_eq!(bot.username, "wizard_bot");
        assert_eq!(bot.name, "Wizard");
        assert_eq!(bot.label(), "@wizard_bot (Wizard)");

        let (api, _) = scripted_api(401, r#"{"ok":false}"#, Vec::new()).await;
        let err = client_for(&api)
            .get_me()
            .await
            .expect_err("a rejected token must fail here, not at the first poll");
        let text = format!("{err:#}");
        assert!(text.contains("401"), "{text}");
        assert!(text.contains("@BotFather"), "{text}");
        assert!(!text.contains(FAKE_TOKEN), "{text}");
    }

    /// Discovery starts from an empty queue on purpose. A bot username is
    /// public, so the backlog of an unattended bot can hold a stranger's
    /// message — and reporting *that* chat id as "the message that just
    /// arrived" is the one way this flow could talk somebody into allow-listing
    /// an attacker. So the backlog is drained and counted, and the id reported
    /// is the one that arrives afterwards.
    #[tokio::test]
    async fn discovery_drains_the_backlog_first_and_reports_the_chat_that_answers() {
        let stranger = r#"{"ok":true,"result":[
            {"update_id":10,"message":{"chat":{"id":666,"type":"private"},"text":"hello?"}}
        ]}"#;
        let empty = r#"{"ok":true,"result":[]}"#;
        let operator = r#"{"ok":true,"result":[
            {"update_id":11,"message":{
                "chat":{"id":4242,"type":"private"},
                "from":{"first_name":"Teddy","last_name":"Tennant","username":"teddy"},
                "text":"hi"}}
        ]}"#;
        let (api, seen) = scripted_api(
            200,
            r#"{"ok":true}"#,
            vec![
                stranger.to_string(),
                empty.to_string(),
                operator.to_string(),
            ],
        )
        .await;
        let mut telegram = client_for(&api);

        assert_eq!(
            telegram.drain_pending().await.expect("drain"),
            1,
            "the stranger's message is consumed and counted, not reported"
        );
        let sighting = telegram
            .next_chat(Duration::from_secs(5))
            .await
            .expect("no transport error")
            .expect("a message arrived");
        assert_eq!(
            sighting,
            ChatSighting {
                chat_id: 4242,
                kind: "private".to_string(),
                from: Some("Teddy Tennant (@teddy)".to_string()),
            },
            "the id offered is the one that arrived after the drain"
        );

        // The cursor is shared with `poll`, and it moved past both updates, so
        // a gateway started next does not re-process either.
        assert_eq!(telegram.offset, 12);
        let requests = seen.lock().expect("recorder").clone();
        assert!(
            requests.iter().any(|line| line.contains("offset=11")),
            "the drain advanced the cursor before the wait began: {requests:#?}"
        );
    }

    /// Time-bounded: no message means `None`, not a wait forever. The caller
    /// turns that into "no message arrived, re-run"; Ctrl-C is the other way
    /// out and needs nothing from this function.
    #[tokio::test]
    async fn waiting_for_a_message_gives_up_instead_of_blocking_forever() {
        let (api, _) = scripted_api(200, r#"{"ok":true}"#, Vec::new()).await;
        let waited = std::time::Instant::now();
        let found = client_for(&api)
            .next_chat(Duration::from_millis(400))
            .await
            .expect("no transport error");
        assert!(found.is_none(), "{found:?}");
        assert!(
            waited.elapsed() < Duration::from_secs(20),
            "it returned on its own deadline, not Telegram's"
        );
    }

    /// Names come from whoever is on the other end, and the line they are
    /// printed on sits directly above "Add chat …? [y/N]". A first name of
    /// `ESC[2K` must not be able to repaint that question.
    #[test]
    fn a_sender_name_cannot_carry_escape_sequences_onto_the_terminal() {
        let hostile = User {
            first_name: Some("\u{1b}[2K\rAdministrator".to_string()),
            last_name: None,
            username: Some("x\u{7}y".to_string()),
        };
        let name = sender_name(&hostile).expect("a name");
        assert!(!name.chars().any(char::is_control), "{name:?}");
        assert_eq!(name, "[2K Administrator (@x y)");

        // Whichever half exists is used, and neither means no name at all.
        assert_eq!(
            sender_name(&User {
                first_name: None,
                last_name: None,
                username: Some("solo".to_string()),
            }),
            Some("@solo".to_string())
        );
        assert_eq!(
            sender_name(&User {
                first_name: None,
                last_name: None,
                username: None,
            }),
            None
        );

        // And it cannot occupy the terminal with one row either.
        let long = "a".repeat(500);
        assert!(
            one_line(&long).chars().count() <= DISPLAY_CHARS + 1,
            "clipped"
        );
    }
}
