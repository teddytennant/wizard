//! `wizard resume`: reopen a conversation, optionally one that belongs to
//! Claude Code.
//!
//! # Why this is not part of [`crate::claude_session`]
//!
//! That module parses `~/.claude` and is forbidden to write. Its
//! `no_write_api_reachable_from_this_module` test greps the module's own
//! source and fails the build if a single filesystem write API is so much as
//! named in it, because `~/.claude` is another program's live state: a write
//! from Wizard would at best race Claude Code appending to the file and at
//! worst destroy a conversation the user cannot get back. A runtime assertion
//! cannot see a write a *future* edit introduces, which is why the property is
//! enforced against the source text rather than at runtime.
//!
//! The import needs Wizard's session writer, so putting it in
//! [`crate::claude_session`] would mean either weakening that guard or
//! laundering the write through an indirection until the grep stopped seeing
//! it. Both defeat the point. It lives here instead: this module may write,
//! and everything it writes lands under `~/.wizard/sessions/`. The only thing
//! it does to `~/.claude` is read it.
//!
//! # Shape
//!
//! [`prepare`] is the entry point [`crate::run`] dispatches through. It turns
//! `wizard resume […]` into the plain `--resume` invocation it is equivalent
//! to, doing the Claude Code import on the way when `--claude` is given, and
//! the caller then falls through into the normal run. `to_chat_messages` is
//! the conversion itself and is the part with decisions in it; the rest is a
//! picker.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::agent::session::Session;
use crate::claude_session::{
    Chain, ChainStop, ClaudeBlock, ClaudeSession, NodeKind, SessionPreview,
};
use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::llm::{
    ChatMessage, ContentBlock, FunctionCall, Image, MAX_IMAGE_BYTES, Role, ToolCall,
    ToolResultBlock,
};

// ---------------------------------------------------------------------------
// The invocation rewrite
// ---------------------------------------------------------------------------

/// What `wizard resume` was asked for, unpacked from the subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Take the conversation from Claude Code rather than Wizard's sessions.
    pub claude: bool,
    /// The Claude Code session id (or a unique prefix) to take, if one was named.
    pub session: Option<String>,
    /// The transcript line to walk the conversation back from, if one was named.
    pub leaf: Option<String>,
    /// Print the listing and stop.
    pub list: bool,
}

impl Request {
    /// The request `cli` describes, or `None` when this invocation is not
    /// `wizard resume` at all.
    ///
    /// Reading the flags off the subcommand here rather than in [`crate::run`]
    /// keeps that dispatch chain a chain: the arm asks whether this is a
    /// resume and hands the answer straight to [`prepare`], with no partial
    /// destructuring of [`Command`] living in the middle of it.
    pub fn from_cli(cli: &Cli) -> Option<Self> {
        let Some(Command::Resume {
            claude,
            session,
            leaf,
            list,
        }) = &cli.command
        else {
            return None;
        };
        Some(Self {
            claude: *claude,
            session: session.clone(),
            leaf: leaf.clone(),
            list: *list,
        })
    }
}

/// Turn `wizard resume [--claude …]` into the `--resume` invocation it is
/// equivalent to, doing any Claude Code import on the way.
///
/// `Ok(None)` means the command already did its whole job and there is nothing
/// to run: `--list` printed the listing, or the picker was cancelled.
///
/// The caller is expected to have cleared [`Cli::command`] already, so that the
/// invocation this returns runs as the plain `--resume` it now is instead of
/// arriving back at this function.
pub fn prepare(mut cli: Cli, request: Request) -> Result<Option<Cli>> {
    // `--cwd` is applied here rather than left to the caller. Claude Code
    // files its sessions under a slug of the working directory, so a listing
    // taken before the chdir would be a different project's, and the session
    // this writes has to record the same directory the TUI will be resuming
    // in or `Session::open_latest` will not consider it.
    if let Some(dir) = &cli.cwd {
        std::env::set_current_dir(dir).with_context(|| format!("entering {}", dir.display()))?;
    }
    let root = std::env::current_dir().context("resolving the working directory")?;
    // Absolute from here on, so the caller's own chdir is a no-op rather than
    // resolving a relative `--cwd` a second time against the directory it has
    // already moved into.
    cli.cwd = Some(root.clone());
    cli.resume = true;

    if !request.claude {
        return Ok(Some(cli));
    }

    let previews = crate::claude_session::list_sessions_for_cwd(&root.display().to_string());
    if previews.is_empty() {
        bail!(
            "Claude Code has no sessions recorded for {}. It files them under a slug of the \
             working directory, so this is also what you get when it was run somewhere else.",
            root.display()
        );
    }
    if request.list {
        print_previews(&previews);
        return Ok(None);
    }

    let Some(preview) = choose(&previews, request.session.as_deref())? else {
        return Ok(None);
    };
    import_claude_session(preview, request.leaf.as_deref(), &root)?;
    Ok(Some(cli))
}

