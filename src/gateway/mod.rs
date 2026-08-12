//! Messaging gateway: expose Wizard over a chat platform so inbound messages
//! drive one autonomous agent turn each and the reply is sent back.
//!
//! The gateway runs as a long-lived headless process (`wizard --gateway`). It
//! builds a single [`Agent`](crate::agent::Agent) in sovereign / auto-approve
//! posture and keeps it for the whole session, so the conversation continues
//! across messages. The transport is abstracted behind the [`Gateway`] trait;
//! [`telegram::Telegram`] is the first concrete backend, and [`none`] is a
//! no-op that errors with an actionable message.
//!
//! A message that names a slash command is not a turn: it runs through the one
//! dispatcher every surface shares, and the answer is the reply. See
//! [`command`] for the surface, and [`command_line`] for the rule that keeps a
//! pasted path (`/etc/hosts`) a prompt rather than a mangled command. The one
//! exception is `/stop` ([`GATEWAY_NATIVE`]), which the loop answers itself
//! because it is about the turn in flight rather than about the agent's state.
//!
//! Which is why the loop polls and runs a turn at the same time: see [`serve`]
//! and [`Pump`]. A gateway that awaits its turn inline is deaf for exactly as
//! long as it is busy, and "busy" is the only interesting thing to be able to
//! say something about.
//!
//! Because it is long-lived and has no terminal, it is also the first surface
//! to install itself as a background service: [`service`] turns
//! `wizard gateway install` into a systemd user unit or a launchd agent via
//! [`crate::platform::service`], and owns the one gateway-specific part of
//! that — getting the bot token to a process that inherits no environment,
//! without writing it into a world-readable unit file.

pub mod command;
pub mod format;
pub mod none;
pub mod service;
pub mod setup;
pub mod telegram;

use std::collections::VecDeque;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::agent::session::Session;
use crate::agent::{
    Agent, AgentEvent, CancelHandle, PlanVerdict, build_headless_agent_for_session,
};
use crate::cli::Cli;
use crate::commands::Execution;
use crate::config::{Config, GatewayKind, Mode};

/// Telegram's hard cap is 4096 UTF-16 code units; stay well under it.
const MAX_MESSAGE_CHARS: usize = 4000;

/// Cap on a single reply before it is split into messages, so a runaway turn
/// cannot flood a chat.
const MAX_REPLY_CHARS: usize = 24_000;

/// An inbound message from a messaging gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    /// Platform chat identifier the message came from (and replies go to).
    pub chat_id: i64,
    /// The message text (caption when the user sent a photo/document with
    /// caption; a short placeholder for media-only messages).
    pub text: String,
    /// Local paths of files downloaded from the platform (photos, image
    /// documents). Empty for pure text. Absolute paths so the agent can
    /// `read_file` them, and so a future vision path can pick them up.
    pub attachments: Vec<std::path::PathBuf>,
}

