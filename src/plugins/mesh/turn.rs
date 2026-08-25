//! What a peer's session stream actually carries: an [`AgentEvent`] that came
//! from another machine.
//!
//! # There is one event enum, and it is the agent's
//!
//! [`AgentEvent`] is the agent loop's report of what happened, and since
//! workstream F3 it is [`Clone`], [`Serialize`] and [`Deserialize`]. That makes
//! it the wire format: a remote node's turn arrives as the same values the
//! local loop emits, so every surface that can already render a turn renders a
//! peer's turn for free, with no translation layer to keep in step.
//!
//! An earlier draft of [`super::transport`] mirrored the agent's events with a
//! small enum of its own (`session_started`, `session_updated`, …) because
//! `AgentEvent` could not be cloned or serialised at the time. Mirroring a
//! growing enum is a debt that pays out as a peer's turn rendering as
//! "something happened" forever, so the mirror is gone. What is left in
//! [`super::PeerEventKind`] is only the three things the agent loop has nothing
//! to say about: a session starting, a session ending, and a peer re-advertising
//! itself.
//!
//! # Why the payload is a newtype and not a bare `AgentEvent`
//!
//! For the same reason [`super::PeerText`] is not a `String`. A `PeerTurn` is
//! attacker-controlled data that reaches a terminal and a GUI, and the type is
//! the reminder. [`PeerTurn::sanitize`] is the only way to build one, the
//! [`Deserialize`] impl runs the same pass (so a record decoded off a wire gets
//! the same treatment as one constructed here), and [`PeerTurn::as_event`] is a
//! greppable name for every place something unwraps a peer's report.
//!
//! It is **display data**. It must never reach a system prompt, a tool
//! argument, or a command dispatcher. Sanitising does not make it safe there;
//! nothing does.
//!
//! # What sanitising does
//!
//! The pass is generic: the event is encoded to JSON, every string in the tree
//! is cleaned, and the result is decoded back into an [`AgentEvent`]. Generic
//! on purpose. A pass written variant by variant is the mirrored enum again in
//! another shape, and it would be one variant behind from the first day
//! somebody adds one; this one covers a variant added tomorrow on the day it
//! lands.
//!
//! On top of the text cleaning there are two members that never cross, whatever
//! carries them ([`redacted`]), a class of variant that never crosses at all
//! ([`AgentEvent::is_request`]), and three bounds ([`PeerTurn::MAX_TEXT`],
//! [`PeerTurn::MAX_ITEMS`], [`PeerTurn::MAX_DEPTH`]). Every one of them fails
//! closed: an event that does not survive the pass is refused, not delivered
//! half-cleaned.