/// Print the picker rows: what each session was about, and how much of it
/// would come back.
fn print_previews(previews: &[SessionPreview]) {
    for (row, preview) in previews.iter().enumerate() {
        let updated = match preview.updated {
            Some(stamp) => stamp.format("%Y-%m-%d %H:%M").to_string(),
            None => "undated".to_string(),
        };
        // Branch points are shown because they are what makes the count above
        // them smaller than the file: a session that forked holds several
        // conversations and only one of them is being offered.
        let branches = match preview.branch_points {
            0 => "unbranched".to_string(),
            1 => "1 branch point".to_string(),
            n => format!("{n} branch points"),
        };
        println!("{:>3}  {}", row + 1, preview.title);
        let count = preview.message_count;
        println!("     {updated}  {count} message(s)  {branches}");
        println!("     {}", preview.session_id);
    }
}

/// Pick the session to take: the one named by `--session`, or the one the
/// operator chooses off the listing.
///
/// `Ok(None)` is a cancelled picker, which is not an error.
fn choose<'a>(
    previews: &'a [SessionPreview],
    wanted: Option<&str>,
) -> Result<Option<&'a SessionPreview>> {
    if let Some(wanted) = wanted {
        let hits: Vec<&SessionPreview> = previews
            .iter()
            .filter(|preview| preview.session_id.starts_with(wanted))
            .collect();
        return match hits.as_slice() {
            [] => bail!(
                "no Claude Code session for this directory has an id starting with {wanted:?}; \
                 `wizard resume --claude --list` prints the ids"
            ),
            [only] => Ok(Some(*only)),
            many => bail!(
                "{wanted:?} matches {} sessions; give more of the id",
                many.len()
            ),
        };
    }

    // Nothing chosen and nowhere to ask. Refusing beats picking the most
    // recent one: this writes a session file and then continues the
    // conversation in it, which is not something to do on a guess in a script.
    if !std::io::stdin().is_terminal() {
        bail!(
            "no session chosen and this is not a terminal; pass --session <id> \
             (`wizard resume --claude --list` prints the ids)"
        );
    }

    print_previews(previews);
    print!("Which session? [1-{}, or q to cancel] ", previews.len());
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading it")?;
    let answer = line.trim();
    if answer.is_empty() || answer.eq_ignore_ascii_case("q") {
        println!("nothing resumed.");
        return Ok(None);
    }
    let row: usize = answer
        .parse()
        .with_context(|| format!("{answer:?} is not one of the row numbers above"))?;
    previews
        .get(row.wrapping_sub(1))
        .map(Some)
        .ok_or_else(|| anyhow!("there is no row {row}"))
}

/// What an import produced.
///
/// Returned rather than printed, because the CLI is no longer the only caller:
/// the session pickers in both GUIs run the same import when a Claude Code row
/// is opened, and a surface with no stdout still has to be able to say what
/// came across and what it stopped on. See [`import`].
#[derive(Debug, Clone)]
pub struct Imported {
    /// The new Wizard session's id — what a surface opens next.
    pub id: String,
    /// The Wizard session file that was written.
    pub path: PathBuf,
    /// The Claude Code transcript it was read from, which was **not** written.
    pub source: PathBuf,
    /// That transcript's own session id.
    pub source_id: String,
    /// How many messages replayed.
    pub messages: usize,
    /// Why the parent-chain walk ended. Two of the four endings are worth
    /// telling the user about; see [`Imported::caveat`].
    pub stop: ChainStop,
}