impl Inbound {
    /// Build a text-only inbound (no attachments). Used by transports and tests.
    pub fn text(chat_id: i64, text: impl Into<String>) -> Self {
        Self {
            chat_id,
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    /// Build a bodyless inbound for a chat the transport already refused.
    ///
    /// A transport checks [`is_authorized`] the moment it knows the chat id
    /// (see `telegram::Telegram::message_to_inbound`) so that nothing is
    /// downloaded and no reply is sent for a stranger. It still hands the id
    /// up, because [`serve`] owns the operator-facing refusal: the id has to
    /// reach the journal, otherwise there is no way to learn which id to add
    /// to `gateway.allowed_chat_ids`. The message body is deliberately
    /// dropped here: it is attacker-controlled text for a chat that is not
    /// allowed to say anything to the agent, and [`serve`] re-checks the id
    /// against the same allow-list before it is used for anything else.
    pub fn refused(chat_id: i64) -> Self {
        Self {
            chat_id,
            text: String::new(),
            attachments: Vec::new(),
        }
    }

    /// Prompt text handed to the agent: original text plus absolute attachment
    /// paths when any files were downloaded, so a text-only model can still
    /// open them with tools.
    pub fn agent_prompt(&self) -> String {
        if self.attachments.is_empty() {
            return self.text.clone();
        }
        let mut prompt = self.text.clone();
        prompt.push_str("\n\n");
        for path in &self.attachments {
            prompt.push_str(&format!("[attached: {}]\n", path.display()));
        }
        prompt
    }
}

/// A chat transport: long-poll for inbound messages and send replies. The
/// agent loop and reply formatting are transport-agnostic and live in
/// [`serve`].
#[async_trait]
pub trait Gateway: Send + Sync {
    /// Short human label for status output (e.g. `"telegram"`).
    fn label(&self) -> &str;

    /// Block until the next batch of inbound messages arrives. A transient
    /// network error returns `Err`; [`serve`] retries with backoff. The
    /// implementation tracks its own cursor so messages are not reprocessed.
    async fn poll(&mut self) -> Result<Vec<Inbound>>;

    /// Send `text` to `chat_id`. Callers pre-split long replies via
    /// [`split_message`].
    async fn send(&self, chat_id: i64, text: &str) -> Result<()>;

    /// Optional UX hint that the bot is working on a reply (e.g. Telegram
    /// `sendChatAction typing`). Default is a no-op so transports that do not
    /// support it need not implement anything.
    async fn typing(&self, _chat_id: i64) -> Result<()> {
        Ok(())
    }

    /// Send `text` as platform-formatted rich text, falling back to plain.
    ///
    /// Default is [`Gateway::send`], so a transport with no markup support
    /// needs nothing. The agent answers in markdown everywhere, and a chat that
    /// shows literal `**bold**` and unindented code fences makes the part of a
    /// reply most worth reading — a command, a patch — the part that reads
    /// worst.
    async fn send_rich(&self, chat_id: i64, text: &str) -> Result<()> {
        self.send(chat_id, text).await
    }

    /// Publish the command list to the platform, so its client can offer them.
    ///
    /// This is what makes typing `/` in Telegram pop up the menu with
    /// descriptions, which is the difference between a bot you have to
    /// remember the commands for and one that tells you. Default no-op.
    async fn advertise_commands(&self, _commands: &[AdvertisedCommand]) -> Result<()> {
        Ok(())
    }
}

/// One command offered to the platform's own command menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisedCommand {
    /// Bare name, no leading slash — the platforms all add their own.
    pub name: String,
    /// One line, shown beside the name in the client's menu.
    pub description: String,
}

/// The one control the gateway answers itself, named once for the three places
/// that have to agree about it: the menu, the router, and the two loops a
/// `/stop` can land in.
const STOP: &str = "stop";

/// The liveness probe, named once for the same reason as [`STOP`].
const PING: &str = "ping";

/// Controls the gateway runs itself, which are deliberately *not* rows of
/// [`crate::commands::COMMANDS`].
///
/// `/stop` interrupts the turn that is running. Everywhere else that is a key —
/// Ctrl-C at the terminal, Esc in the window — and a chat has no keyboard, so
/// it has to be a message. It is not a table row because there is nothing for
/// the other surfaces' columns to say: the shared dispatcher runs against an
/// agent that is, by construction, not in a turn, so a `/stop` routed through
/// it could only ever answer "nothing is running".
///
/// `/ping` answers, immediately and from the poll loop itself, with what the
/// gateway is doing. It is the only thing in this file that answers the
/// question the whole module is written around: *is it still there?* A chat
/// cannot tell a bot that is thinking hard from a bot whose process died, whose
/// poll loop wedged, or whose allow-list refuses it — all four look like
/// silence. So the probe deliberately jumps the backlog and is answered inside
/// [`Pump::absorb`] rather than through the dispatcher: an answer that queued
/// behind a running turn would prove nothing, because queueing behind a running
/// turn is exactly what a wedged gateway also does.
///
/// `(name, description)`, in the shape the menu wants. The advertised-menu test
/// holds these to the same rules as the table's own names, including that a
/// control here never shadows a row there.
const GATEWAY_NATIVE: &[(&str, &str)] = &[
    (STOP, "stop the turn that is running (this chat's Ctrl-C)"),
    (
        PING,
        "is the gateway still listening? answers straight away",
    ),
];

/// Whether `name` is a gateway-native control rather than a row of the table.
fn is_gateway_native(name: &str) -> bool {
    GATEWAY_NATIVE.iter().any(|(native, _)| *native == name)
}

/// The gateway-native controls in the shape `/help` lists a command, so the
/// chat's help and the chat's menu cannot disagree about what it can type.
///
/// Appended by [`command::apply_command`] rather than by
/// [`crate::commands::surface::help_text`], which is derived from the table and
/// is right to know nothing about a control that only exists here.
pub(crate) fn native_help() -> String {
    let mut text = String::from("\n\nin this chat only:");
    for (name, description) in GATEWAY_NATIVE {
        text.push_str(&format!("\n  /{name} — {description}"));
    }
    text
}

/// The commands this surface will actually run, in table order, plus the
/// gateway-native controls ([`GATEWAY_NATIVE`]).
///
/// Derived from [`crate::commands::COMMANDS`] rather than listed here, so the
/// menu a chat sees cannot advertise something the gateway would refuse, and a
/// command added to the table arrives in Telegram without anybody remembering
/// to add it twice. `Unavailable` rows are filtered out for the same reason: an
/// autocomplete entry that answers "not available in this chat" is worse than
/// no entry at all.
///
/// Telegram's constraints, which are the tightest of the platforms and so the
/// ones worth meeting here: names are `[a-z0-9_]{1,32}` and descriptions are
/// 1–256 characters. Every current name is already lowercase ASCII; the filter
/// is defensive, so a future command with a hyphen is dropped from the menu
/// rather than making `setMyCommands` reject the whole batch.
pub fn advertised_commands() -> Vec<AdvertisedCommand> {
    let mut offered: Vec<AdvertisedCommand> = crate::commands::COMMANDS
        .iter()
        .filter(|spec| {
            spec.execution(crate::commands::surface::Surface::Gateway) != Execution::Unavailable
        })
        .filter(|spec| {
            !spec.name.is_empty()
                && spec.name.len() <= 32
                && spec
                    .name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        })
        .map(|spec| AdvertisedCommand {
            name: spec.name.to_string(),
            description: spec.description.chars().take(256).collect(),
        })
        .collect();
    // The controls that are this surface's own. A menu derived purely from the
    // table would leave the chat with no way to interrupt a turn *and* no way
    // to find out that there is one.
    offered.extend(
        GATEWAY_NATIVE
            .iter()
            .map(|(name, description)| AdvertisedCommand {
                name: (*name).to_string(),
                description: description.chars().take(256).collect(),
            }),
    );
    offered
}

/// Whether `chat_id` may drive the agent. The allow-list is closed: the id
/// must be listed explicitly, and an **empty list allows nobody**.
///
/// This is deliberately fail-closed. A gateway turn runs in sovereign posture
/// with the unrestricted tool set on the operator's machine, and `/plan` can
/// turn the read-only phase off, so "no list configured" must not mean
/// "anyone who guesses the bot's handle owns this box". An operator who wants
/// to let someone in adds their id to `gateway.allowed_chat_ids`; the refusal
/// path logs the id so it can be copied from the journal.
///
/// Transports call this themselves, as soon as the chat id is known and
/// *before* any work is done for the message (see [`Inbound::refused`]);
/// [`authorize_inbound`] then re-checks it here, where the refusal is emitted.
pub fn is_authorized(chat_id: i64, allowed: &[i64]) -> bool {
    allowed.contains(&chat_id)
}

/// The one warning about allow-listing a group chat, or `None` when every id
/// in `allowed` is positive.
///
/// A negative id is a group or supergroup, and [`is_authorized`] authorises a
/// *chat*, not a person: the sender is never consulted (the transport parses
/// it only to print a name during setup). So allow-listing a group admits
/// every current member and every future one, and anybody in it who can add
/// people can grant that too — with the sovereign tool set, on this machine.
///
/// It lives here, said once, because three surfaces have to say it — the
/// gateway at startup, `wizard doctor`, and `wizard gateway setup` at the
/// moment it offers to write the id down — and three copies of a security
/// warning is three chances for the mildest one to be the one somebody reads.
/// None of the three refuses: allow-listing a group you control is a
/// legitimate thing to do deliberately.
pub fn group_chat_warning(allowed: &[i64]) -> Option<String> {
    let groups: Vec<i64> = allowed.iter().copied().filter(|id| *id < 0).collect();
    (!groups.is_empty()).then(|| {
        format!(
            "{groups:?} look like group chats. The allow-list authorises a chat, not a \
             person, so every member of those groups — including anyone added later — can \
             run agent turns on this machine with full tool access. Prefer a one-to-one \
             chat id."
        )
    })
}

/// The vague reply an unauthorized chat gets. Deliberately says nothing about
/// who is allowed, whether the id is close to one that is, or what the bot
/// does: a stranger learns only that this chat is not it.
const UNAUTHORIZED_REPLY: &str = "unauthorized: this chat is not allowed";

/// What a `/stop` is answered with when there is nothing to stop. Plainly, and
/// not with silence: the chat cannot see whether a turn is running.
const NOTHING_TO_STOP: &str = "nothing to stop — no turn is running.";

/// What the chat is told once a `/stop` has taken effect. It says the session
/// survived on purpose: a stopped turn is not a broken conversation, and the
/// next message continues where this one left off.
const STOPPED_REPLY: &str =
    "stopped the running turn. The conversation is intact — send another message to carry on.";

/// What a message that arrived mid-turn is told. There is one agent, so it
/// waits its turn rather than being dropped or run beside the one in flight.
const QUEUED_REPLY: &str = "queued — still working on the previous message. \
     This one runs when that turn finishes; /stop cancels it.";

/// What the chat is told when a turn panicked. It says the gateway survived,
/// because the only thing the reader can otherwise conclude from an answer that
/// never comes is that the bot is dead — and acting on that (restarting the
/// service, losing the conversation) would be the wrong move.
const TURN_PANICKED: &str = "that turn hit a bug and stopped. The gateway is still \
     listening and the conversation is intact — send another message to carry on. The \
     failure was:";

/// The same for a slash command. Kept separate because the advice differs:
/// nothing about the agent's state changed, so the command is worth retyping.
const COMMAND_PANICKED: &str = "that command hit a bug. The gateway is still listening; \
     nothing about the session changed. The failure was:";

/// How long a Ctrl-C waits for the turn it just cancelled to come back on its
/// own before the process leaves without it. The terminal's grace, for the same
/// reason: the flag is checked between stream chunks and tool calls, so a tool
/// already running (a five-minute build) cannot be shortened by it.
const INTERRUPT_GRACE: std::time::Duration = std::time::Duration::from_millis(1_500);

/// Apply the allow-list to one inbound message. `true` means the message may
/// reach the agent; `false` means it was refused, and the refusal has already
/// been logged (with the chat id, which is the one thing the operator needs)
/// and answered with exactly one vague reply.
///
/// This is the whole of [`serve`]'s enforcement, split out so the refusal path
/// can be exercised against a recording gateway. It is the *second* gate, not
/// the first: a transport must refuse before it downloads an attachment or
/// answers an unsupported message type, because by the time a message reaches
/// here the bytes would already be on disk (see [`Inbound::refused`]).
async fn authorize_inbound(gateway: &dyn Gateway, message: &Inbound, allowed: &[i64]) -> bool {
    if is_authorized(message.chat_id, allowed) {
        return true;
    }
    // Print the id as well as tracing it: the gateway usually runs under
    // systemd with no tracing subscriber configured, and the id is the one
    // thing the operator needs in order to allow the chat.
    let refusal = format!(
        "refused chat {} (not in gateway.allowed_chat_ids{}); \
         add it to ~/.wizard/config.toml to allow this chat",
        message.chat_id,
        if allowed.is_empty() {
            ", which is empty, so every chat is refused"
        } else {
            ""
        }
    );
    eprintln!("{refusal}");
    tracing::warn!("{refusal}");
    if let Err(err) = gateway.send(message.chat_id, UNAUTHORIZED_REPLY).await {
        eprintln!("failed to send rejection: {err:#}");
    }
    false
}

/// Floor between two empty polls. Well under the long-poll window, so it never
/// delays a real message; enough that a server ignoring the window cannot turn
/// the loop into a spin.
const MIN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Split `text` into chunks of at most `max` UTF-16 code units — the unit
/// Telegram counts — preferring to break on line boundaries. A single line
/// longer than `max` is hard-split. The concatenation of the chunks equals
/// `text`; an empty input yields one empty chunk.
pub fn split_message(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    // Telegram's limit is UTF-16 code units, so that is what the budget counts.
    //
    // It used to count `chars()`, which is the same number only for the basic
    // plane: every emoji, and every other astral-plane character, is two units.
    // A 4,000-character chunk with more than 96 of them exceeds the 4,096-unit
    // cap the constant's own comment names, `sendMessage` rejects it, and the
    // loop that sends the chunks used to `break` — so an ordinary
    // emoji-decorated long answer arrived truncated with nothing in the chat to
    // say so. The existing test used `é`, which is one unit, and could not see
    // it.
    fn units(s: &str) -> usize {
        s.encode_utf16().count()
    }

    for line in text.split_inclusive('\n') {
        // A line that cannot fit on its own is hard-split by characters.
        if units(line) > max {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            for ch in line.chars() {
                if units(&current) + ch.len_utf16() > max {
                    chunks.push(std::mem::take(&mut current));
                }
                current.push(ch);
            }
            continue;
        }
        if units(&current) + units(line) > max {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Entry point for `wizard --gateway`: dispatch on the configured gateway
/// kind. [`GatewayKind::None`] is an actionable error; otherwise the matching
/// transport is constructed and driven by [`serve`].
///
/// Project root is `$WIZARD_GATEWAY_CWD` when set (useful for systemd units
/// whose `WorkingDirectory` is `$HOME`), otherwise the process current
/// directory.
pub async fn run(config: Config, _cli: Cli) -> Result<()> {
    let project_root = gateway_project_root()?;
    match config.gateway.kind {
        GatewayKind::None => none::NoneGateway.poll().await.map(|_| ()),
        GatewayKind::Telegram => {
            let gateway = telegram::Telegram::connect(&config.gateway)?;
            serve(Box::new(gateway), config, &project_root).await
        }
    }
}

/// Resolve the project root the gateway agent should operate on.
fn gateway_project_root() -> Result<std::path::PathBuf> {
    if let Ok(cwd) = std::env::var("WIZARD_GATEWAY_CWD") {
        let path = std::path::PathBuf::from(cwd.trim());
        if !path.as_os_str().is_empty() {
            return Ok(path);
        }
    }
    std::env::current_dir().context("determining project root")
}

/// Drive a gateway: build one sovereign agent, then loop, poll for inbound
/// messages (retrying network errors with backoff), enforce the allow-list
/// ([`authorize_inbound`]), run one agent turn per message, and send the reply
/// (split into platform-sized chunks). Runs until Ctrl-C.
///
/// The allow-list is enforced twice on purpose: the transport refuses before
/// it does any work for the message (no attachment download, no reply), and
/// this loop refuses again before the agent sees anything.
///
/// # The turn runs *beside* the poll, not instead of it
///
/// The loop used to await a turn inline, so for as long as the agent was
/// working nothing polled — and a message sent during a turn could not even be
/// received, let alone acted on. That made `/stop` impossible to implement as a
/// command, because the one moment it is worth typing is the one moment the
/// gateway was deaf.
///
/// So the turn is a future the loop selects on ([`Pump::run_turn`]) while it
/// keeps polling: `poll` borrows the transport and the turn borrows the agent,
/// which are disjoint, so neither has to be spawned onto a task and the agent
/// never leaves this function. What arrives during a turn is queued
/// ([`Pump::queue`]) rather than dropped or run beside it — there is one agent,
/// so there is one turn at a time — except for `/stop`, which is the only thing
/// with anything to say to a turn already in flight.
async fn serve(mut gateway: Box<dyn Gateway>, config: Config, project_root: &Path) -> Result<()> {
    // The gateway is fully autonomous: there is no terminal, so run in
    // sovereign posture.
    let mut agent_config = config.clone();
    agent_config.mode = Mode::Sovereign;
    agent_config.max_steps = agent_config.max_steps.for_mode(Mode::Sovereign);

    // The gateway has no console at all: it is a daemon whose only user is on
    // the other end of a chat. So it declares nothing (`build_headless_agent`
    // below loads hooks through `crate::trust::Console::Unavailable`) and an
    // undecided project's hooks simply do not run. Say so once, out loud,
    // rather than leaving the operator to discover that `session_start` never
    // fired: this goes to stdout and to the log, which under systemd is the
    // same journal the refusals themselves land in.
    if let Some(why) = crate::trust::unattended_refusal(project_root) {
        println!("wizard: {why}");
        tracing::warn!("{why}");
    }

    // The MCP servers are connected *here*, once, and kept: `/reload` has to
    // re-register against the manager this process already runs. Letting the
    // agent builder connect its own would leave the gateway with no handle on
    // it, and a reload would start a second copy of every configured server —
    // each a real OS process that nothing later shuts down.
    let mut mcp = crate::agent::connect_mcp().await;
    let session = Session::create(&Config::sessions_dir()?).context("creating gateway session")?;
    // The session file has to exist before the agent is built, and the build
    // can fail — an unreachable provider is the ordinary case. Without this the
    // failed start left an empty session behind, and empty sessions are not
    // free: they are listed by `/resume` and by the window's sidebar forever,
    // so a bot that could not reach its provider three times leaves three rows
    // of nothing for somebody to scroll past.
    let session_path = session.path().to_path_buf();
    let mut agent =
        match build_headless_agent_for_session(&agent_config, project_root, session, Some(&mcp))
            .await
            .context("building gateway agent")
        {
            Ok(agent) => agent,
            Err(err) => {
                let _ = std::fs::remove_file(&session_path);
                return Err(err);
            }
        };
    // `plan_first = true`: the first turn plans read-only; the collector in
    // run_one_turn auto-approves the plan and includes it in the reply.
    if config.plan_first {
        agent.set_plan_mode(true);
    }
    // `omakase = true`: chef's choice, which implies plan mode (`set_omakase`
    // turns it on) and adds the prompt plus the `interview` behaviour.
    if config.omakase {
        agent.set_omakase(true);
    }

    // session_start hooks fire once for the whole gateway session. Guarded
    // because a hook is somebody else's code: a panic while firing it used to
    // end the process before it had listened for a single message, with the
    // operator left to work out why `wizard --gateway` exits instantly.
    if let Err(message) = without_dying(fire_session_hooks(&mut agent, true)).await {
        eprintln!("session_start hooks panicked ({message}); carrying on without them");
    }

    let allowed = config.gateway.allowed_chat_ids.clone();
    println!(
        "wizard gateway ({}) — listening for messages (Ctrl-C to stop)",
        gateway.label()
    );
    // An empty allow-list refuses everything (see `is_authorized`), which
    // looks like a dead bot from the outside. Say so up front, on stdout, so
    // the reason is in the journal next to the refusals themselves.
    if allowed.is_empty() {
        println!(
            "warning: gateway.allowed_chat_ids is empty, so every message will be refused. \
             Run `wizard gateway setup` to discover your chat id and add it, or copy the id \
             from a refusal below into allowed_chat_ids in ~/.wizard/config.toml."
        );
    } else {
        println!("allowed chat ids: {allowed:?}");
        // Given that an allowed message runs a sovereign turn with `execute`,
        // a group id is worth one loud line at the moment it is happening
        // rather than a sentence in a document nobody re-reads.
        if let Some(warning) = group_chat_warning(&allowed) {
            println!("warning: {warning}");
        }
    }

    // Publish the command list so the client offers it. Best-effort: a bot
    // whose menu did not update is worse than one that did, and better than one
    // that refused to start over it.
    let advertised = advertised_commands();
    match gateway.advertise_commands(&advertised).await {
        Ok(()) => println!("{} command(s) offered in chat", advertised.len()),
        Err(err) => eprintln!("could not publish the command list: {err:#}"),
    }

    // `/fusion` is a session toggle with nowhere on the agent to live, so the
    // serve loop holds it for as long as the agent it applies to.
    let mut fusion = false;

    let mut pump = Pump {
        allowed: &allowed,
        config: &config,
        shutdown: watch_for_interrupt(),
        queue: VecDeque::new(),
        next_poll: Instant::now(),
        attempt: 0,
        liveness: Liveness::new(Instant::now()),
    };

    loop {
        // One message at a time, in arrival order: there is a single agent, so
        // a turn and a command cannot overlap. With the backlog empty there is
        // nothing to do but wait for the transport.
        let Some(Queued { chat_id, what }) = pump.queue.pop_front() else {
            if !pump.poll_once(&mut *gateway).await {
                break;
            }
            continue;
        };
        match what {
            // Never queued: `Pump::absorb` answers a refusal where it sees it,
            // and drops the message there. Matched rather than wildcarded so a
            // future disposition has to be decided about.
            Disposition::Refused => {}
            Disposition::Command(line) => {
                println!("← [{chat_id}] {line}");
                // A `/stop` only reaches here when it came round with nothing
                // running — a stop aimed at a live turn is taken inside
                // `Pump::run_turn` and never queued. It has no table row, so
                // the shared dispatcher would call it an unknown command, which
                // is worse than an answer and worse than silence.
                if pump.answer_native_control(&*gateway, chat_id, &line).await {
                    continue;
                }
                if let Err(err) = gateway.typing(chat_id).await {
                    tracing::debug!("typing indicator failed: {err:#}");
                }
                let mut ctx = command::GatewayCtx {
                    config: &agent_config,
                    project_root,
                    mcp: &mut mcp,
                    fusion: &mut fusion,
                };
                // Guarded for the same reason the turn is: a command runs
                // arbitrary code (a `/reload` reconnecting MCP servers, a
                // `/cost` doing arithmetic on a provider's numbers) and a panic
                // in any of it used to take the whole gateway with it.
                let reply = match without_dying(command::apply_command(&mut agent, &mut ctx, &line))
                    .await
                {
                    Ok(reply) => reply,
                    Err(message) => format!("{COMMAND_PANICKED} {message}"),
                };
                // A command that genuinely had nothing to say sends
                // nothing; every one the dispatcher routes answers.
                if !reply.trim().is_empty() {
                    send_reply(&*gateway, chat_id, &cap_reply(reply)).await;
                }
            }
            Disposition::Turn {
                prompt,
                attachments,
            } => {
                println!("← [{chat_id}] {}", first_line(&prompt));
                if let Err(err) = gateway.typing(chat_id).await {
                    tracing::debug!("typing indicator failed: {err:#}");
                }
                // Cloned before the turn borrows the agent: this is what a
                // `/stop` polled *during* the turn raises, and what Ctrl-C
                // raises before the process leaves.
                let cancel = agent.cancel_handle();
                let turn = guarded_turn(run_one_turn(&mut agent, &prompt, &attachments));
                if !pump.run_turn(&mut *gateway, chat_id, &cancel, turn).await {
                    break;
                }
            }
        }
    }

    fire_session_hooks(&mut agent, false).await;
    Ok(())
}

/// Run `work`, turning a panic inside it into a value instead of into a dead
/// gateway.
///
/// The serve loop awaits a turn inline — it has to, because the agent is
/// borrowed mutably and deliberately never leaves [`serve`] — so a panic
/// anywhere underneath it unwinds straight out of `serve`, out of `main`, and
/// the process is gone. That is the worst possible shape of "it randomly
/// stops": a bot that was answering a minute ago, no reply, nothing in the chat
/// to say why, and under a supervisor a restart that starts a fresh session and
/// loses the conversation. And it is not exotic — an index into a model-supplied
/// list, an `expect` on a tool's output, a slice on a byte offset in text
/// somebody pasted, any of them will do it.
///
/// A panic is still a bug and the default hook still writes the message and
/// backtrace to stderr, which under systemd is the journal. This only decides
/// what the bug costs: one turn, or the whole service.
async fn without_dying<F>(work: F) -> std::result::Result<F::Output, String>
where
    F: std::future::Future,
{
    use futures_util::FutureExt;

    std::panic::AssertUnwindSafe(work)
        .catch_unwind()
        .await
        .map_err(|payload| panic_message(payload.as_ref()))
}

/// The message a caught panic carries.
///
/// `panic!("...")` and `unwrap`/`expect` all produce a `String` or a `&str`;
/// anything else is opaque, and says so rather than inventing a description.
/// This text goes into a chat message, so an honest "no message" is better than
/// a plausible-looking guess at what went wrong.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "panicked with no message".to_string()
}

/// One agent turn, with a panic inside it turned into the answer rather than
/// into the end of the service. See [`without_dying`].
///
/// Wrapping happens *here*, around the whole turn, rather than at
/// [`Pump::run_turn`]'s call site, so the guard is part of the future the pump
/// selects on and a test can hand the pump a turn that panics and watch what
/// the chat is told.
async fn guarded_turn<F>(turn: F) -> TurnOutcome
where
    F: std::future::Future<Output = TurnOutcome>,
{
    match without_dying(turn).await {
        Ok(outcome) => outcome,
        Err(message) => TurnOutcome {
            reply: format!("{TURN_PANICKED} {message}"),
            // Treated as spoken so the report survives a `/stop` that landed in
            // the same moment: "stopped the running turn" arriving alone, for a
            // turn that actually crashed, would send the reader looking for the
            // wrong thing.
            spoke: true,
        },
    }
}

/// Raise a flag on Ctrl-C, once, for the whole process.
///
/// A flag and not a future because the loop can no longer afford to build a
/// fresh `tokio::signal::ctrl_c()` per iteration: a signal that arrives while
/// no future is registered for it is missed, and now that a turn runs *inside*
/// the select the gap between two iterations is a whole turn's worth of message
/// handling. Registering once, up front, in a task whose only job is this,
/// means every select can simply watch a flag that stays raised — so Ctrl-C is
/// observed during a turn rather than after it.
///
/// [`CancelHandle`] is exactly that flag (raise once, wait on it from anywhere,
/// clone it freely), so it is reused rather than reinvented. This one is the
/// *process*'s, and is never handed to the agent.
fn watch_for_interrupt() -> CancelHandle {
    let shutdown = CancelHandle::default();
    let raise = shutdown.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => raise.cancel(),
            // Nothing to do but say so. Raising the flag here would quit a
            // gateway that was asked to do the opposite, and leaving it down
            // costs only the polite shutdown — SIGTERM still ends the process.
            Err(err) => eprintln!("could not listen for Ctrl-C ({err:#}); use SIGTERM to stop"),
        }
    });
    shutdown
}