use anyhow::{Context, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::agent::AgentEvent;

use super::{PeerText, sanitize_body, sanitize_label};

/// One [`AgentEvent`] that came from a peer.
///
/// See the [module docs](self): the wrapper carries no data of its own, it
/// carries the fact that what is inside was written by a machine this one does
/// not control.
#[derive(Debug, Clone)]
pub struct PeerTurn(AgentEvent);

impl PeerTurn {
    /// Characters of peer-written text one event may carry, counted over every
    /// string value in it rather than per string.
    ///
    /// One number rather than a per-field cap, because per-field caps multiply:
    /// a bound of "8k per string" over an event with a thousand strings is not
    /// a bound. A turn's text is prose and a tool result is a file, so this is
    /// generous by the standards of [`PeerText::MAX_CHARS`] and still bounded:
    /// at four bytes per character it is 64 KiB of the worst case, times
    /// [`super::transport::SUBSCRIPTION_BUFFER`] buffered events, per
    /// subscription.
    pub const MAX_TEXT: usize = 16 * 1024;

    /// Elements kept from any one array or object inside an event.
    ///
    /// [`PeerTurn::MAX_TEXT`] bounds the *text* but not the *count*: a peer
    /// that sends a hundred thousand empty todo items costs nothing in
    /// characters and a great deal in allocations.
    pub const MAX_ITEMS: usize = 256;

    /// Nesting kept inside an event.
    ///
    /// [`AgentEvent::ToolStarted`] carries free-form JSON arguments, so the
    /// shape of an event is partly the far end's choice, and the sanitising
    /// pass walks it recursively. A thousand-deep array is a stack overflow
    /// with no error message. 128 matches serde_json's own decoder limit, so
    /// this bound is never the first thing a hostile message meets.
    pub const MAX_DEPTH: usize = 128;

    /// Clean one of this machine's own agent events for the mesh. The only way
    /// to build a [`PeerTurn`] in code.
    ///
    /// `None` when the event does not cross at all (see
    /// [`AgentEvent::is_request`]) or when it
    /// did not survive the pass. Both are refusals, and both are silent to the
    /// caller by design: a publisher that had to handle "this one does not
    /// cross" at every call site would eventually stop handling it.
    ///
    /// Outbound events go through this too, not only inbound ones. The
    /// loopback hands the same value to the subscriber that the publisher
    /// built, with no encode in between, so a boundary that only cleaned on the
    /// way *in* would clean nothing at all in this release.
    pub fn sanitize(event: &AgentEvent) -> Option<Self> {
        let encoded = match serde_json::to_value(event) {
            Ok(encoded) => encoded,
            Err(why) => {
                tracing::debug!("a local agent event could not be encoded for the mesh: {why:#}");
                return None;
            }
        };
        match Self::clean(encoded) {
            Ok(turn) => Some(turn),
            Err(why) => {
                tracing::debug!("an agent event does not cross the mesh: {why:#}");
                None
            }
        }
    }

    /// The report, for rendering.
    ///
    /// Named so it is greppable. Every call is a place where a peer's data
    /// leaves the boundary, and there should be few enough of them to read in
    /// one sitting.
    pub fn as_event(&self) -> &AgentEvent {
        &self.0
    }

    /// The report, owned, for a surface that folds it into a transcript.
    pub fn into_event(self) -> AgentEvent {
        self.0
    }

    /// The whole pass, over an already-encoded event.
    ///
    /// Shared by [`PeerTurn::sanitize`] and by [`Deserialize`], so the two
    /// paths into this type cannot drift: the bug that costs a boundary its
    /// meaning is the one where constructing is checked and decoding is not.
    fn clean(mut encoded: Value) -> anyhow::Result<Self> {
        let mut budget = Self::MAX_TEXT;
        scrub(&mut encoded, &mut budget, 0);
        let event: AgentEvent = serde_json::from_value(encoded)
            .context("a peer's agent event did not survive sanitising")?;
        // Asked of the decoded event rather than of its serde tag, so the rule
        // is an exhaustive match next to the variants and a variant added later
        // cannot cross by default. Decoding first is safe: `scrub` has already
        // bounded the text, the breadth and the depth, and building an enum
        // value has no effect beyond building it.
        if event.is_request() {
            bail!(
                "a peer's event asks this machine for something rather than reporting what happened"
            );
        }
        Ok(Self(event))
    }
}

/// Serialises as the event itself: the wrapper is a claim about where the value
/// came from, and there is nobody on the far side for that claim to be true of.
impl Serialize for PeerTurn {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

/// Sanitises on the way in, so a record read off a wire gets the same treatment
/// as one built by [`PeerTurn::sanitize`].
///
/// The same reasoning as [`PeerText`]'s own [`Deserialize`]: a boundary that can
/// be walked around by decoding a record instead of constructing one is not a
/// boundary. Decoding into a [`Value`] first is what makes the pass generic, and
/// it is also what keeps [`AgentEvent`] from ever being built out of unclean
/// input, even transiently.
impl<'de> Deserialize<'de> for PeerTurn {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = Value::deserialize(deserializer)?;
        PeerTurn::clean(encoded).map_err(serde::de::Error::custom)
    }
}

// Whether a variant is a *report* of something that happened, and so may cross
// the mesh, or a *request* for this machine to act, which may not, is decided
// by `AgentEvent::is_request` in `src/agent/event.rs`. It lives there, as an
// exhaustive match next to the variants, so that a variant added later cannot
// cross to peers by default; it used to be a one-entry negative match on the
// serde tag here, and the person adding a request-shaped variant was never
// asked. `PeerTurn::clean` is the caller.
//
// `AgentEvent::PlanReady` and `AgentEvent::Interview` are reports by that rule
// even though something is waiting on them: their text is the most interesting
// thing in a plan-mode turn and a watcher should see it. What is taken from
// them is the ability to answer, by voiding the ticket (see `redacted`).

/// What a member must be replaced with, when it is one of the two that never
/// cross whatever carries them.
///
/// Keyed on the member's name rather than on [`AgentEvent`]'s shape, so a
/// variant added tomorrow is covered the day it lands rather than the day
/// somebody remembers this file.
///
/// - `images` becomes an empty array. An image on an event is a *file on the
///   sender's disk* ([`crate::images::ImageRef`]) plus, one level down, the
///   base64 of its bytes. The path is the dangerous half: the TUI prints it and
///   the GUI links to it for "open full size", so carrying it through turns a
///   peer's event into an instruction for this machine's renderer to open a
///   local file of the peer's choosing, and `~/.ssh/id_rsa` is a path like any
///   other. Emptying the array removes every filesystem path from the wire in
///   one move, and takes the megabytes of base64 with it, which a watch stream
///   has no budget for anyway. A watcher is told an image was produced; it is
///   not handed one.
/// - `gate` becomes ticket 0. A gate is a live turn parked in the process that
///   opened it ([`crate::agent::PlanGate`]). Over a socket the number is
///   meaningless, but the loopback puts both nodes in *one* process sharing
///   *one* gate desk, so a watcher could approve a plan review on the machine
///   it is only supposed to be watching. Ticket 0 is never issued (the desk's
///   counter starts at 1), so a voided gate finds nothing to claim and
///   `claim()` answers `None`.
///
/// Nothing else is redacted by name. In particular a `path` inside
/// [`AgentEvent::ToolStarted`]'s free-form arguments is kept: it is text the
/// peer's agent chose, rendered as text, and blanking it would hide what the
/// peer's agent actually did while protecting nothing. The property that
/// matters is narrower than "no peer ever mentions a path": it is that nothing
/// this machine *opens* is pointed at by a peer.
fn redacted(name: &str) -> Option<Value> {
    match name {
        "images" => Some(Value::Array(Vec::new())),
        "gate" => Some(Value::from(0u64)),
        _ => None,
    }
}

/// Clean every string in `encoded` in place, spending `budget` as it goes.
///
/// Recursive, and bounded at [`PeerTurn::MAX_DEPTH`] because the arguments of a
/// tool call are the far end's JSON: past the limit a subtree becomes `Null`,
/// which either lands in a free-form [`Value`] field harmlessly or fails the
/// decode that follows, and a refused event is the right end of that.
fn scrub(encoded: &mut Value, budget: &mut usize, depth: usize) {
    if depth > PeerTurn::MAX_DEPTH {
        *encoded = Value::Null;
        return;
    }
    match encoded {
        Value::String(text) => {
            // The body policy, not the label one: see [`sanitize_body`]. A
            // turn's text is prose whose lines and indentation are most of its
            // meaning, and a delta is a *fragment* of it, so the trimming and
            // whitespace-collapsing that make a good node label would corrupt
            // the stream (`"hello"` + `" world"` is two words until something
            // eats the leading space).
            //
            // The budget is spent in serde's field order, so a variant whose
            // first field is enormous leaves nothing for the rest. That is the
            // honest shape of a single bound: what it costs is a truncated
            // event, and what it buys is one number to state.
            let cleaned = sanitize_body(text, *budget);
            *budget = budget.saturating_sub(cleaned.chars().count());
            *text = cleaned;
        }
        Value::Array(items) => {
            items.truncate(PeerTurn::MAX_ITEMS);
            for item in items.iter_mut() {
                scrub(item, budget, depth + 1);
            }
        }
        Value::Object(fields) => {
            let mut kept = Map::new();
            for (name, mut member) in std::mem::take(fields) {
                if kept.len() == PeerTurn::MAX_ITEMS {
                    break;
                }
                // Depth 0 is the variant tag, not a field: serde writes
                // `AgentEvent::Images` as `{"images": {…}}`, so the outermost
                // member of every event is named after the variant. Redacting
                // there would replace the whole payload of the one variant
                // whose *name* collides with a field name, and the event would
                // be refused rather than cleaned. Fields start one level in.
                match redacted(&name).filter(|_| depth > 0) {
                    Some(replacement) => member = replacement,
                    None => scrub(&mut member, budget, depth + 1),
                }
                // A member's *name* is peer text too (an object inside a tool
                // call's arguments is keyed by whatever the far end wrote), so
                // it is cleaned as well. It is bounded on its own rather than
                // out of `budget`: names are structure, and letting one long
                // value exhaust the budget before the next key is read would
                // blank a field name and lose the whole event to a decode
                // failure. Two names that clean to the same text collapse into
                // one member, which is the honest reading of two names a human
                // could not tell apart.
                kept.insert(sanitize_label(&name, PeerText::MAX_CHARS), member);
            }
            *fields = kept;
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{DoneReason, ImageSource, PlanGate};
    use crate::images::ImageRef;
    use crate::llm::Image;
    use crate::tools::ToolOutput;

    /// The encoded form of a turn, which is the only equality a wire type has:
    /// [`AgentEvent`] is deliberately not `PartialEq` (a `ToolOutput` carries
    /// image bytes, and comparing those is never what a test means).
    fn encoded(turn: &PeerTurn) -> Value {
        serde_json::to_value(turn).expect("a peer turn encodes")
    }

    /// A turn that has been through the boundary, or a panic naming what was
    /// refused.
    fn crossed(event: AgentEvent) -> PeerTurn {
        PeerTurn::sanitize(&event).unwrap_or_else(|| panic!("{event:?} should cross the mesh"))
    }

    #[test]
    fn a_report_crosses_as_itself() {
        // The whole point of F3: no translation, no summary, the same values
        // the local loop emits.
        let turn = crossed(AgentEvent::StepCompleted { step: 3 });
        assert!(matches!(
            turn.as_event(),
            AgentEvent::StepCompleted { step: 3 }
        ));
        let turn = crossed(AgentEvent::Done {
            reason: DoneReason::Completed,
        });
        assert!(matches!(
            turn.as_event(),
            AgentEvent::Done {
                reason: DoneReason::Completed
            }
        ));
        // A unit variant encodes as a bare string rather than an object, which
        // is the shape a tag filter written against objects alone would miss.
        let turn = crossed(AgentEvent::StreamRetrying);
        assert_eq!(encoded(&turn), Value::String("stream_retrying".into()));
        let turn = crossed(AgentEvent::Usage {
            prompt_tokens: 1200,
            completion_tokens: 34,
        });
        assert!(matches!(
            turn.as_event(),
            AgentEvent::Usage {
                prompt_tokens: 1200,
                ..
            }
        ));
    }

    #[test]
    fn text_is_cleaned_but_keeps_its_shape() {
        // A peer's assistant text lands in a terminal whose whole surface is
        // escape sequences, so the escape has to go. Its lines and its
        // indentation do not: a transcript that ran every line of a code block
        // together and stripped the indent would be a sanitiser nobody leaves
        // switched on.
        let turn = crossed(AgentEvent::TextDelta(
            "\u{1b}[2Jfn main() {\n    println!(\"hi\");\n}\n\n\n\ndone\u{202e}\u{200b}".into(),
        ));
        let AgentEvent::TextDelta(text) = turn.as_event() else {
            panic!("{turn:?}");
        };
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\u{202e}'), "{text:?}");
        assert!(!text.contains('\u{200b}'), "{text:?}");
        assert_eq!(text, " [2Jfn main() {\n    println!(\"hi\");\n}\n\ndone");
        // Blank-line runs collapse to one blank line, so a peer cannot scroll
        // a watcher's screen with newlines alone.
        assert!(!text.contains("\n\n\n"), "{text:?}");

        // A delta is a fragment, so its own leading and trailing space is
        // content: trimming it would join two words that the peer's model put
        // apart, which is a sanitiser that changes what was said.
        let turn = crossed(AgentEvent::TextDelta(" world ".into()));
        let AgentEvent::TextDelta(text) = turn.as_event() else {
            panic!("{turn:?}");
        };
        assert_eq!(text, " world ");
    }

    #[test]
    fn a_command_request_never_crosses() {
        // A subscription is read-only. `CommandRequested` is Wizard's own
        // slash-command line, dispatched by the interactive surface once the
        // turn ends: arriving from a peer it is another machine driving this
        // one's menu.
        assert!(
            PeerTurn::sanitize(&AgentEvent::CommandRequested("/model gpt-5.3-codex".into()))
                .is_none()
        );
        // And it cannot be smuggled past the constructor by decoding a record
        // instead of building one, which is the bypass that matters.
        let decoded: Result<PeerTurn, _> =
            serde_json::from_str("{\"command_requested\":\"/model gpt-5.3-codex\"}");
        assert!(decoded.is_err(), "{:?}", decoded.map(|t| encoded(&t)));
    }

    #[test]
    fn a_gate_ticket_cannot_be_claimed_through_the_mesh() {
        // The loopback runs both nodes in one process, and the gate desk is a
        // process-wide static, so an un-voided ticket would let a watcher
        // approve a plan review on the machine it is watching.
        let (gate, _parked) = PlanGate::open();
        let turn = crossed(AgentEvent::PlanReady {
            plan: "1. read the file\n2. write the file".into(),
            gate,
        });
        let AgentEvent::PlanReady {
            plan,
            gate: delivered,
        } = turn.as_event()
        else {
            panic!("{turn:?}");
        };
        // The plan itself crosses: it is the most interesting thing in a
        // plan-mode turn and a watcher is there to read it.
        assert_eq!(plan, "1. read the file\n2. write the file");
        assert!(
            delivered.claim().is_none(),
            "a watcher must not be able to answer the peer's plan review"
        );
        // The desk still holds the real one: what was voided is the copy that
        // crossed, not the gate the peer's own surface is going to answer.
        assert!(gate.claim().is_some(), "the local gate was collateral");
    }

    #[test]
    fn images_do_not_cross_because_a_path_is_an_instruction() {
        // An `ImageRef` names a file on the *sender's* disk, and the surfaces
        // open what they are pointed at.
        let turn = crossed(AgentEvent::Images {
            source: ImageSource::Tool("screenshot".into()),
            images: vec![ImageRef {
                path: "/home/someone/.ssh/id_ed25519".into(),
                mime: "image/png".into(),
                bytes: 4096,
            }],
        });
        let AgentEvent::Images { images, source } = turn.as_event() else {
            panic!("{turn:?}");
        };
        assert!(images.is_empty(), "{images:?}");
        assert_eq!(source, &ImageSource::Tool("screenshot".into()));
        assert!(!encoded(&turn).to_string().contains("id_ed25519"));

        // One level down, on a tool result, and with the base64 payload that a
        // text stream has no budget for.
        let turn = crossed(AgentEvent::ToolFinished {
            name: "render_chart".into(),
            output: ToolOutput::ok_with_images(
                "wrote chart.png",
                vec![Image::new("QUJD", "image/png")],
            ),
        });
        let AgentEvent::ToolFinished { output, .. } = turn.as_event() else {
            panic!("{turn:?}");
        };
        assert_eq!(output.content, "wrote chart.png");
        assert!(output.images.is_empty(), "{:?}", output.images);
        assert!(!encoded(&turn).to_string().contains("QUJD"));
    }

    #[test]
    fn a_tool_call_keeps_the_arguments_that_say_what_the_peer_did() {
        // The other side of the redaction: it is scoped to what this machine
        // would *open*, not to every string that looks like a path. A watcher
        // whose tool calls all render as `read_file()` is watching nothing.
        let turn = crossed(AgentEvent::ToolStarted {
            name: "read_file".into(),
            args: serde_json::json!({ "path": "src/mesh/mod.rs", "limit": 40 }),
        });
        let AgentEvent::ToolStarted { args, .. } = turn.as_event() else {
            panic!("{turn:?}");
        };
        assert_eq!(args["path"], Value::String("src/mesh/mod.rs".into()));
        assert_eq!(args["limit"], Value::from(40));
    }

    #[test]
    fn sanitising_survives_the_decode_path() {
        // The bypass: build the payload by decoding a record rather than by
        // calling the constructor. Nothing here went through `sanitize`, so
        // this is the only pass the text gets.
        let hostile = serde_json::json!({
            "tool_finished": {
                "name": "execute\u{202e}",
                "output": {
                    "content": "\u{1b}]0;owned\u{0007}ok",
                    "is_error": false,
                    "images": [{ "b64": "QUJD", "mime": "image/png", "path": "/etc/shadow" }],
                },
            }
        });
        let turn: PeerTurn = serde_json::from_value(hostile).expect("decode");
        let AgentEvent::ToolFinished { name, output } = turn.as_event() else {
            panic!("{turn:?}");
        };
        assert_eq!(name, "execute");
        assert!(!output.content.contains('\u{1b}'), "{:?}", output.content);
        assert!(output.content.contains("ok"), "{:?}", output.content);
        assert!(output.images.is_empty());
        assert!(!encoded(&turn).to_string().contains("shadow"));
        // And it round-trips as the cleaned form, so re-encoding what was
        // received cannot restore the original bytes.
        let again: PeerTurn = serde_json::from_value(encoded(&turn)).expect("re-decode");
        assert_eq!(encoded(&again), encoded(&turn));
    }

    #[test]
    fn one_event_carries_a_bounded_amount_of_everything() {
        // Text. The bound is over the whole event rather than per string,
        // because per-string caps multiply by however many strings a variant
        // happens to have.
        let turn = crossed(AgentEvent::SubagentFinished {
            id: 1,
            name: "x".repeat(PeerTurn::MAX_TEXT * 2),
            task: "y".repeat(PeerTurn::MAX_TEXT * 2),
            completed: true,
            output: "z".repeat(PeerTurn::MAX_TEXT * 2),
        });
        let AgentEvent::SubagentFinished {
            name, task, output, ..
        } = turn.as_event()
        else {
            panic!("{turn:?}");
        };
        let text = name.chars().count() + task.chars().count() + output.chars().count();
        assert!(text <= PeerTurn::MAX_TEXT, "{text} characters crossed");
        assert!(name.ends_with('…'), "the elision is visible: {name:?}");
        // Spent in serde's field order, so the fields after the one that ate
        // the budget get nothing. A truncated event, never an unbounded one.
        assert!(task.is_empty() && output.is_empty(), "{task:?} {output:?}");

        // Breadth. Cheap in characters, expensive in allocations.
        let todos: Vec<_> = (0..PeerTurn::MAX_ITEMS * 4)
            .map(|_| crate::tools::todo::TodoItem {
                content: String::new(),
                status: crate::tools::todo::TodoStatus::Pending,
            })
            .collect();
        let turn = crossed(AgentEvent::TodoUpdated(todos));
        let AgentEvent::TodoUpdated(items) = turn.as_event() else {
            panic!("{turn:?}");
        };
        assert_eq!(items.len(), PeerTurn::MAX_ITEMS);

        // Depth. A tool call's arguments are the far end's JSON, and the pass
        // that walks them is recursive.
        let mut nested = Value::Null;
        for _ in 0..PeerTurn::MAX_DEPTH + 16 {
            nested = Value::Array(vec![nested]);
        }
        let turn = crossed(AgentEvent::ToolStarted {
            name: "deep".into(),
            args: nested,
        });
        let AgentEvent::ToolStarted { args, .. } = turn.as_event() else {
            panic!("{turn:?}");
        };
        fn depth_of(value: &Value) -> usize {
            match value {
                Value::Array(items) => 1 + items.iter().map(depth_of).max().unwrap_or(0),
                Value::Object(fields) => 1 + fields.values().map(depth_of).max().unwrap_or(0),
                _ => 0,
            }
        }
        assert!(
            depth_of(args) <= PeerTurn::MAX_DEPTH,
            "{} levels crossed",
            depth_of(args)
        );
    }

    #[test]
    fn an_event_that_does_not_survive_the_pass_is_refused_rather_than_half_cleaned() {
        // Fail closed. A member name that cleans away entirely takes the field
        // it was naming with it, and the decode that follows is what notices.
        let mangled = serde_json::json!({
            "tool_started": { "\u{200b}\u{200b}": "read_file", "args": {} }
        });
        assert!(serde_json::from_value::<PeerTurn>(mangled).is_err());
        // A variant nobody here has ever heard of is refused too, rather than
        // being decoded into whatever it happens to match.
        assert!(serde_json::from_value::<PeerTurn>(serde_json::json!({"rm_rf": "/"})).is_err());
    }
}