impl Imported {
    /// What the chain ran into, when that is worth saying out loud.
    ///
    /// A conversation that stops short is the one outcome a user would
    /// otherwise mistake for data loss on Wizard's side, so both endings that
    /// truncate it get a sentence and the two that do not get silence.
    pub fn caveat(&self) -> Option<String> {
        match &self.stop {
            ChainStop::MissingParent(uuid) => Some(format!(
                "The chain stops at {uuid}, which is not in that file: normal after a /clear \
                 or a compaction, and everything before it is not recoverable here."
            )),
            ChainStop::Cycle(uuid) => Some(format!(
                "The chain loops back at {uuid}, which Claude Code does not write; that file \
                 has been edited or damaged, and the import stops at the loop."
            )),
            ChainStop::Root | ChainStop::UnknownLeaf(_) => None,
        }
    }

    /// The whole outcome as prose, for a surface whose only output channel is
    /// the conversation it just opened.
    ///
    /// It says the file was not modified because that is precisely what a user
    /// would reasonably fear an "open" had done to the session they still have
    /// running in the other program.
    pub fn summary(&self) -> String {
        let mut text = format!(
            "imported {} message(s) from Claude Code session {} — its file was read, not modified",
            self.messages, self.source_id
        );
        if let Some(caveat) = self.caveat() {
            text.push('\n');
            text.push_str(&caveat);
        }
        text
    }
}

/// Convert one Claude Code conversation into a fresh Wizard session under
/// `~/.wizard/sessions/`, so `--resume` (or a picker) picks it up as the newest
/// session for this project.
///
/// `leaf` names the line to walk the conversation back from; `None` takes the
/// tip Claude Code itself would resume ([`ClaudeSession::resume_chain`]).
/// `root` is the working directory the new session records, which is what makes
/// it the one a resume in that directory finds.
///
/// Nothing under `~/.claude` is touched: [`crate::claude_session`] cannot write
/// there (its own test scans that module's source for a write API, and drives
/// this function against a fixture tree to prove the tree survives it), and the
/// only file created here is the Wizard session being produced.
pub fn import(source: &Path, leaf: Option<&str>, root: &Path) -> Result<Imported> {
    import_into(&Config::sessions_dir()?, source, leaf, root)
}

/// [`import`] writing into an explicit sessions directory.
pub fn import_into(
    sessions_dir: &Path,
    source: &Path,
    leaf: Option<&str>,
    root: &Path,
) -> Result<Imported> {
    let session = ClaudeSession::load(source)?;
    // A Claude Code transcript is a DAG. The conversation is the parent chain
    // walked back from a chosen leaf, never the file read top to bottom: an
    // edited or rewound prompt appends a second child under the same parent,
    // so a flat read interleaves branches that were never in one conversation.
    let chain = match leaf {
        Some(leaf) => session.resolve_chain(leaf),
        None => session.resume_chain(),
    };
    if let ChainStop::UnknownLeaf(uuid) = &chain.stop {
        bail!(
            "{} has no line {uuid:?}; a leaf is a uuid from that file",
            source.display()
        );
    }

    let messages = to_chat_messages(&session, &chain);
    if messages.is_empty() {
        bail!(
            "nothing in {} would replay: the chain holds no user or assistant messages",
            source.display()
        );
    }

    let out = Session::create_in(sessions_dir, root)?;
    for message in &messages {
        out.append(message)?;
    }

    Ok(Imported {
        id: out.id.clone(),
        path: out.path().to_path_buf(),
        source: source.to_path_buf(),
        source_id: session.session_id.clone(),
        messages: messages.len(),
        stop: chain.stop,
    })
}