/// One authorized message waiting for the agent.
#[derive(Debug)]
struct Queued {
    /// Where the answer goes.
    chat_id: i64,
    /// What [`disposition`] decided it was, decided at the moment it arrived so
    /// the allow-list is applied when the message is *received* rather than
    /// whenever the agent gets round to it.
    what: Disposition,
}

/// The half of the serve loop that faces the transport: the poll cadence, the
/// backlog of messages waiting for the one agent, and the one control that has
/// to be heard *while* a turn is running.
///
/// A struct and not another stretch of [`serve`] because `serve` cannot be
/// driven by a test — it needs a provider, a session, a network and a Ctrl-C to
/// stop — and everything interesting about `/stop` happens in here. Its methods
/// take a turn as a plain future, so a test can hand them one that ends when
/// the flag is raised and watch what the chat is told.
struct Pump<'a> {
    /// The allow-list, re-checked on every message that arrives (see
    /// [`disposition`]).
    allowed: &'a [i64],
    /// Config, for the retry ladder ([`poll_backoff`]).
    config: &'a Config,
    /// Raised when the process is asked to quit. Watched by every select.
    shutdown: CancelHandle,
    /// Authorized messages waiting for the agent, in arrival order.
    queue: VecDeque<Queued>,
    /// Earliest the transport may be polled again: where both the empty-poll
    /// floor and the failure ladder are spent.
    next_poll: Instant,
    /// Consecutive poll failures, for the backoff ladder.
    attempt: u32,
    /// What the gateway would say about itself if asked. See [`Liveness`].
    liveness: Liveness,
}

/// How often an otherwise silent gateway says it is still there.
///
/// The gateway's normal log is *empty*: no messages means no lines, for days.
/// That is indistinguishable from a wedged poll loop, a revoked token being
/// retried forever, or a process that exited and left the unit inactive — and
/// the operator only finds out which when they eventually message the bot and
/// get nothing back. Ten minutes is often enough that a gap in the journal is
/// obvious, and rare enough that a month of it is a few thousand lines.
const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(600);

/// What the gateway can honestly say about whether it is working.
///
/// Kept as facts rather than as a status string so the same numbers serve both
/// readers: the periodic line in the journal, for an operator who is looking at
/// the machine, and the `/ping` answer, for the far more common case of an
/// operator who has only the chat and a bot that has gone quiet.
#[derive(Debug)]
struct Liveness {
    /// When the serve loop began. Restarts are the thing an operator most often
    /// wants to rule in or out, and a gateway that reports an uptime of ninety
    /// seconds has answered the question on its own.
    started: Instant,
    /// The last time the transport answered *at all*, successfully or not.
    /// This, and not the last message, is what proves the loop is turning: a
    /// bot with no traffic for a week is healthy, and one whose last poll was
    /// an hour ago is not.
    last_poll: Instant,
    /// Messages accepted from the allow-list since start.
    served: u64,
    /// When the next heartbeat line is due.
    next_report: Instant,
}

impl Liveness {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            last_poll: now,
            served: 0,
            next_report: now + HEARTBEAT,
        }
    }

    /// Whether a heartbeat is due, advancing the schedule if so.
    ///
    /// Scheduled from `now` rather than from the previous due time so a gateway
    /// that spent an hour inside one turn emits one line when it comes back
    /// rather than six at once.
    fn heartbeat_due(&mut self, now: Instant) -> bool {
        if now < self.next_report {
            return false;
        }
        self.next_report = now + HEARTBEAT;
        true
    }
}

/// The one line that says what the gateway is doing, for the journal and for
/// `/ping`.
///
/// Pure, and taking every input explicitly, because the whole value of this
/// line is that it is *true*: a status report assembled from whatever happened
/// to be in scope is how "idle" comes to be printed by a loop that has been
/// failing to poll for an hour. Every claim here is a fact the pump already
/// tracks, and the test holds each of them to a distinguishable rendering.
fn liveness_line(
    state: &Liveness,
    now: Instant,
    busy: bool,
    queued: usize,
    failures: u32,
) -> String {
    let mut line = format!(
        "gateway alive — up {}, last poll {} ago, {} message(s) served, {}",
        compact_duration(now.saturating_duration_since(state.started)),
        compact_duration(now.saturating_duration_since(state.last_poll)),
        state.served,
        match busy {
            true => "a turn is running",
            false => "idle",
        }
    );
    if queued > 0 {
        line.push_str(&format!(", {queued} waiting"));
    }
    // The one thing that turns "alive" into "alive but not working", so it is
    // never omitted when it is true: a bot answering /ping while its polls have
    // failed forty times running is telling the operator something specific.
    if failures > 0 {
        line.push_str(&format!(
            ", {failures} consecutive poll failure(s) — see the log"
        ));
    }
    line
}

/// `3d4h`, `2h15m`, `90s`: the coarsest two units that still say something.
/// A status line is read at a glance and nobody needs the seconds in a week.
fn compact_duration(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    let (days, hours, minutes) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if days > 0 {
        return format!("{days}d{hours}h");
    }
    if hours > 0 {
        return format!("{hours}h{minutes}m");
    }
    if minutes > 0 {
        return format!("{minutes}m{}s", secs % 60);
    }
    format!("{secs}s")
}

impl Pump<'_> {
    /// Wait for the next batch and file it, racing the shutdown flag. `false`
    /// means the process was asked to quit.
    async fn poll_once(&mut self, gateway: &mut dyn Gateway) -> bool {
        let (started, result) = tokio::select! {
            biased;
            () = self.shutdown.cancelled() => {
                println!("\n[gateway stopped]");
                return false;
            }
            polled = poll_at(gateway, self.next_poll) => polled,
        };
        let batch = self.settle(started, result, false);
        self.absorb(&*gateway, batch, None).await;
        true
    }

    /// Run one turn to completion while the transport keeps being polled, and
    /// send whatever answers it. `false` means the process was asked to quit.
    ///
    /// This is the whole point of the restructure: for as long as `turn` is in
    /// flight the loop is still listening, so a `/stop` can arrive and be acted
    /// on. Every other message joins the queue — one agent, one turn — and is
    /// told so, since the alternative is a chat that looks like it swallowed
    /// what was typed into it.
    ///
    /// Cancelling leaves the agent reusable, which is what makes `/stop` a
    /// control rather than a way to break a session: the flag stops the turn at
    /// the next stream chunk or tool boundary, the turn returns through its
    /// ordinary path with whatever it had produced, and
    /// [`Agent::run_turn_with_images`] re-arms the flag at the start of the next
    /// one. Nothing is aborted, nothing is rebuilt, and the conversation keeps
    /// its history — exactly what the terminal's Ctrl-C does.
    async fn run_turn<F>(
        &mut self,
        gateway: &mut dyn Gateway,
        chat_id: i64,
        cancel: &CancelHandle,
        turn: F,
    ) -> bool
    where
        F: std::future::Future<Output = TurnOutcome>,
    {
        tokio::pin!(turn);
        // Cloned out so the select's borrow of it cannot fight the `&mut self`
        // the batch handling below needs.
        let shutdown = self.shutdown.clone();
        let mut stopper: Option<i64> = None;
        let mut outcome: Option<TurnOutcome> = None;
        let mut interrupted = false;

        loop {
            let polled = tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    interrupted = true;
                    break;
                }
                finished = &mut turn => {
                    outcome = Some(finished);
                    break;
                }
                polled = poll_at(gateway, self.next_poll) => polled,
            };
            // Deliberately out here rather than in the select's handler: an
            // `.await` inside a handler runs while the sibling futures sit
            // un-polled, so a refusal or a "queued" reply sent from in there
            // would stall the turn for as long as the send took. Out here the
            // turn is only paused for the message handling itself, which is the
            // shortest it can be without a second task.
            let batch = self.settle(polled.0, polled.1, true);
            if let Some(chat) = self.absorb(&*gateway, batch, Some(cancel)).await {
                stopper = Some(chat);
            }
        }

        if interrupted {
            // Ctrl-C is a *process* interrupt and must not wait out a turn. Ask
            // the turn to stop, give it the same grace the terminal gives it so
            // the session file and the tool results land, and leave without it
            // if it does not take the hint.
            println!("\n[gateway stopping — interrupting the running turn]");
            cancel.cancel();
            if tokio::time::timeout(INTERRUPT_GRACE, turn).await.is_err() {
                eprintln!("the turn did not stop within the grace period; leaving anyway");
            }
            return false;
        }

        let outcome = match outcome {
            Some(outcome) => outcome,
            // Unreachable: the loop leaves either with a finished turn or with
            // an interrupt, and the interrupt returned above.
            None => return true,
        };

        match stopper {
            None => send_reply(gateway, chat_id, &outcome.reply).await,
            Some(stopper) => {
                // Whatever the turn managed to say before it stopped is worth
                // sending; the fallback line it says when it said nothing at
                // all is not, because the stop notice says it better.
                if outcome.spoke {
                    send_reply(gateway, chat_id, &outcome.reply).await;
                }
                send_reply(gateway, stopper, STOPPED_REPLY).await;
                // A second allowed chat can stop this chat's turn. Whoever was
                // waiting on the answer is owed the reason it is not coming.
                if stopper != chat_id {
                    send_reply(gateway, chat_id, STOPPED_REPLY).await;
                }
            }
        }
        true
    }

    /// Fold one poll's result into the cadence and hand back the batch it
    /// yielded (empty on a failure, which is scheduled for a retry instead).
    fn settle(
        &mut self,
        started: Instant,
        result: Result<Vec<Inbound>>,
        busy: bool,
    ) -> Vec<Inbound> {
        // Either answer proves the loop is turning, which is the whole of what
        // `last_poll` claims: an error from the transport is a poll that
        // happened, and a gateway that has been failing to reach Telegram for
        // an hour is a very different thing from one that has been wedged for
        // an hour with no error at all.
        self.liveness.last_poll = Instant::now();
        if self.liveness.heartbeat_due(self.liveness.last_poll) {
            println!(
                "{}",
                liveness_line(
                    &self.liveness,
                    self.liveness.last_poll,
                    busy,
                    self.queue.len(),
                    self.attempt,
                )
            );
        }
        match result {
            Ok(batch) => {
                self.attempt = 0;
                // A floor under the success path, which had none.
                //
                // The error path is a careful jittered ladder that honours
                // `Retry-After`; a *successful* poll just looped straight back
                // round. `getUpdates` is asked to hang for `LONG_POLL_SECS`, so
                // an answer that arrives immediately and empty means something
                // upstream is not honouring that — a captive portal, an
                // intercepting proxy, Telegram during an incident — and the
                // gateway becomes a hot loop hammering the API at full rate on
                // a pinned core. That is how a bot earns a 429 and stops
                // answering the person it is for.
                if batch.is_empty() {
                    self.next_poll = started + MIN_POLL_INTERVAL;
                }
                batch
            }
            Err(err) => {
                let delay = poll_backoff(self.attempt, self.config, &err);
                eprintln!(
                    "gateway poll failed ({err:#}); retrying in {:.1}s",
                    delay.as_secs_f64()
                );
                self.attempt = self.attempt.saturating_add(1);
                self.next_poll = Instant::now() + delay;
                Vec::new()
            }
        }
    }

    /// File one polled batch: refuse what the allow-list refuses, act on a
    /// `/stop` aimed at the running turn, and queue the rest in arrival order.
    /// Returns the chat that asked for the stop, if one did.
    ///
    /// `running` is the in-flight turn's cancel handle, or `None` when the agent
    /// is idle, and it decides the two things that differ. A `/stop` is only a
    /// stop when there is something to stop — idle, it is queued like anything
    /// else, so the answer comes in the order the messages did rather than
    /// jumping the backlog. And only a message that actually has to wait is
    /// told that it is waiting.
    async fn absorb(
        &mut self,
        gateway: &dyn Gateway,
        batch: Vec<Inbound>,
        running: Option<&CancelHandle>,
    ) -> Option<i64> {
        let mut stopper = None;
        for message in batch {
            let chat_id = message.chat_id;
            // The allow-list first, before anything a message can ask for —
            // including the stop. A stranger's `/stop` costs exactly what a
            // stranger's anything costs: one vague refusal, and no reach into
            // this process at all.
            let what = match disposition(gateway, message, self.allowed).await {
                Disposition::Refused => continue,
                what => what,
            };
            // Answered here, from the poll loop, and never queued. A `/ping`
            // that waited its turn behind the backlog would be answering a
            // different question from the one that was asked: "are you still
            // reading messages" is only worth asking *while* something else is
            // in the way, and only an immediate answer means anything.
            if let Disposition::Command(line) = &what
                && native_control(line) == Some(PING)
            {
                println!("← [{chat_id}] {line}");
                let answer = liveness_line(
                    &self.liveness,
                    Instant::now(),
                    running.is_some(),
                    self.queue.len(),
                    self.attempt,
                );
                send_reply(gateway, chat_id, &answer).await;
                continue;
            }
            // Counted below the probe: `/ping` asks about the work, it is not
            // the work, and a counter that includes the times somebody checked
            // the counter is a counter nobody can reason about.
            self.liveness.served = self.liveness.served.saturating_add(1);
            if let (Disposition::Command(line), Some(cancel)) = (&what, running)
                && is_stop_command(line)
            {
                println!("← [{chat_id}] {line}");
                cancel.cancel();
                stopper = Some(chat_id);
                continue;
            }
            if running.is_some() {
                println!("← [{chat_id}] (queued) {}", first_line(what.echo()));
                send_reply(gateway, chat_id, QUEUED_REPLY).await;
            }
            self.queue.push_back(Queued { chat_id, what });
        }
        stopper
    }

    /// Answer a gateway-native control that reached the backlog, and say
    /// whether `line` was one.
    ///
    /// Only `/stop` normally gets here, and only when it came round with
    /// nothing running: a stop aimed at a live turn is taken inside
    /// [`Pump::absorb`] and never queued, and a `/ping` is answered there in
    /// every case. Silence would be the wrong answer to either — the chat
    /// cannot see whether a turn is running — and so would the shared
    /// dispatcher's, which has no row for a control this surface invented and
    /// would call it an unknown command.
    async fn answer_native_control(&self, gateway: &dyn Gateway, chat_id: i64, line: &str) -> bool {
        match native_control(line) {
            Some(STOP) => {
                send_reply(gateway, chat_id, NOTHING_TO_STOP).await;
                true
            }
            Some(PING) => {
                let answer = liveness_line(
                    &self.liveness,
                    Instant::now(),
                    false,
                    self.queue.len(),
                    self.attempt,
                );
                send_reply(gateway, chat_id, &answer).await;
                true
            }
            _ => false,
        }
    }
}

/// Poll the transport, but not before `not_before`.
///
/// The wait lives inside the future the loop selects on, and that is the point:
/// a sleep in the select's *handler* would hold up the turn running in the arm
/// beside it for its whole duration — a second of empty-poll floor, or a minute
/// of backoff ladder, of a turn not being polled. Returns the instant the poll
/// itself began, which is what the floor is measured from.
async fn poll_at(
    gateway: &mut dyn Gateway,
    not_before: Instant,
) -> (Instant, Result<Vec<Inbound>>) {
    let now = Instant::now();
    if not_before > now {
        tokio::time::sleep(not_before - now).await;
    }
    let started = Instant::now();
    let result = gateway.poll().await;
    (started, result)
}

/// The [`GATEWAY_NATIVE`] control `line` names, or `None` when it is an
/// ordinary command (or not a command at all).
///
/// Takes a line already normalized by [`command_line`] and re-strips the
/// `@botname` suffix anyway: this decides whether a turn keeps running and
/// whether a liveness probe is answered, neither of which is a place to rely on
/// a caller having done the right thing first. Derived from the table rather
/// than matched by hand so a control added to [`GATEWAY_NATIVE`] is routed
/// without anybody remembering to add it in a second place.
fn native_control(line: &str) -> Option<&'static str> {
    let rest = line.trim().strip_prefix('/')?;
    let head = rest.split_whitespace().next().unwrap_or("");
    let name = head.split('@').next().unwrap_or("");
    GATEWAY_NATIVE
        .iter()
        .map(|(native, _)| *native)
        .find(|native| *native == name)
}

/// Whether `line` is the `/stop` control.
fn is_stop_command(line: &str) -> bool {
    native_control(line) == Some(STOP)
}

/// How long to wait before polling again after `attempt` consecutive failures.
///
/// The shared LLM ladder rather than a bare `min(max, base * 2^n)`: several
/// gateway instances against one bot token (or one instance a platform is
/// rate-limiting) otherwise wake at the identical whole second and re-storm
/// the endpoint together, and a `Retry-After` the platform actually sent is
/// the one number worth honouring. Telegram answers a flood with exactly that.
fn poll_backoff(attempt: u32, config: &Config, err: &anyhow::Error) -> std::time::Duration {
    crate::llm::retry_delay(
        attempt,
        config.retry_base_secs,
        config.retry_max_secs,
        err.downcast_ref::<crate::llm::RetryAfter>()
            .map(|stated| stated.0),
    )
}

/// What [`serve`] does with one inbound message.
#[derive(Debug, PartialEq, Eq)]
enum Disposition {
    /// Not on the allow-list. The refusal has already been logged and answered
    /// by [`authorize_inbound`]; the message goes no further.
    Refused,
    /// A slash command, normalized to the line
    /// [`command::apply_command`] parses. No agent turn: the reply is what the
    /// one dispatcher has to say about it.
    Command(String),
    /// Run one agent turn on this prompt.
    Turn {
        prompt: String,
        attachments: Vec<std::path::PathBuf>,
    },
}

impl Disposition {
    /// The text a console line shows for this message.
    fn echo(&self) -> &str {
        match self {
            Disposition::Refused => "",
            Disposition::Command(line) => line,
            Disposition::Turn { prompt, .. } => prompt,
        }
    }
}

/// Decide what to do with one inbound message, allow-list first.
///
/// Split out of [`serve`]'s loop because `serve` cannot be driven by a test:
/// it needs a provider, a session, a network and a Ctrl-C to stop. With the
/// decision inside the loop, deleting the allow-list check left the whole
/// suite green while a stranger's message reached a sovereign agent turn. This
/// is the ordering that matters: refuse before the commands, before the
/// console line, before the typing indicator, before the turn.
async fn disposition(gateway: &dyn Gateway, message: Inbound, allowed: &[i64]) -> Disposition {
    if !authorize_inbound(gateway, &message, allowed).await {
        return Disposition::Refused;
    }
    // Attachments belong to a turn, never to a command: a photo captioned
    // `/status` is a photo, and answering it with a status report would drop
    // the file on the floor.
    if message.attachments.is_empty()
        && let Some(line) = command_line(&message.text)
    {
        return Disposition::Command(line);
    }
    Disposition::Turn {
        prompt: message.agent_prompt(),
        attachments: message.attachments,
    }
}

/// The slash-command line `text` is, or `None` when it is a prompt.
///
/// # Why this is not "starts with a slash"
///
/// A chat is a place people paste paths. `/etc/hosts`, `/usr/local/bin`, and a
/// bare `/` are messages *for the model*, and swallowing them into a command
/// executor would make the gateway answer "unknown command" to half of what a
/// developer types about their filesystem. So the first token decides, and it
/// decides against being a command by default:
///
/// * A name [`crate::commands::spec`] knows is a command. This is the table,
///   the same one [`advertised_commands`] publishes to Telegram, so the menu
///   and the router can never disagree about what exists.
/// * A control the gateway runs itself ([`GATEWAY_NATIVE`]) is a command too.
///   `/stop` is advertised, so it has to route as one — including the
///   `/stop@botname` a group chat's menu sends — or the one message that means
///   "interrupt" would become one more turn for the busy agent to run.
/// * A name the table does *not* know is a command only when it is the whole
///   message and it is shaped like one — one word of `[A-Za-z0-9_-]`, no path
///   separator, no dot. `/notacommand` is a typo worth answering ("unknown
///   command — try /help") rather than a sovereign agent turn spent on a
///   misspelling; `/etc/hosts` and `/deploy the release to prod` are not.
///
/// Telegram appends `@botname` to a command picked from the menu in a group
/// chat, so that suffix is stripped before the name is looked up — otherwise
/// every command in a group would be an unknown one.
fn command_line(text: &str) -> Option<String> {
    let text = text.trim();
    let rest = text.strip_prefix('/')?;
    let (head, args) = match rest.find(char::is_whitespace) {
        Some(at) => (&rest[..at], rest[at..].trim_start()),
        None => (rest, ""),
    };
    let name = head.split('@').next().unwrap_or(head);

    // A row of the table, a control this surface runs itself (`/stop`, which
    // has no row anywhere), or a lone word shaped like one (a typo worth
    // answering). Anything else — a path, a word with prose after it — is the
    // model's.
    let known = crate::commands::spec(name).is_some() || is_gateway_native(name);
    let typo = args.is_empty() && is_command_shaped(name);
    if !known && !typo {
        return None;
    }
    Some(match args.is_empty() {
        true => format!("/{name}"),
        false => format!("/{name} {args}"),
    })
}