/// [`import`] with the terminal's report, for `wizard resume --claude`.
fn import_claude_session(preview: &SessionPreview, leaf: Option<&str>, root: &Path) -> Result<()> {
    let imported = import(&preview.path, leaf, root)?;
    println!(
        "imported {} message(s) from Claude Code session {}",
        imported.messages, imported.source_id
    );
    println!("  from  {}", imported.source.display());
    println!("  into  {}", imported.path.display());
    // Said explicitly because the alternative is what a user would reasonably
    // fear: that resuming here moves, locks or rewrites the conversation they
    // still have open in the other program.
    println!("  Claude Code's own file was read and not modified.");
    if let Some(caveat) = imported.caveat() {
        println!("  {caveat}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The conversion
// ---------------------------------------------------------------------------

/// The name recorded for a tool result whose call is not on this chain.
///
/// Reachable when the chain starts mid-conversation (a `/clear` or a
/// compaction dropped the turn that made the call), so the result is real and
/// the name for it is genuinely not in the file. Recording a placeholder keeps
/// the result, which the model needs; inventing a plausible tool name would
/// put a lie in the transcript.
const UNKNOWN_TOOL: &str = "unknown";

/// Convert the replayable part of `chain` into Wizard's own message history.
///
/// The mapping is mechanical except in three places, and those are decisions
/// rather than omissions:
///
/// - **Reasoning is dropped.** A Claude Code thinking block carries a
///   `signature`, and Anthropic accepts a replayed one only when that
///   signature comes back untouched. It was issued for another client's
///   request, on another account, and Wizard may not even be pointed at
///   Anthropic; replaying it would fail the first turn of the resumed session
///   with a provider error about a block the user never knew was there.
///   Dropping loses how the model got there and keeps what it said.
/// - **A user line becomes up to two messages.** Claude Code puts the answers
///   to the previous turn's tool calls on a `user` line, sometimes alongside
///   something the user actually typed (an interrupt). Wizard's tool results
///   live on [`Role::Tool`] messages and its prose on [`Role::User`] ones, so
///   the line splits, results first, in the order a provider needs them.
/// - **Tool results are text.** Wizard's [`ToolResultBlock`] carries a string,
///   so an image inside a `tool_result` does not survive. The text does, and
///   an error result says so in its first word, because Wizard has no
///   `is_error` field to carry the flag in.
///
/// Assistant tool calls left without a result (the conversation ended
/// mid-turn) are deliberately *not* patched here: `Session::load_history`
/// synthesizes the missing results when the session is opened, which is the
/// same repair an interrupted Wizard run gets, in one place.
fn to_chat_messages(session: &ClaudeSession, chain: &Chain) -> Vec<ChatMessage> {
    // Tool name by call id, filled in as the chain is walked. Claude Code's
    // `tool_result` blocks carry the id and not the name, and Wizard needs
    // both: every surface labels the result card with the name, and a session
    // file has to stay readable on its own. The chain is root-first, so a
    // call is always seen before the result that answers it.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut out: Vec<ChatMessage> = Vec::new();

    for node in chain.replayable(session) {
        match node.kind {
            NodeKind::Assistant => {
                let mut blocks = Vec::new();
                for block in &node.content {
                    match block {
                        ClaudeBlock::Text(text) if !text.trim().is_empty() => {
                            blocks.push(ContentBlock::text(text.clone()));
                        }
                        ClaudeBlock::ToolUse { id, name, input } => {
                            tool_names.insert(id.clone(), name.clone());
                            blocks.push(ContentBlock::ToolUse(ToolCall {
                                id: id.clone(),
                                function: FunctionCall {
                                    name: name.clone(),
                                    arguments: input.clone(),
                                },
                            }));
                        }
                        _ => {}
                    }
                }
                if !blocks.is_empty() {
                    out.push(ChatMessage::new(Role::Assistant, blocks));
                }
            }
            NodeKind::User => {
                let mut results = Vec::new();
                let mut said = Vec::new();
                for block in &node.content {
                    match block {
                        ClaudeBlock::Text(text) if !text.trim().is_empty() => {
                            said.push(ContentBlock::text(text.clone()));
                        }
                        ClaudeBlock::Image { source } => {
                            if let Some(image) = image_block(source) {
                                said.push(ContentBlock::Image(image));
                            }
                        }
                        ClaudeBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let name = tool_names
                                .get(tool_use_id)
                                .cloned()
                                .unwrap_or_else(|| UNKNOWN_TOOL.to_string());
                            results.push(ContentBlock::ToolResult(ToolResultBlock {
                                tool_use_id: tool_use_id.clone(),
                                name,
                                content: result_text(content, *is_error),
                                is_error: Some(*is_error),
                            }));
                        }
                        _ => {}
                    }
                }
                // Results before prose: a tool result answers the turn that
                // came before it, and an interrupt typed alongside it is the
                // start of the next one.
                if !results.is_empty() {
                    out.push(ChatMessage::new(Role::Tool, results));
                }
                if !said.is_empty() {
                    out.push(ChatMessage::new(Role::User, said));
                }
            }
            NodeKind::Attachment | NodeKind::System | NodeKind::Progress => {}
        }
    }
    out
}

/// Flatten a `tool_result` payload into the string Wizard records.
///
/// Claude Code writes it both as a bare string and as a block array (text plus
/// images), so both shapes are handled. `is_error` becomes a prefix because
/// Wizard's [`ToolResultBlock`] has no flag for it, and a failing tool result
/// that reads as a successful one is how a resumed model concludes that a
/// broken command worked.
fn result_text(content: &Value, is_error: bool) -> String {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            let mut joined = String::new();
            for item in items {
                let Some(part) = item.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if !joined.is_empty() {
                    joined.push('\n');
                }
                joined.push_str(part);
            }
            joined
        }
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if is_error {
        return format!("error: {text}");
    }
    text
}

/// Convert an Anthropic image `source` object into a Wizard image, or `None`
/// when it is not one this build can carry.
///
/// A `{type: "url"}` source has no bytes to take, and anything past
/// [`MAX_IMAGE_BYTES`] is dropped at this seam exactly as it is at every other
/// one: an import is not the place to put a 30 MB attachment into a history
/// that is re-sent on every turn.
fn image_block(source: &Value) -> Option<Image> {
    let data = source.get("data")?.as_str()?;
    let mime = source.get("media_type")?.as_str()?;
    // base64 encodes 3 bytes as 4 characters; this is the decoded size without
    // decoding it first.
    if data.len() / 4 * 3 > MAX_IMAGE_BYTES {
        return None;
    }
    Some(Image::new(data, mime))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    /// The redacted copies of real Claude Code sessions, the same ones
    /// `crate::claude_session` is tested against.
    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/claude_sessions")
            .join(name)
    }

    fn load(name: &str) -> ClaudeSession {
        ClaudeSession::load(&fixture(name)).expect("fixture loads")
    }

    #[test]
    fn only_the_resume_subcommand_produces_a_request() {
        // The dispatch arm in `crate::run` asks this question and nothing
        // else; answering it wrongly for another subcommand would rewrite an
        // unrelated invocation into a `--resume` one.
        let resume =
            Cli::try_parse_from(["wizard", "resume", "--claude", "--list"]).expect("cli parses");
        let request = Request::from_cli(&resume).expect("this is a resume");
        assert!(request.claude);
        assert!(request.list);
        assert_eq!(request.session, None);

        assert_eq!(
            Request::from_cli(&Cli::try_parse_from(["wizard", "doctor"]).expect("cli parses")),
            None
        );
        assert_eq!(
            Request::from_cli(&Cli::try_parse_from(["wizard"]).expect("cli parses")),
            None
        );
    }

    #[test]
    fn a_linear_session_converts_to_the_messages_it_holds() {
        let session = load("linear.jsonl");
        let messages = to_chat_messages(&session, &session.resume_chain());
        assert_eq!(messages.len(), 2, "one prompt, one reply");
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert!(!messages[0].text().is_empty());
    }

    #[test]
    fn tool_calls_keep_their_provider_ids_and_gain_their_names() {
        // The ids are the only correlation there is between a call and the
        // result that answers it, so an import that renumbered them would
        // hand the model a history no provider will accept.
        let session = load("branched.jsonl");
        let messages = to_chat_messages(&session, &session.resume_chain());

        let mut calls: HashMap<String, String> = HashMap::new();
        let mut results = 0usize;
        for message in &messages {
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse(call) => {
                        assert!(call.id.starts_with("toolu_"), "{}", call.id);
                        calls.insert(call.id.clone(), call.function.name.clone());
                    }
                    ContentBlock::ToolResult(result) => {
                        results += 1;
                        let name = calls.get(&result.tool_use_id);
                        assert_eq!(
                            name,
                            Some(&result.name),
                            "a result must carry the name of the call it answers"
                        );
                        assert_ne!(result.name, UNKNOWN_TOOL);
                    }
                    _ => {}
                }
            }
        }
        assert!(results > 0, "the fixture has tool results");
    }

    #[test]
    fn a_tool_result_lands_on_a_tool_message_before_the_call_is_answered() {
        // Wizard puts tool results on `Role::Tool` messages; Claude Code puts
        // them on a `user` line. Getting this wrong produces a history whose
        // tool results are prose, which providers reject.
        let session = load("branched.jsonl");
        let messages = to_chat_messages(&session, &session.resume_chain());
        let tool_messages = messages.iter().filter(|m| m.role == Role::Tool).count();
        assert!(tool_messages > 0);
        for message in &messages {
            let has_result = message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult(_)));
            if has_result {
                assert_eq!(message.role, Role::Tool, "results only on tool messages");
            }
        }
    }

    #[test]
    fn reasoning_is_not_replayed() {
        // The fixture has thinking blocks. Their signatures were issued for
        // another client's request, so carrying them across would fail the
        // first turn of the resumed session rather than help it.
        let session = load("branched.jsonl");
        assert!(
            session.nodes.iter().any(|node| node
                .content
                .iter()
                .any(|block| matches!(block, ClaudeBlock::Thinking { .. }))),
            "the fixture is only meaningful if it has reasoning in it"
        );
        let messages = to_chat_messages(&session, &session.resume_chain());
        for message in &messages {
            for block in &message.content {
                assert!(
                    !matches!(block, ContentBlock::Thinking(_)),
                    "no reasoning survives the import"
                );
            }
        }
    }

    #[test]
    fn the_branch_that_is_imported_is_the_one_that_was_chosen() {
        // The property the whole design rests on: the file is a DAG, so which
        // conversation comes back is decided by the leaf, not by reading the
        // file top to bottom. The fixture forks in exactly one place, and the
        // branch that was abandoned there must not appear in the import.
        let session = load("branched.jsonl");
        let points = session.branch_points();
        assert_eq!(points.len(), 1, "the fixture forks exactly once");

        let resumed_chain = session.resume_chain();
        let abandoned = points[0]
            .children
            .iter()
            .find(|child| {
                let chain = session.resolve_chain(child);
                let tip = chain.positions.last().copied();
                tip.is_some_and(|at| !resumed_chain.positions.contains(&at))
            })
            .expect("one of the two branches was abandoned");

        // Taking the abandoned tip on purpose gives a different conversation,
        // which is what `--leaf` is for.
        let other = to_chat_messages(&session, &session.resolve_chain(abandoned));
        let resumed = to_chat_messages(&session, &resumed_chain);
        assert!(!other.is_empty());
        assert!(!resumed.is_empty());
        assert_ne!(other.len(), resumed.len(), "two conversations, not one");

        // A top-to-bottom read of the file would have taken every replayable
        // line in it, the abandoned branch included. Both conversations are
        // strictly smaller than that, which is the bug this design avoids.
        let all = session
            .nodes
            .iter()
            .filter(|node| node.is_replayable())
            .count();
        assert!(resumed.len() < all, "the file holds more than one chain");
        assert!(other.len() < all);
    }

    #[test]
    fn an_unknown_leaf_is_reported_rather_than_silently_resuming_something_else() {
        let session = load("branched.jsonl");
        let chain = session.resolve_chain("not-a-uuid");
        assert!(matches!(chain.stop, ChainStop::UnknownLeaf(_)));
        assert!(to_chat_messages(&session, &chain).is_empty());
    }

    #[test]
    fn a_failed_tool_result_says_so_in_the_text() {
        // Wizard's tool-result block has no `is_error`, so the flag has to
        // survive as text or a resumed model reads a failure as a success.
        let plain = result_text(&Value::String("ok".into()), false);
        assert_eq!(plain, "ok");
        let failed = result_text(&Value::String("no such file".into()), true);
        assert!(failed.starts_with("error: "), "{failed}");

        // Block arrays are flattened to their text; a bare null is empty
        // rather than the string "null".
        let blocks = serde_json::json!([
            {"type": "text", "text": "one"},
            {"type": "image", "source": {}},
            {"type": "text", "text": "two"},
        ]);
        assert_eq!(result_text(&blocks, false), "one\ntwo");
        assert_eq!(result_text(&Value::Null, false), "");
    }

    #[test]
    fn an_oversized_or_linked_image_is_dropped_at_this_seam() {
        let small = serde_json::json!({
            "type": "base64",
            "media_type": "image/png",
            "data": "aGk=",
        });
        let image = image_block(&small).expect("a small image is taken");
        assert_eq!(image.mime, "image/png");

        // No bytes to take.
        let linked = serde_json::json!({"type": "url", "url": "https://x.invalid/a.png"});
        assert!(image_block(&linked).is_none());

        // Past the cap every other seam applies. History is re-sent on every
        // turn, so an import is the worst place to make an exception.
        let huge = "A".repeat(MAX_IMAGE_BYTES / 3 * 4 + 16);
        let big = serde_json::json!({"media_type": "image/png", "data": huge});
        assert!(image_block(&big).is_none());
    }

    /// The entry point both GUIs' pickers call. It has to write a Wizard
    /// session that carries the chosen *branch* and nothing else, and it has to
    /// report enough for a surface with no stdout to explain what happened.
    #[test]
    fn importing_a_branched_session_writes_only_the_chosen_branch() {
        let sessions = tempfile::tempdir().expect("sessions dir");
        let workspace = tempfile::tempdir().expect("workspace");
        let source = fixture("branched.jsonl");
        let session = load("branched.jsonl");

        let tip = session.tip().map(str::to_string);
        let imported = import_into(sessions.path(), &source, tip.as_deref(), workspace.path())
            .expect("import the tip");
        assert_eq!(imported.source, source);
        assert_eq!(imported.source_id, session.session_id);
        assert_eq!(imported.stop, ChainStop::Root);
        assert_eq!(imported.caveat(), None, "a clean chain says nothing extra");
        assert!(imported.summary().contains("read, not modified"));

        // The file holds more than one conversation; the import holds one.
        let all = session
            .nodes
            .iter()
            .filter(|node| node.is_replayable())
            .count();
        assert!(imported.messages > 0);
        assert!(
            imported.messages < all,
            "{} of {all} — a top-to-bottom read would have taken them all",
            imported.messages
        );

        // And it is a session Wizard can reopen by the id that was reported.
        let reopened = Session::open_by_id(sessions.path(), &imported.id)
            .expect("open by id")
            .expect("the session that was just written");
        assert_eq!(reopened.path(), imported.path);
        let root = workspace.path().display().to_string();
        assert_eq!(
            reopened.cwd(),
            Some(root.as_str()),
            "the session records the directory it will run in"
        );

        // The abandoned branch is not in it. The fixture forks exactly once,
        // and the child that is not on the resumed chain is the conversation
        // the user rewound away from — replaying it would put words in the
        // transcript that were never in this conversation.
        let points = session.branch_points();
        assert_eq!(points.len(), 1);
        let resumed = session.resume_chain();
        let abandoned = points[0]
            .children
            .iter()
            .find(|child| {
                let tip = session.resolve_chain(child).positions.last().copied();
                tip.is_some_and(|at| !resumed.positions.contains(&at))
            })
            .expect("one branch was abandoned");
        let other = import_into(sessions.path(), &source, Some(abandoned), workspace.path())
            .expect("import the abandoned branch on purpose");
        assert_ne!(
            other.messages, imported.messages,
            "two conversations, not one"
        );
        assert_ne!(other.id, imported.id, "and two sessions");
    }

    /// A leaf that names nothing is refused rather than quietly resolving to
    /// some other conversation, and nothing is written when it is.
    #[test]
    fn importing_an_unknown_leaf_writes_no_session() {
        let sessions = tempfile::tempdir().expect("sessions dir");
        let workspace = tempfile::tempdir().expect("workspace");
        let err = import_into(
            sessions.path(),
            &fixture("branched.jsonl"),
            Some("not-a-uuid"),
            workspace.path(),
        )
        .expect_err("an unknown leaf is an error");
        assert!(format!("{err:#}").contains("not-a-uuid"), "{err:#}");
        assert_eq!(
            std::fs::read_dir(sessions.path())
                .expect("read the sessions dir")
                .count(),
            0,
            "a refused import leaves no half-written session behind"
        );
    }

    #[test]
    fn a_result_whose_call_is_off_the_chain_keeps_a_placeholder_name() {
        // Reachable when a compaction dropped the turn that made the call:
        // the chain starts mid-conversation and the first tool result on it
        // answers something no longer in the file. The result is real and the
        // model needs it; the name is genuinely unknown, so it says so rather
        // than naming a plausible tool that was never called.
        let session = load("branched.jsonl");
        let chain = session.resume_chain();
        let start = chain
            .positions
            .iter()
            .position(|&at| {
                session.nodes[at]
                    .content
                    .iter()
                    .any(|block| matches!(block, ClaudeBlock::ToolResult { .. }))
            })
            .expect("the fixture answers a tool call");
        let tail = Chain {
            positions: chain.positions[start..].to_vec(),
            stop: chain.stop.clone(),
        };

        let messages = to_chat_messages(&session, &tail);
        let first = messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .expect("the window opens on a tool result");
        assert_eq!(first.name, UNKNOWN_TOOL);
        assert!(!first.tool_use_id.is_empty(), "the id still correlates");
    }
}