/// Whether `name` could be a command word at all: one token of ASCII letters,
/// digits, `_` or `-`, within Telegram's 32-character limit for a command name.
/// A path segment fails on its separator, and a bare `/` fails on being empty.
fn is_command_shaped(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Send one reply, split into platform-sized chunks.
///
/// One failed chunk does not cancel the rest. `break` meant a single rejected
/// message silently swallowed the whole remainder of an answer; the reader is
/// better served by the parts that do send, and the operator by knowing how
/// many did not.
async fn send_reply(gateway: &dyn Gateway, chat_id: i64, reply: &str) {
    let chunks = split_message(reply, MAX_MESSAGE_CHARS);
    let total = chunks.len();
    let mut failed = 0usize;
    for chunk in chunks {
        if let Err(err) = gateway.send_rich(chat_id, &chunk).await {
            failed += 1;
            eprintln!("failed to send reply to {chat_id}: {err:#}");
        }
    }
    if failed == 0 {
        return;
    }
    eprintln!("{failed} of {total} reply chunk(s) did not reach {chat_id}");
    // Say so in the chat, not only in a journal nobody is reading. An answer
    // that arrives with a hole in it reads as the agent having thought
    // something odd; an answer that never arrives reads as a dead bot. Either
    // way the reader's next move depends on knowing it was the transport, and
    // this short line is far likelier to get through than the chunk that just
    // did not — it is small, it is plain, and it is one message rather than
    // six. If it fails too there is nothing further to try, and it must not
    // recurse into another round of failure reporting.
    let notice = format!(
        "({failed} of {total} parts of that reply could not be delivered — \
         Telegram refused them; the log has the reason)"
    );
    if let Err(err) = gateway.send(chat_id, &notice).await {
        eprintln!("could not tell {chat_id} that the reply was incomplete: {err:#}");
    }
}

/// Cap one reply at [`MAX_REPLY_CHARS`], saying so where it was cut, so a
/// runaway turn (or a very long report) cannot flood a chat.
fn cap_reply(reply: String) -> String {
    if reply.chars().count() <= MAX_REPLY_CHARS {
        return reply;
    }
    let truncated: String = reply.chars().take(MAX_REPLY_CHARS).collect();
    format!("{truncated}\n… (reply truncated)")
}

/// Fire the `session_start` (`start = true`) or `session_end` hooks and
/// print their activity — the gateway has no long-lived event channel.
async fn fire_session_hooks(agent: &mut Agent, start: bool) {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    if start {
        agent.fire_session_start(&tx).await;
    } else {
        agent.fire_session_end(Some(&tx)).await;
    }
    drop(tx);
    while let Some(event) = rx.recv().await {
        if let AgentEvent::HookFired {
            event,
            command,
            outcome,
        } = event
        {
            println!("hook {event}: {outcome} ({command})");
        }
    }
}

/// Folds one turn's events into the single chat message that answers it.
///
/// A struct rather than a closure inside the collector loop so the folding can
/// be tested without a provider, a network and a chat platform: the retry rule
/// below is exactly the kind of thing that stays broken for months when the
/// only way to see it is a flaky model mid-conversation.
#[derive(Default)]
struct ReplyCollector {
    /// The assistant text, concatenated in arrival order.
    reply: String,
    /// Length of `reply` at the last completed step. Everything before it is
    /// committed: it is in the model's history and no retry takes it back.
    committed: usize,
    /// Tools the turn ran, for the "(done; ran tools: …)" fallback when the
    /// model said nothing.
    tools: Vec<String>,
    /// Last error surfaced, for the "no reply" fallback.
    error: Option<String>,
}

impl ReplyCollector {
    fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(delta) => self.reply.push_str(&delta),
            AgentEvent::ToolStarted { name, .. } => self.tools.push(name),
            AgentEvent::Error(message) => self.error = Some(message),
            AgentEvent::StepCompleted { .. } => self.committed = self.reply.len(),
            AgentEvent::StreamRetrying => {
                // The dead attempt's text never entered history and the retry
                // re-generates it from the start of this step, so keeping it
                // would send the same paragraph to the chat twice. Only this
                // step's text goes: earlier steps are already committed.
                self.reply.truncate(self.committed);
            }
            AgentEvent::PlanReady { plan, gate } => {
                // No human reviews a gateway plan: include it in the
                // reply and approve so the turn proceeds to execute.
                self.reply
                    .push_str(&format!("[plan]\n{plan}\n[plan auto-approved]\n\n"));
                gate.answer(PlanVerdict::approve());
                // The plan is part of the reply, so it is committed text: a
                // retry of the step that follows must not swallow it.
                self.committed = self.reply.len();
            }
            AgentEvent::Interview { gate, .. } => {
                // Nobody is at the other end to answer questions between two
                // chat messages; declining lets the model plan on its own
                // rather than parking the turn inside the tool.
                gate.decline();
            }
            // A chat message is the turn's answer and nothing else: progress
            // notices (compaction), thinking, todos, background bookkeeping and
            // subagent chatter are all noise in a phone notification. Listed
            // one by one so a new event has to be decided about here rather
            // than silently dropped by a wildcard.
            AgentEvent::ThinkingDelta(_)
            | AgentEvent::ToolFinished { .. }
            | AgentEvent::Images { .. }
            | AgentEvent::Notice(_)
            | AgentEvent::HookFired { .. }
            | AgentEvent::OmakaseProceeding { .. }
            | AgentEvent::Usage { .. }
            | AgentEvent::ContextSize { .. }
            | AgentEvent::UltraGuidance { .. }
            | AgentEvent::TodoUpdated(_)
            | AgentEvent::TaskStarted { .. }
            | AgentEvent::TaskFinished { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentFinished { .. }
            | AgentEvent::SubagentRunStarted { .. }
            | AgentEvent::SubagentRunText { .. }
            | AgentEvent::SubagentRunToolStarted { .. }
            | AgentEvent::SubagentRunToolFinished { .. }
            | AgentEvent::SubagentRunImages { .. }
            | AgentEvent::SubagentRunStep { .. }
            | AgentEvent::SubagentRunDone { .. }
            | AgentEvent::CommandRequested(_)
            | AgentEvent::Done { .. } => {}
            // A shell command's console. The gateway answers HTTP requests
            // with no interactive user behind them, so its tool context leaves
            // `ConsoleAccess` at `None` and no command opens one.
            AgentEvent::ConsoleOpened { .. }
            | AgentEvent::ConsoleWaiting { .. }
            | AgentEvent::ConsoleOutput { .. }
            | AgentEvent::ConsoleClosed { .. } => {}
        }
    }
}

/// What one finished turn has to say.
#[derive(Debug)]
struct TurnOutcome {
    /// The message that answers it: the model's text, or an honest fallback
    /// when it produced none.
    reply: String,
    /// Whether the model actually said something, as opposed to the fallback.
    ///
    /// It matters exactly once: a turn stopped by `/stop` before it spoke is
    /// answered by the stop notice alone, because "(done, no reply)" arriving
    /// beside "stopped the running turn" reads as two answers disagreeing about
    /// what happened.
    spoke: bool,
}

/// Run exactly one agent turn against `text` (with optional image attachments)
/// and collect the reply: stream the turn while draining its [`AgentEvent`]
/// channel, concatenating text deltas and noting tool activity. The reply is
/// capped at [`MAX_REPLY_CHARS`].
async fn run_one_turn(agent: &mut Agent, text: &str, images: &[std::path::PathBuf]) -> TurnOutcome {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);

    // Drain events concurrently with the turn: the turn borrows the agent
    // mutably and owns the sender (dropped on completion, which ends the
    // collector); the collector owns the receiver — disjoint borrows.
    let collector = async move {
        let mut reply = ReplyCollector::default();
        while let Some(event) = rx.recv().await {
            reply.handle(event);
        }
        reply
    };

    let images = images.to_vec();
    let (
        done,
        ReplyCollector {
            mut reply,
            tools,
            error,
            ..
        },
    ) = tokio::join!(agent.run_turn_with_images(text, images, tx), collector);

    let reply_trimmed = reply.trim();
    let spoke = !reply_trimmed.is_empty();
    if spoke {
        reply = reply_trimmed.to_string();
    } else {
        // The turn's own `Err` is consulted as well as the error *event*, and
        // it is the last word before the bland fallbacks. The two normally
        // agree — `run_turn_with_images` emits the event on its way out — but
        // "normally" is doing real work there: the event channel is bounded and
        // an emit that failed, or a failure raised before the channel exists,
        // would leave the chat reading "(done, no reply)" for a turn that
        // actually collapsed. A bot that answers "done" to everything is the
        // hardest version of this bug to notice.
        reply = match (error, done.err(), tools.is_empty()) {
            (Some(message), _, _) => format!("(no reply — {message})"),
            (None, Some(err), _) => format!("(no reply — the turn failed: {err:#})"),
            (None, None, false) => format!("(done; ran tools: {})", tools.join(", ")),
            (None, None, true) => "(done, no reply)".to_string(),
        };
    }

    TurnOutcome {
        reply: cap_reply(reply),
        spoke,
    }
}

/// First line of `text`, for terse console logging of inbound messages.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {

    /// `omakase = true` reaches the agent on the gateway surface.
    ///
    /// Startup here honoured `plan_first` only, so a config that asked for
    /// chef's choice got plain plan mode in every Telegram session: no omakase
    /// system prompt, no chef's-choice `interview` behaviour, no warning.
    /// Grep, like the copies of this test in `headless.rs` and `app/tests.rs`:
    /// the defect is the *absence* of a call.
    #[test]
    fn the_gateway_runner_applies_omakase_and_not_only_plan_mode() {
        let source = include_str!("mod.rs");
        let (production, _) = source
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("this module ends with its test module");
        assert!(
            production.contains("config.plan_first"),
            "plan_first is still read here"
        );
        assert!(
            production.contains("agent.set_omakase("),
            "omakase must reach the agent on this surface too, not just plan mode"
        );
    }

    /// The chat's command menu is the command table, minus what it cannot run,
    /// plus the controls this surface runs itself.
    ///
    /// Derived rather than listed, so a command added to the table reaches
    /// Telegram without anybody remembering to add it twice, and — more
    /// importantly — the menu can never offer something the gateway would
    /// answer "not available in this chat". An autocomplete entry that refuses
    /// is worse than no entry.
    ///
    /// The one thing the table cannot supply is `/stop`: the terminal's
    /// equivalent is Ctrl-C and a chat has no keyboard, so it is declared in
    /// [`GATEWAY_NATIVE`] instead. Every advertised entry must therefore be one
    /// or the other — a row the gateway runs, or a declared native control —
    /// and never both, since a control that shadowed a row would quietly take
    /// that command away from the chat.
    #[test]
    fn the_advertised_menu_matches_what_the_gateway_will_actually_run() {
        use crate::commands::surface::Surface;

        let advertised = advertised_commands();
        assert!(!advertised.is_empty(), "the gateway runs some commands");

        let offered: std::collections::HashSet<&str> =
            advertised.iter().map(|c| c.name.as_str()).collect();

        for spec in crate::commands::COMMANDS {
            let runs = spec.execution(Surface::Gateway) != Execution::Unavailable;
            let listed = offered.contains(spec.name);
            assert_eq!(
                runs, listed,
                "/{} runs={runs} but listed={listed} — the menu and the table disagree",
                spec.name
            );
        }

        // And nothing is offered that is neither.
        for command in &advertised {
            let row = crate::commands::spec(&command.name)
                .is_some_and(|spec| spec.execution(Surface::Gateway) != Execution::Unavailable);
            let native = is_gateway_native(&command.name);
            assert!(
                row ^ native,
                "/{} is advertised but is row={row} native={native} — every entry must be \
                 exactly one of the two",
                command.name
            );
        }
        assert!(
            offered.contains(STOP),
            "a chat with no keyboard needs /stop in the menu"
        );

        // Telegram's own limits, which are the tightest and so the ones worth
        // meeting: `[a-z0-9_]{1,32}` names, 1..=256 character descriptions.
        // One bad entry makes `setMyCommands` reject the whole batch, which
        // would cost the entire menu rather than the one command.
        for command in &advertised {
            assert!(
                (1..=32).contains(&command.name.len())
                    && command
                        .name
                        .chars()
                        .all(|ch| { ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' }),
                "{:?} is not a name Telegram accepts",
                command.name
            );
            assert!(
                (1..=256).contains(&command.description.chars().count()),
                "/{} has a description Telegram would refuse ({} chars)",
                command.name,
                command.description.chars().count()
            );
        }

        // The ones a chat plainly should have.
        for name in ["status", "cost", "clear", "help", "plan"] {
            assert!(offered.contains(name), "/{name} should be offered");
        }
        // And the ones it plainly should not: no screen to put them on.
        for name in ["vim", "ui", "dashboard"] {
            assert!(!offered.contains(name), "/{name} has nowhere to land here");
        }
    }

    /// Chunks are budgeted in UTF-16 code units, which is what Telegram counts.
    ///
    /// Counting `chars()` is the same number only for the basic plane. Every
    /// emoji is two units, so a 4,000-character chunk carrying more than 96 of
    /// them exceeded the 4,096-unit cap named in `MAX_MESSAGE_CHARS`'s own
    /// comment; `sendMessage` rejected it and the send loop used to `break`, so
    /// an ordinary emoji-heavy answer arrived truncated with nothing to say so.
    /// The pre-existing test used `é` — one unit — and could not see it.
    #[test]
    fn chunks_are_measured_the_way_telegram_measures_them() {
        let emoji = "🙂".repeat(4000);
        for chunk in split_message(&emoji, MAX_MESSAGE_CHARS) {
            assert!(
                chunk.encode_utf16().count() <= MAX_MESSAGE_CHARS,
                "a chunk was {} UTF-16 units, over the {MAX_MESSAGE_CHARS} budget",
                chunk.encode_utf16().count()
            );
        }
        // Nothing is lost in the splitting.
        assert_eq!(split_message(&emoji, MAX_MESSAGE_CHARS).concat(), emoji);

        // A single unbroken line of astral characters is hard-split, and still
        // never exceeds the budget.
        let one_word = "𝔘".repeat(9000);
        let chunks = split_message(&one_word, MAX_MESSAGE_CHARS);
        assert!(
            chunks.len() >= 4,
            "expected several chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            assert!(chunk.encode_utf16().count() <= MAX_MESSAGE_CHARS);
        }
        assert_eq!(chunks.concat(), one_word);

        // And plain text is unchanged by the switch.
        let plain = "hello\nworld\n";
        assert_eq!(
            split_message(plain, MAX_MESSAGE_CHARS),
            vec![plain.to_string()]
        );
    }
    use std::sync::Mutex;

    use super::*;

    /// A transport that records everything the serve loop asks it to do, so a
    /// refusal can be inspected for what it did *not* send, and that can be
    /// scripted with the batches its polls hand back.
    #[derive(Default)]
    struct RecordingGateway {
        sends: Mutex<Vec<(i64, String)>>,
        typings: Mutex<Vec<i64>>,
        /// What `poll` answers, in order. Empty batches once it runs out.
        batches: Mutex<VecDeque<Vec<Inbound>>>,
        /// Fired the first time a poll finds the script spent, so a fake turn
        /// can end exactly when the messages under test have been delivered —
        /// no sleeps, no timing, and the same order every run.
        drained: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    impl RecordingGateway {
        fn sends(&self) -> Vec<(i64, String)> {
            self.sends.lock().expect("sends lock").clone()
        }

        /// Everything sent to `chat_id`, in order.
        fn sends_to(&self, chat_id: i64) -> Vec<String> {
            self.sends()
                .into_iter()
                .filter(|(chat, _)| *chat == chat_id)
                .map(|(_, text)| text)
                .collect()
        }

        /// A transport that hands `batches` back one poll at a time, and a
        /// receiver that resolves once they have all been delivered.
        fn scripted(batches: Vec<Vec<Inbound>>) -> (Self, tokio::sync::oneshot::Receiver<()>) {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let gateway = Self {
                batches: Mutex::new(batches.into()),
                drained: Mutex::new(Some(tx)),
                ..Self::default()
            };
            (gateway, rx)
        }
    }

    #[async_trait]
    impl Gateway for RecordingGateway {
        fn label(&self) -> &str {
            "recording"
        }

        async fn poll(&mut self) -> Result<Vec<Inbound>> {
            if let Some(batch) = self.batches.lock().expect("batches lock").pop_front() {
                return Ok(batch);
            }
            if let Some(done) = self.drained.lock().expect("drained lock").take() {
                let _ = done.send(());
            }
            Ok(Vec::new())
        }

        async fn send(&self, chat_id: i64, text: &str) -> Result<()> {
            self.sends
                .lock()
                .expect("sends lock")
                .push((chat_id, text.to_string()));
            Ok(())
        }

        async fn typing(&self, chat_id: i64) -> Result<()> {
            self.typings.lock().expect("typings lock").push(chat_id);
            Ok(())
        }
    }

    /// Adversarial: the refusal path itself, not just the predicate. A
    /// stranger's message must produce exactly one deliberately vague reply
    /// and nothing else: no typing indicator, no echo of what they sent, no
    /// second message, and (asserted in the transport's own test) no download.
    #[tokio::test]
    async fn an_unauthorized_chat_gets_one_vague_refusal_and_nothing_else() {
        let gateway = RecordingGateway::default();
        // Exactly the shipped default: an empty allow-list.
        let allowed = crate::config::GatewayConfig::default().allowed_chat_ids;
        let message = Inbound {
            chat_id: 999,
            text: "delete every file in the project".to_string(),
            attachments: vec![std::path::PathBuf::from("/tmp/payload.jpg")],
        };

        assert!(
            !authorize_inbound(&gateway, &message, &allowed).await,
            "an empty allow-list must refuse"
        );

        let sends = gateway.sends();
        assert_eq!(
            sends,
            vec![(999, UNAUTHORIZED_REPLY.to_string())],
            "exactly one reply, and it is the vague one"
        );
        assert!(
            !sends[0].1.contains("delete every file"),
            "the refusal must not echo the sender back at them: {sends:?}"
        );
        assert!(
            !sends[0].1.contains("allowed_chat_ids") && !sends[0].1.contains("999"),
            "the refusal must not leak the allow-list or confirm the id: {sends:?}"
        );
        assert!(
            gateway.typings.lock().expect("typings lock").is_empty(),
            "a refused chat gets no typing indicator either"
        );

        // Defence in depth: an `Inbound::refused` handed up by a transport
        // that already refused is refused again here, not silently allowed.
        assert!(!authorize_inbound(&gateway, &Inbound::refused(999), &[42]).await);
        assert_eq!(gateway.sends().len(), 2, "one refusal per refused message");
    }

    #[tokio::test]
    async fn an_authorized_chat_passes_the_gate_without_a_reply() {
        let gateway = RecordingGateway::default();
        assert!(authorize_inbound(&gateway, &Inbound::text(42, "hi"), &[42]).await);
        assert!(
            gateway.sends().is_empty(),
            "the gate itself says nothing to an allowed chat"
        );
    }

    /// Adversarial: the serve loop's own ordering, which nothing used to
    /// cover. Every test above drives `authorize_inbound` directly, so the
    /// call site could be deleted and the suite would stay green while a
    /// stranger's message drove a sovereign agent turn.
    #[tokio::test]
    async fn the_allow_list_is_consulted_before_anything_a_message_can_ask_for() {
        let gateway = RecordingGateway::default();

        // Not on the list: refused, whatever the message says. `/plan` is the
        // sharp case: it switches the read-only phase off for every later
        // turn, so a stranger toggling it is worse than a stranger chatting.
        for text in ["/plan", "/omakase", "delete every file in the project"] {
            assert_eq!(
                disposition(&gateway, Inbound::text(999, text), &[42]).await,
                Disposition::Refused,
                "{text}"
            );
        }
        // A refusal per refused message, and nothing else: no typing hint, no
        // echo of what was sent.
        assert_eq!(gateway.sends().len(), 3);
        assert!(
            gateway
                .sends()
                .iter()
                .all(|(chat, text)| *chat == 999 && text == UNAUTHORIZED_REPLY)
        );
        assert!(gateway.typings.lock().expect("typings lock").is_empty());

        // On the list: a command is recognised and everything else becomes a
        // turn, with the attachment paths carried into it.
        assert_eq!(
            disposition(&gateway, Inbound::text(42, " /plan \n"), &[42]).await,
            Disposition::Command("/plan".to_string())
        );
        assert_eq!(
            disposition(&gateway, Inbound::text(42, "/omakase"), &[42]).await,
            Disposition::Command("/omakase".to_string())
        );
        let photo = Inbound {
            chat_id: 42,
            text: "look".to_string(),
            attachments: vec![std::path::PathBuf::from("/tmp/a.jpg")],
        };
        match disposition(&gateway, photo, &[42]).await {
            Disposition::Turn {
                prompt,
                attachments,
            } => {
                assert!(prompt.contains("[attached: /tmp/a.jpg]"), "{prompt}");
                assert_eq!(attachments, vec![std::path::PathBuf::from("/tmp/a.jpg")]);
            }
            other => panic!("an allowed chat's message runs a turn, got {other:?}"),
        }
        assert_eq!(
            gateway.sends().len(),
            3,
            "an allowed chat is answered by the turn, not by the gate"
        );
    }

    /// Every command the menu offers is one the router will actually recognise
    /// as a command. The old router matched exactly `/plan` and `/omakase`, so
    /// the other twenty-four advertised commands asked the *model* about their
    /// own text — a menu entry that costs a sovereign turn and answers with a
    /// guess.
    #[test]
    fn every_advertised_command_is_routed_as_a_command() {
        for advertised in advertised_commands() {
            let line = format!("/{}", advertised.name);
            assert_eq!(
                command_line(&line),
                Some(line.clone()),
                "{line} is offered in the menu but would have become a turn"
            );
            // And the way Telegram sends it from a group chat's menu.
            assert_eq!(
                command_line(&format!("/{}@wizardbot", advertised.name)),
                Some(line.clone()),
                "{line} picked from a group's menu carries an @suffix"
            );
        }
    }

    /// The line between a command and a prompt, which is the whole risk in
    /// routing on a leading slash: a chat is where people paste paths.
    #[test]
    fn a_path_is_a_prompt_and_a_command_is_a_command() {
        // Commands, with their arguments preserved verbatim for the one parser.
        for (text, expected) in [
            ("/status", "/status"),
            ("  /help \n", "/help"),
            ("/model gpt-5", "/model gpt-5"),
            ("/btw   why did that fail?", "/btw why did that fail?"),
            ("/goal ship the release", "/goal ship the release"),
            ("/server@wizardbot start", "/server start"),
            // Unknown, but shaped like a command and alone on the line: a typo
            // worth answering rather than a turn spent on a misspelling.
            ("/notacommand", "/notacommand"),
        ] {
            assert_eq!(
                command_line(text),
                Some(expected.to_string()),
                "{text:?} should route as a command"
            );
        }

        // Prompts. A path, a bare slash, and anything with prose after a word
        // the table does not have.
        for text in [
            "/etc/hosts",
            "/",
            "  /  ",
            "/usr/local/bin/wizard --help",
            "/tmp/build.log has the error",
            "/deploy the release to prod",
            "//",
            "/@someone",
            "hello",
            "",
        ] {
            assert_eq!(
                command_line(text),
                None,
                "{text:?} must reach the model as a prompt"
            );
        }
    }

    /// A photo captioned `/status` is a photo. Routing it as a command would
    /// drop the downloaded file on the floor.
    #[tokio::test]
    async fn an_attachment_is_always_a_turn_whatever_the_caption_says() {
        let gateway = RecordingGateway::default();
        let photo = Inbound {
            chat_id: 42,
            text: "/status".to_string(),
            attachments: vec![std::path::PathBuf::from("/tmp/a.jpg")],
        };
        assert!(matches!(
            disposition(&gateway, photo, &[42]).await,
            Disposition::Turn { .. }
        ));
    }

    /// A pump with an empty backlog and nobody interrupting, ready to be
    /// handed a turn.
    fn test_pump<'a>(allowed: &'a [i64], config: &'a Config) -> Pump<'a> {
        Pump {
            allowed,
            config,
            shutdown: CancelHandle::default(),
            queue: VecDeque::new(),
            next_poll: Instant::now(),
            attempt: 0,
            liveness: Liveness::new(Instant::now()),
        }
    }

    /// A stand-in for a real turn: it ends when the cancel flag is raised,
    /// which is what the agent's run loop does, and answers with what it had
    /// produced by then.
    async fn turn_stopping_on(cancel: CancelHandle, reply: &'static str) -> TurnOutcome {
        cancel.cancelled().await;
        TurnOutcome {
            reply: reply.to_string(),
            spoke: !reply.is_empty(),
        }
    }

    /// The feature: a `/stop` sent while the agent is working reaches the turn
    /// *during* it, and the chat is told.
    ///
    /// This is what the old loop made impossible. It awaited the turn inline,
    /// so nothing polled until the turn was over — a `/stop` could not even be
    /// received, and by the time it was there was nothing left to stop.
    #[tokio::test]
    async fn a_stop_during_a_turn_cancels_it_and_says_so() {
        let config = Config::default();
        let allowed = [42];
        let mut pump = test_pump(&allowed, &config);
        let (mut gateway, _drained) =
            RecordingGateway::scripted(vec![vec![Inbound::text(42, "/stop")]]);

        let cancel = CancelHandle::default();
        let turn = turn_stopping_on(cancel.clone(), "half an answer");
        assert!(
            pump.run_turn(&mut gateway, 42, &cancel, turn).await,
            "stopping a turn is not stopping the gateway"
        );

        assert!(
            cancel.is_cancelled(),
            "the /stop must reach the turn while it is still running"
        );
        // What it managed to say, then the confirmation — in that order, so the
        // chat reads as an answer that was cut short rather than as a refusal.
        assert_eq!(
            gateway.sends_to(42),
            vec!["half an answer".to_string(), STOPPED_REPLY.to_string()]
        );
        assert!(
            STOPPED_REPLY.contains("conversation is intact"),
            "a stopped turn is not a broken session, and the chat is told so"
        );
        assert!(pump.queue.is_empty(), "a stop is acted on, never queued");
    }

    /// A `/stop` with nothing to stop answers plainly. Silence would be wrong
    /// (the chat cannot see whether a turn is running) and so would the shared
    /// dispatcher's "unknown command", which is what `/stop` would get from it:
    /// the control has no row in the table.
    #[tokio::test]
    async fn a_stop_with_nothing_running_says_so_plainly() {
        let gateway = RecordingGateway::default();
        let config = Config::default();
        let allowed = [42];
        let pump = test_pump(&allowed, &config);

        assert!(pump.answer_native_control(&gateway, 42, "/stop").await);
        assert_eq!(gateway.sends(), vec![(42, NOTHING_TO_STOP.to_string())]);

        // The form a group chat's menu sends, and an argument nobody asked for.
        assert!(
            pump.answer_native_control(&gateway, 42, "/stop@wizardbot")
                .await
        );
        assert!(pump.answer_native_control(&gateway, 42, "/stop now").await);

        // Everything else is left to the dispatcher, untouched.
        for line in ["/status", "/stopwatch", "/plan", "stop"] {
            assert!(
                !pump.answer_native_control(&gateway, 42, line).await,
                "{line} is not a control this surface answers"
            );
        }
        assert_eq!(gateway.sends().len(), 3, "one answer per stop, and no more");
    }

    /// One agent means one turn: a message that lands mid-turn waits, in
    /// arrival order, and is told that it is waiting rather than vanishing.
    #[tokio::test]
    async fn a_message_that_arrives_mid_turn_is_queued_for_after_it() {
        let config = Config::default();
        let allowed = [42];
        let mut pump = test_pump(&allowed, &config);
        let (mut gateway, drained) = RecordingGateway::scripted(vec![vec![
            Inbound::text(42, "and another thing"),
            Inbound::text(42, "/status"),
        ]]);

        let cancel = CancelHandle::default();
        // A turn that ends of its own accord once the batch has been delivered.
        let turn = async move {
            let _ = drained.await;
            TurnOutcome {
                reply: "the first answer".to_string(),
                spoke: true,
            }
        };
        assert!(pump.run_turn(&mut gateway, 42, &cancel, turn).await);

        assert!(
            !cancel.is_cancelled(),
            "an ordinary message must not interrupt the turn it arrived during"
        );
        // Both waiting, in the order they were sent, for `serve` to pop.
        assert_eq!(pump.queue.len(), 2);
        match &pump.queue[0].what {
            Disposition::Turn { prompt, .. } => assert_eq!(prompt, "and another thing"),
            other => panic!("the first message should still be a turn, got {other:?}"),
        }
        assert_eq!(
            pump.queue[1].what,
            Disposition::Command("/status".to_string())
        );
        // Each was told, and the turn's own answer still arrived last.
        assert_eq!(
            gateway.sends_to(42),
            vec![
                QUEUED_REPLY.to_string(),
                QUEUED_REPLY.to_string(),
                "the first answer".to_string(),
            ]
        );
        assert!(
            QUEUED_REPLY.contains("/stop"),
            "the one moment /stop is worth knowing about is while something is queued behind it"
        );
    }

    /// Adversarial: `/stop` is a control over a sovereign agent, so it is
    /// allow-listed like everything else — and costs a stranger exactly what
    /// any other message costs them, which is one vague refusal.
    #[tokio::test]
    async fn a_strangers_stop_is_refused_and_stops_nothing() {
        let config = Config::default();
        let allowed = [42];
        let mut pump = test_pump(&allowed, &config);
        let (mut gateway, drained) =
            RecordingGateway::scripted(vec![vec![Inbound::text(999, "/stop")]]);

        let cancel = CancelHandle::default();
        let turn = async move {
            let _ = drained.await;
            TurnOutcome {
                reply: "finished".to_string(),
                spoke: true,
            }
        };
        assert!(pump.run_turn(&mut gateway, 42, &cancel, turn).await);

        assert!(
            !cancel.is_cancelled(),
            "a chat that is not on the list cannot touch the running turn"
        );
        assert_eq!(
            gateway.sends_to(999),
            vec![UNAUTHORIZED_REPLY.to_string()],
            "one vague refusal, and nothing that says whether anything was running"
        );
        assert!(
            pump.queue.is_empty(),
            "and nothing of theirs is kept for later either"
        );
        assert!(gateway.typings.lock().expect("typings lock").is_empty());
        // The turn it could not stop finished and answered as usual.
        assert_eq!(gateway.sends_to(42), vec!["finished".to_string()]);
    }

    /// Ctrl-C still stops the *process*, and is observed during a turn rather
    /// than after it — which is the thing the restructure could most easily
    /// have broken, since the turn now runs inside the select that used to
    /// race the signal alone.
    #[tokio::test]
    async fn ctrl_c_during_a_turn_is_observed_without_waiting_for_the_turn() {
        let config = Config::default();
        let allowed = [42];
        let mut pump = test_pump(&allowed, &config);
        let (mut gateway, _drained) = RecordingGateway::scripted(Vec::new());

        // Exactly what the signal watcher does.
        pump.shutdown.cancel();

        let cancel = CancelHandle::default();
        let turn = turn_stopping_on(cancel.clone(), "half an answer");
        assert!(
            !pump.run_turn(&mut gateway, 42, &cancel, turn).await,
            "the serve loop is told to stop"
        );
        assert!(
            cancel.is_cancelled(),
            "and the turn is asked to wind down rather than being left running"
        );
        assert!(
            gateway.sends().is_empty(),
            "a process on its way out does not chat: {:?}",
            gateway.sends()
        );
    }

    /// A turn that panics is the sharpest version of "it randomly stops": the
    /// agent is borrowed by `serve` and the turn is awaited inline, so before
    /// this guard the unwind went straight out of the serve loop, out of
    /// `main`, and the process was gone. No reply, nothing in the chat, and
    /// under a supervisor a restart that starts a fresh session and loses the
    /// conversation.
    ///
    /// The bar is that the chat is *told*, that it is told what happened, and
    /// that the loop is still serving afterwards.
    #[tokio::test]
    async fn a_turn_that_panics_answers_the_chat_and_leaves_the_gateway_serving() {
        /// Stands in for a tool that indexes past the end of a model-supplied
        /// list, which is the ordinary way this happens.
        async fn exploding_turn() -> TurnOutcome {
            panic!("a tool indexed past the end");
        }

        let config = Config::default();
        let allowed = [42];
        let mut pump = test_pump(&allowed, &config);
        let (mut gateway, _drained) = RecordingGateway::scripted(Vec::new());

        let cancel = CancelHandle::default();
        assert!(
            pump.run_turn(&mut gateway, 42, &cancel, guarded_turn(exploding_turn()))
                .await,
            "a panicking turn must not stop the gateway"
        );

        let sends = gateway.sends_to(42);
        assert_eq!(sends.len(), 1, "exactly one answer: {sends:?}");
        assert!(
            sends[0].contains("a tool indexed past the end"),
            "the chat is told what actually failed: {}",
            sends[0]
        );
        assert!(
            sends[0].contains("still listening"),
            "and that the bot is not dead, which is the other thing silence \
             would have meant: {}",
            sends[0]
        );
    }

    /// The guard itself: a panic becomes a value, and its message survives in
    /// both the shapes `panic!` produces (a literal is a `&str`, a formatted
    /// one is a `String`). An unwind that came back with nothing to say would
    /// leave the chat a report that named no cause at all.
    #[tokio::test]
    async fn a_panic_becomes_a_value_carrying_whatever_it_said() {
        assert_eq!(
            without_dying(async { 7 }).await,
            Ok(7),
            "a future that does not panic is untouched"
        );
        assert_eq!(
            without_dying(async { panic!("a literal") }).await,
            Err::<(), _>("a literal".to_string())
        );
        let formatted = 4;
        assert_eq!(
            without_dying(async move { panic!("built from {formatted}") }).await,
            Err::<(), _>("built from 4".to_string())
        );
        // `unwrap` on a `None`, which is what this actually catches in the wild.
        let missing: Option<u8> = "not a number".parse().ok();
        let err = without_dying(async move { missing.expect("a tool's output") })
            .await
            .expect_err("an expect is a panic like any other");
        assert!(err.contains("a tool's output"), "{err}");
        // And a payload that is neither says so rather than inventing a cause.
        assert_eq!(
            panic_message(&std::panic::AssertUnwindSafe(())),
            "panicked with no message"
        );
    }

    /// `/ping` is the answer to "is it wedged or just busy", and the only way
    /// it can answer that is by replying *while* the gateway is busy. So it is
    /// taken by the poll loop and never queued: an answer that waited its turn
    /// behind the backlog would be indistinguishable from the silence it was
    /// sent to investigate.
    #[tokio::test]
    async fn a_ping_is_answered_from_the_poll_loop_even_mid_turn() {
        let config = Config::default();
        let allowed = [42];
        let mut pump = test_pump(&allowed, &config);
        let (mut gateway, drained) = RecordingGateway::scripted(vec![vec![
            Inbound::text(42, "and another thing"),
            Inbound::text(42, "/ping"),
        ]]);

        let cancel = CancelHandle::default();
        let turn = async move {
            let _ = drained.await;
            TurnOutcome {
                reply: "the first answer".to_string(),
                spoke: true,
            }
        };
        assert!(pump.run_turn(&mut gateway, 42, &cancel, turn).await);

        assert!(
            !cancel.is_cancelled(),
            "a liveness probe must not disturb the turn it is asking about"
        );
        let sends = gateway.sends_to(42);
        // The ordinary message queued and was told so; the ping jumped it.
        assert_eq!(sends.len(), 3, "{sends:?}");
        assert_eq!(sends[0], QUEUED_REPLY);
        assert!(sends[1].starts_with("gateway alive"), "{}", sends[1]);
        assert!(
            sends[1].contains("a turn is running"),
            "the answer has to distinguish busy from wedged: {}",
            sends[1]
        );
        assert!(
            sends[1].contains("1 waiting"),
            "and say how much is stacked up behind it: {}",
            sends[1]
        );
        assert_eq!(sends[2], "the first answer");
        assert_eq!(
            pump.queue.len(),
            1,
            "the ping is answered and dropped, never queued"
        );
    }

    /// Every claim the liveness line makes is a fact the pump tracks, and each
    /// one renders distinguishably. A status line that says "idle" for a loop
    /// which has been failing to poll for an hour is worse than no line at all:
    /// it is the reassurance an operator stops investigating on.
    #[test]
    fn the_liveness_line_distinguishes_idle_from_busy_from_not_polling() {
        let now = Instant::now();
        let mut state = Liveness::new(now - std::time::Duration::from_secs(7_000));
        state.last_poll = now - std::time::Duration::from_secs(3);
        state.served = 12;

        let idle = liveness_line(&state, now, false, 0, 0);
        assert!(idle.contains("up 1h56m"), "{idle}");
        assert!(idle.contains("last poll 3s ago"), "{idle}");
        assert!(idle.contains("12 message(s) served"), "{idle}");
        assert!(idle.contains("idle"), "{idle}");
        assert!(!idle.contains("waiting"), "nothing is queued: {idle}");
        assert!(!idle.contains("failure"), "nothing has failed: {idle}");

        let busy = liveness_line(&state, now, true, 2, 0);
        assert!(busy.contains("a turn is running"), "{busy}");
        assert!(busy.contains("2 waiting"), "{busy}");

        // The case worth having the line for at all: answering, but not
        // reaching Telegram. It must not read as healthy.
        let broken = liveness_line(&state, now, false, 0, 41);
        assert!(
            broken.contains("41 consecutive poll failure(s)"),
            "{broken}"
        );
    }

    /// The heartbeat exists so an operator can tell a quiet gateway from a dead
    /// one, and it is scheduled from the moment it fires rather than from when
    /// it was due — a gateway that spent an hour inside one turn should print
    /// one line when it surfaces, not six.
    #[test]
    fn the_heartbeat_is_periodic_and_does_not_burst_after_a_long_turn() {
        let start = Instant::now();
        let mut state = Liveness::new(start);

        assert!(!state.heartbeat_due(start), "not due the moment it starts");
        assert!(!state.heartbeat_due(start + HEARTBEAT - std::time::Duration::from_secs(1)));
        assert!(state.heartbeat_due(start + HEARTBEAT));
        assert!(
            !state.heartbeat_due(start + HEARTBEAT),
            "and not twice for the same moment"
        );

        // An hour with nothing polling: one line, then the schedule resumes
        // from there.
        let late = start + HEARTBEAT + std::time::Duration::from_secs(3_600);
        assert!(state.heartbeat_due(late));
        assert!(!state.heartbeat_due(late + std::time::Duration::from_secs(1)));
        assert!(state.heartbeat_due(late + HEARTBEAT));
    }

    #[test]
    fn a_duration_reads_at_a_glance() {
        use std::time::Duration;
        assert_eq!(compact_duration(Duration::from_secs(0)), "0s");
        assert_eq!(compact_duration(Duration::from_secs(45)), "45s");
        assert_eq!(compact_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(compact_duration(Duration::from_secs(7_000)), "1h56m");
        assert_eq!(compact_duration(Duration::from_secs(200_000)), "2d7h");
    }

    /// Adversarial: `/ping` reads out the gateway's internals, so it is
    /// allow-listed exactly like everything else. A stranger who finds the bot
    /// must not be able to learn that it is up, how long it has been up, or how
    /// busy it is — that is a reconnaissance answer, and the whole point of
    /// [`UNAUTHORIZED_REPLY`] is that a stranger learns nothing.
    #[tokio::test]
    async fn a_strangers_ping_learns_nothing() {
        let config = Config::default();
        let allowed = [42];
        let mut pump = test_pump(&allowed, &config);
        let (gateway, _drained) = RecordingGateway::scripted(Vec::new());

        assert!(
            pump.absorb(&gateway, vec![Inbound::text(999, "/ping")], None)
                .await
                .is_none()
        );
        assert_eq!(
            gateway.sends_to(999),
            vec![UNAUTHORIZED_REPLY.to_string()],
            "one vague refusal, and nothing about the gateway's state"
        );
        assert!(pump.queue.is_empty());
        assert_eq!(
            pump.liveness.served, 0,
            "a refused message was never served either"
        );
    }

    /// Nobody is at the other end of a gateway turn to answer questions between
    /// two chat messages, so the `interview` tool has to be declined. An
    /// unanswered gate is not a slow turn, it is a permanently parked one: the
    /// tool waits on a oneshot that nothing will ever send, `/stop` is the only
    /// way out, and from the chat it looks exactly like a bot that died
    /// mid-sentence. The plan gate opposite it has the same shape and its own
    /// test.
    #[test]
    fn an_interview_is_declined_so_the_turn_cannot_park_forever() {
        let mut collector = ReplyCollector::default();
        let (gate, mut answers) = crate::agent::InterviewGate::open();
        collector.handle(AgentEvent::Interview {
            questions: vec![crate::agent::InterviewQuestion {
                question: "which environment?".to_string(),
                options: Vec::new(),
            }],
            gate,
        });
        assert_eq!(
            answers.try_recv().expect("the gate must be answered"),
            None,
            "declined, so the model proceeds on its own judgment"
        );
        assert!(
            collector.reply.is_empty(),
            "and the questions are not relayed into the chat as an answer"
        );
    }

    /// `/stop` is advertised, and routes as a command in both the forms a
    /// client sends it — otherwise the one message that means "interrupt"
    /// would become one more turn for the busy agent to run.
    #[test]
    fn stop_is_offered_and_routed_as_a_command() {
        assert!(advertised_commands().iter().any(|c| c.name == STOP));
        assert!(
            crate::commands::spec(STOP).is_none(),
            "/stop is this surface's own control, not a table row"
        );

        for (text, expected) in [
            ("/stop", "/stop"),
            ("  /stop \n", "/stop"),
            ("/stop@wizardbot", "/stop"),
            ("/stop now", "/stop now"),
        ] {
            assert_eq!(
                command_line(text),
                Some(expected.to_string()),
                "{text:?} must route as a command"
            );
            assert!(
                is_stop_command(&command_line(text).expect("a command")),
                "{text:?} must be recognised as the stop"
            );
        }

        // And the near misses are not it.
        for line in ["/stopwatch", "/status", "/plan", "stop", "/"] {
            assert!(!is_stop_command(line), "{line}");
        }
    }

    /// A provider that answers nothing: these drive the agent's own state, and
    /// none of them needs a model call.
    #[derive(Debug)]
    struct SilentProvider;

    #[async_trait]
    impl crate::llm::provider::LlmProvider for SilentProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn chat_stream(
            &self,
            _request: crate::llm::ChatRequest,
        ) -> Result<crate::llm::ChatStream> {
            anyhow::bail!("no model behind this test")
        }
        fn label(&self) -> String {
            "silent".to_string()
        }
    }

    fn test_agent(cwd: &Path) -> Agent {
        let sessions = Config::sessions_dir().expect("sessions dir");
        let session = Session::create_in(&sessions, cwd).expect("session");
        let hooks = std::sync::Arc::new(crate::hooks::HookEngine::new(
            Vec::new(),
            cwd.to_path_buf(),
            session.id.clone(),
        ));
        Agent::new(
            std::sync::Arc::new(SilentProvider),
            crate::tools::registry::ToolRegistry::new(),
            Config::default(),
            Vec::new(),
            cwd.to_path_buf(),
            session,
            true,
            hooks,
        )
        .expect("agent")
    }

    /// Run one command line the way [`serve`] does.
    async fn run_command(agent: &mut Agent, cwd: &Path, line: &str) -> String {
        let config = Config::default();
        let mut mcp = crate::mcp::McpManager::empty();
        let mut fusion = false;
        let mut ctx = command::GatewayCtx {
            config: &config,
            project_root: cwd,
            mcp: &mut mcp,
            fusion: &mut fusion,
        };
        command::apply_command(agent, &mut ctx, line).await
    }

    /// The bug this file exists for: a command the menu offers runs, and comes
    /// back as text to send rather than as a prompt for the model.
    #[tokio::test]
    async fn a_known_command_runs_and_answers_with_text() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut agent = test_agent(tmp.path());

        let status = run_command(&mut agent, tmp.path(), "/status").await;
        assert!(status.contains("model: "), "{status}");
        assert!(status.contains("plan mode: off"), "{status}");

        // `/plan` and `/omakase` are table rows now, not special cases in the
        // serve loop, and they still confirm both directions in words.
        let on = run_command(&mut agent, tmp.path(), "/plan").await;
        assert!(on.contains("plan mode on"), "{on}");
        assert!(agent.plan_mode(), "the toggle reached the agent");
        let off = run_command(&mut agent, tmp.path(), "/plan").await;
        assert_eq!(off, "plan mode off");
        assert!(!agent.plan_mode());

        let omakase = run_command(&mut agent, tmp.path(), "/omakase").await;
        assert!(omakase.contains("omakase on"), "{omakase}");
        assert!(
            agent.omakase() && agent.plan_mode(),
            "one state, both flags"
        );
        let back = run_command(&mut agent, tmp.path(), "/omakase").await;
        assert!(back.contains("omakase off"), "{back}");

        // And `/help` is the table's, listing what this chat runs.
        let help = run_command(&mut agent, tmp.path(), "/help").await;
        assert!(help.contains("/status"), "{help}");
        assert!(help.contains("not available over chat: "), "{help}");
        // Including the control the table cannot know about. A help that
        // contradicts the autocomplete is how a menu comes to look stale.
        assert!(help.contains("/stop — "), "{help}");
    }

    /// A command the gateway's column declares `Unavailable` answers with the
    /// table's own refusal, and that refusal reaches the chat rather than being
    /// collected and dropped.
    #[tokio::test]
    async fn an_unavailable_command_returns_the_tables_refusal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut agent = test_agent(tmp.path());

        for (line, expected) in [
            ("/vim", "Telegram draws it"),
            ("/ui", "Telegram draws it"),
            ("/quit", "gateway is a service"),
            ("/exit", "gateway is a service"),
            ("/diff", "'/diff' does not run on this surface"),
            ("/settings", "'/settings' does not run on this surface"),
            ("/provider", "'/provider' does not run on this surface"),
            ("/login xai", "'/login' does not run on this surface"),
            ("/resume abc", "'/resume' does not run on this surface"),
        ] {
            let reply = run_command(&mut agent, tmp.path(), line).await;
            assert!(
                reply.contains(expected),
                "{line} should refuse by name, got {reply:?}"
            );
        }
    }

    /// A bad command is a reply, not a panic and not silence: the parser's own
    /// words, which are the ones the terminal's prompt would have used.
    #[tokio::test]
    async fn a_command_that_does_not_parse_answers_with_the_parsers_own_words() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut agent = test_agent(tmp.path());

        for (line, expected) in [
            ("/notacommand", "unknown command '/notacommand' — try /help"),
            (
                "/mode sideways",
                "unknown mode 'sideways' (genie|sovereign)",
            ),
            ("/rewind soon", "usage: /rewind [turn]"),
            ("/btw", "usage: /btw <question>"),
        ] {
            let reply = run_command(&mut agent, tmp.path(), line).await;
            assert_eq!(reply, expected, "{line}");
        }
    }

    /// Command output goes out through `send_rich`, so it has to survive the
    /// markdown → Telegram HTML converter: a raw `<tag>` in a usage line is a
    /// 400 from `sendMessage`, which is a *lost reply*, not a cosmetic slip.
    #[test]
    fn command_output_survives_the_html_converter() {
        let help = crate::commands::surface::help_text(crate::commands::surface::Surface::Gateway);
        let html = format::to_telegram_html(&help);

        // The argument hints are the sharp case: `/btw <question>`.
        assert!(html.contains("&lt;question&gt;"), "{html}");
        assert!(!html.contains("<question>"), "{html}");
        // The list itself survives: names, indentation and the honest tail.
        assert!(html.contains("\n  /status"), "{html}");
        assert!(html.contains("not available over chat: "), "{html}");

        // Nothing but the tags this converter emits is left unescaped, in any
        // of the reports a chat is likely to ask for.
        for text in [
            help.as_str(),
            "background tasks:\n  #1 [running] grep -r \"a<b\" . > out.txt",
            "usage: /model <tag> — or pick one from the model menu",
            "set usd_per_mtok_in / usd_per_mtok_out on provider 'p'",
        ] {
            let mut html = format::to_telegram_html(text);
            for tag in [
                "<b>", "</b>", "<i>", "</i>", "<code>", "</code>", "<pre>", "</pre>",
            ] {
                html = html.replace(tag, "");
            }
            assert!(
                !html.contains('<') && !html.contains('>'),
                "unescaped markup would cost the whole message: {html}"
            );
        }
    }

    #[test]
    fn the_poll_backoff_honours_a_stated_retry_after_and_stays_inside_the_ladder() {
        // The gateway used to sleep exactly `min(max, base * 2^n)` whole
        // seconds and ignore the platform's own deadline, so several instances
        // against one bot token woke together and re-stormed the endpoint.
        let config = crate::config::Config {
            retry_base_secs: 5,
            retry_max_secs: 300,
            ..Default::default()
        };

        let plain = anyhow::anyhow!("connection reset");
        for attempt in 0..4 {
            let delay = poll_backoff(attempt, &config, &plain);
            let ceiling = config.retry_max_secs.min(config.retry_base_secs << attempt);
            assert!(
                delay >= std::time::Duration::from_secs(config.retry_base_secs)
                    && delay <= std::time::Duration::from_secs(ceiling),
                "attempt {attempt}: {delay:?} outside [{}s, {ceiling}s]",
                config.retry_base_secs
            );
        }

        // A server-stated deadline is a floor, jittered a little on top: the
        // first retry after a 429 must not land before the platform said so.
        let stated = anyhow::Error::new(crate::llm::RetryAfter(std::time::Duration::from_secs(30)))
            .context("429 Too Many Requests");
        let delay = poll_backoff(0, &config, &stated);
        assert!(
            delay >= std::time::Duration::from_secs(30),
            "a stated Retry-After is a floor, got {delay:?}"
        );
        assert!(
            delay <= std::time::Duration::from_secs(31),
            "and it is not multiplied by the ladder, got {delay:?}"
        );
    }

    #[test]
    fn a_refused_inbound_carries_the_id_and_nothing_else() {
        let refused = Inbound::refused(-100123);
        assert_eq!(refused.chat_id, -100123);
        assert!(refused.text.is_empty());
        assert!(refused.attachments.is_empty());
    }

    /// Adversarial: a stranger who finds the bot gets nothing. An empty
    /// allow-list is the default and used to mean "allow everyone", which
    /// handed any sender a sovereign agent turn with the unrestricted tool
    /// set (and `/plan` to switch the read-only phase off). Empty must refuse.
    #[test]
    fn empty_allow_list_refuses_every_chat() {
        assert!(!is_authorized(123, &[]));
        assert!(!is_authorized(-100, &[]));
        assert!(!is_authorized(0, &[]));
        assert!(!is_authorized(i64::MIN, &[]));
        assert!(!is_authorized(i64::MAX, &[]));
        // The default config is exactly this case.
        assert!(!is_authorized(
            42,
            &crate::config::GatewayConfig::default().allowed_chat_ids
        ));
    }

    #[test]
    fn authorization_enforces_membership_when_list_set() {
        let allowed = [42, -100123];
        assert!(is_authorized(42, &allowed));
        assert!(is_authorized(-100123, &allowed));
        assert!(!is_authorized(7, &allowed));
        // A near-miss on a group id is not a match.
        assert!(!is_authorized(-100124, &allowed));
    }

    #[test]
    fn split_short_message_is_one_chunk() {
        let chunks = split_message("hello world", 4000);
        assert_eq!(chunks, vec!["hello world".to_string()]);
    }

    #[test]
    fn split_empty_message_yields_one_empty_chunk() {
        assert_eq!(split_message("", 4000), vec![String::new()]);
    }

    #[test]
    fn split_respects_max_and_preserves_content() {
        let text = "line one\nline two\nline three\nline four\n";
        let chunks = split_message(text, 18);
        assert!(chunks.iter().all(|c| c.chars().count() <= 18), "{chunks:?}");
        assert_eq!(chunks.concat(), text, "round-trips losslessly");
    }

    #[test]
    fn split_counts_characters_not_bytes() {
        // Telegram's cap is in characters; a byte-based split would cut a
        // multibyte character in half and panic (or produce invalid UTF-8).
        let text = "é".repeat(10);
        let chunks = split_message(&text, 4);
        assert_eq!(chunks, vec!["éééé", "éééé", "éé"]);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn split_hard_splits_an_overlong_line() {
        let line = "x".repeat(25);
        let chunks = split_message(&line, 10);
        assert_eq!(chunks, vec!["xxxxxxxxxx", "xxxxxxxxxx", "xxxxx"]);
        assert_eq!(chunks.concat(), line);
    }

    #[test]
    fn agent_prompt_is_text_when_no_attachments() {
        let inbound = Inbound::text(1, "hello");
        assert_eq!(inbound.agent_prompt(), "hello");
    }

    #[test]
    fn agent_prompt_appends_attachment_paths() {
        let inbound = Inbound {
            chat_id: 1,
            text: "look".to_string(),
            attachments: vec![
                std::path::PathBuf::from("/tmp/a.jpg"),
                std::path::PathBuf::from("/tmp/b.png"),
            ],
        };
        let prompt = inbound.agent_prompt();
        assert!(prompt.starts_with("look\n\n"), "{prompt}");
        assert!(prompt.contains("[attached: /tmp/a.jpg]"), "{prompt}");
        assert!(prompt.contains("[attached: /tmp/b.png]"), "{prompt}");
    }

    /// A completion that dies mid-stream is re-generated from scratch, so the
    /// half-sentence the first attempt streamed must leave the reply with it.
    /// Without this the chat gets the same paragraph twice, the first copy cut
    /// off mid-word, which reads as the model stuttering rather than as a
    /// transport fault.
    #[test]
    fn a_retried_stream_leaves_no_duplicate_in_the_reply() {
        let mut collector = ReplyCollector::default();
        collector.handle(AgentEvent::TextDelta("looked around. ".to_string()));
        collector.handle(AgentEvent::StepCompleted { step: 1 });
        // A second completion streams half an answer, dies, and is retried.
        collector.handle(AgentEvent::TextDelta("the ans".to_string()));
        collector.handle(AgentEvent::StreamRetrying);
        collector.handle(AgentEvent::TextDelta("the answer is 42".to_string()));

        assert_eq!(collector.reply, "looked around. the answer is 42");
    }

    /// The plan is part of the reply, not part of the step that follows it: a
    /// retry after the review must not take it back out.
    #[test]
    fn an_auto_approved_plan_survives_a_later_retry() {
        let mut collector = ReplyCollector::default();
        let (gate, mut verdict) = crate::agent::PlanGate::open();
        collector.handle(AgentEvent::PlanReady {
            plan: "1. do it".to_string(),
            gate,
        });
        assert!(
            verdict.try_recv().expect("verdict sent").approved,
            "a gateway plan is auto-approved"
        );

        collector.handle(AgentEvent::TextDelta("half an ans".to_string()));
        collector.handle(AgentEvent::StreamRetrying);
        collector.handle(AgentEvent::TextDelta("done".to_string()));

        assert_eq!(
            collector.reply,
            "[plan]\n1. do it\n[plan auto-approved]\n\ndone"
        );
    }
}
