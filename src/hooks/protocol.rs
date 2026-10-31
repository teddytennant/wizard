//! The JSON a hook prints on stdout, and what an exit code means.
//!
//! This follows Claude Code's protocol: a single JSON object on stdout with
//! universal fields (`continue`, `stopReason`, `systemMessage`, `decision`,
//! `reason`) and an event-specific `hookSpecificOutput` object. Exit 2 blocks
//! with stderr as the reason; any other nonzero exit is a warning.

use serde_json::Value;

/// A tool-use verdict from `permissionDecision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny,
    Ask,
    /// Accepted, but Wizard has no deferred-call resume, so it counts as no
    /// opinion.
    Defer,
}

/// What a hook's stdout said, normalised across the field spellings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookOutput {
    /// `continue: false` halts the turn.
    pub halt: bool,
    /// `stopReason`, shown to the user when `halt` is set.
    pub stop_reason: Option<String>,
    /// `systemMessage`, a note for the user.
    pub system_message: Option<String>,
    /// Top-level `decision: "block"` (or the old `"block"` spelling).
    pub block: bool,
    /// `reason` next to a decision.
    pub reason: Option<String>,
    /// `permissionDecision` (or the deprecated `approve`/`block`).
    pub verdict: Option<Verdict>,
    /// `permissionDecisionReason`.
    pub verdict_reason: Option<String>,
    /// `updatedInput`, in Claude's argument shape.
    pub updated_input: Option<Value>,
    /// Wizard's own `updated_args`, in Wizard's argument shape.
    pub updated_args: Option<Value>,
    /// `additionalContext`.
    pub additional_context: Option<String>,
    /// `updatedToolOutput`, replaces what the model sees from a tool.
    pub updated_tool_output: Option<String>,
    /// `hookSpecificOutput.decision` for `PermissionRequest`.
    pub permission_request: Option<PermissionRequestDecision>,
    /// The whole `hookSpecificOutput` object, for event-specific fields
    /// (`action`, `content`, `worktreePath`, `displayContent`, ...).
    pub specific: Value,
}

/// `hookSpecificOutput.decision` on `PermissionRequest`.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequestDecision {
    pub allow: bool,
    pub message: Option<String>,
    pub updated_input: Option<Value>,
}

/// Parse stdout as a hook response. `None` means "not a JSON object", which
/// is plain text. The rule is Claude's: the trimmed output must start with
/// `{` and end with `}` and parse as one object.
pub fn parse_stdout(stdout: &str) -> Option<HookOutput> {
    let trimmed = stdout.trim();
    if !(trimmed.starts_with('{') && trimmed.ends_with('}')) {
        return None;
    }
    let Value::Object(map) = serde_json::from_str::<Value>(trimmed).ok()? else {
        return None;
    };
    let text = |key: &str| map.get(key).and_then(Value::as_str).map(str::to_string);
    let specific = map
        .get("hookSpecificOutput")
        .cloned()
        .unwrap_or(Value::Null);
    let stext = |key: &str| {
        specific
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    let mut out = HookOutput {
        halt: map.get("continue") == Some(&Value::Bool(false)),
        stop_reason: text("stopReason"),
        system_message: text("systemMessage"),
        reason: text("reason"),
        updated_args: map.get("updated_args").filter(|v| v.is_object()).cloned(),
        additional_context: stext("additionalContext").or_else(|| text("additionalContext")),
        updated_input: specific
            .get("updatedInput")
            .filter(|v| v.is_object())
            .cloned(),
        updated_tool_output: stext("updatedToolOutput").or_else(|| text("updatedToolOutput")),
        verdict_reason: stext("permissionDecisionReason"),
        specific: specific.clone(),
        ..HookOutput::default()
    };

    match text("decision").as_deref() {
        Some("block") => out.block = true,
        Some("approve") => out.verdict = Some(Verdict::Allow),
        _ => {}
    }
    if let Some(decision) = stext("permissionDecision") {
        out.verdict = match decision.as_str() {
            "allow" => Some(Verdict::Allow),
            "deny" => Some(Verdict::Deny),
            "ask" => Some(Verdict::Ask),
            "defer" => Some(Verdict::Defer),
            _ => out.verdict,
        };
    }
    if let Some(decision) = specific.get("decision").filter(|v| v.is_object()) {
        let behavior = decision.get("behavior").and_then(Value::as_str);
        if let Some(allow) = match behavior {
            Some("allow") => Some(true),
            Some("deny") => Some(false),
            _ => None,
        } {
            out.permission_request = Some(PermissionRequestDecision {
                allow,
                message: decision
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                updated_input: decision
                    .get("updatedInput")
                    .filter(|v| v.is_object())
                    .cloned(),
            });
        }
    }
    Some(out)
}

/// What one finished hook process means once exit code and stdout are read
/// together.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdicts {
    /// Exit 0 (or a nonzero code alongside valid JSON is not special-cased).
    Ok(Option<HookOutput>, String),
    /// Exit 2: block, with this reason.
    Block(String, Option<HookOutput>),
    /// Any other exit code: a warning, nothing else.
    Warn(String),
}

/// Apply the exit-code table: 0 continues, 2 blocks with stderr as the
/// reason, anything else is a warning.
pub fn interpret(code: i32, stdout: &str, stderr: &str) -> Verdicts {
    match code {
        0 => Verdicts::Ok(parse_stdout(stdout), stdout.trim().to_string()),
        2 => {
            let reason = stderr.trim();
            Verdicts::Block(
                if reason.is_empty() {
                    "hook gave no reason".to_string()
                } else {
                    reason.to_string()
                },
                parse_stdout(stdout),
            )
        }
        other => {
            let reason = stderr.trim();
            Verdicts::Warn(if reason.is_empty() {
                format!("exit {other}")
            } else {
                format!("exit {other}: {reason}")
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_text_and_empty_output_are_not_json() {
        assert!(parse_stdout("").is_none());
        assert!(parse_stdout("all clear").is_none());
        assert!(parse_stdout("{not json}").is_none());
        assert!(parse_stdout("[1,2]").is_none());
    }

    #[test]
    fn permission_decision_and_updated_input() {
        let out = parse_stdout(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny",
            "permissionDecisionReason":"no","updatedInput":{"command":"ls"},"additionalContext":"ctx"}}"#,
        )
        .unwrap();
        assert_eq!(out.verdict, Some(Verdict::Deny));
        assert_eq!(out.verdict_reason.as_deref(), Some("no"));
        assert_eq!(out.updated_input, Some(json!({"command": "ls"})));
        assert_eq!(out.additional_context.as_deref(), Some("ctx"));
    }

    #[test]
    fn top_level_block_and_continue_false() {
        let out =
            parse_stdout(r#"{"decision":"block","reason":"r","continue":false,"stopReason":"s"}"#)
                .unwrap();
        assert!(out.block && out.halt);
        assert_eq!(out.reason.as_deref(), Some("r"));
        assert_eq!(out.stop_reason.as_deref(), Some("s"));
    }

    #[test]
    fn deprecated_approve_maps_to_allow() {
        let out = parse_stdout(r#"{"decision":"approve"}"#).unwrap();
        assert_eq!(out.verdict, Some(Verdict::Allow));
        assert!(!out.block);
    }

    #[test]
    fn wizards_own_updated_args_still_parses() {
        let out = parse_stdout(r#"{"updated_args":{"path":"b"}}"#).unwrap();
        assert_eq!(out.updated_args, Some(json!({"path": "b"})));
        let out = parse_stdout(r#"{"updated_args":"nope"}"#).unwrap();
        assert!(out.updated_args.is_none());
    }

    #[test]
    fn permission_request_decision() {
        let out = parse_stdout(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest",
            "decision":{"behavior":"deny","message":"no"}}}"#,
        )
        .unwrap();
        let decision = out.permission_request.unwrap();
        assert!(!decision.allow);
        assert_eq!(decision.message.as_deref(), Some("no"));
    }

    #[test]
    fn exit_codes_follow_the_table() {
        assert!(matches!(interpret(0, "text", "err"), Verdicts::Ok(None, _)));
        assert!(matches!(
            interpret(2, "", " denied \n"),
            Verdicts::Block(reason, None) if reason == "denied"
        ));
        assert!(matches!(
            interpret(2, "", ""),
            Verdicts::Block(reason, _) if reason == "hook gave no reason"
        ));
        assert!(matches!(interpret(1, "", "boom"), Verdicts::Warn(w) if w == "exit 1: boom"));
        assert!(matches!(interpret(3, "", ""), Verdicts::Warn(w) if w == "exit 3"));
    }
}

/// The answer a `--permission-prompt-tool` gives, as the text of its result:
/// `{"behavior":"allow","updatedInput":{...}}` lets the call run (with the new
/// input when there is one), `{"behavior":"deny","message":"..."}` refuses it.
/// Anything else is a refusal, since an unreadable answer must not allow.
pub fn prompt_tool_answer(text: &str) -> Result<Option<Value>, String> {
    let value: Value = serde_json::from_str(text.trim()).map_err(|_| {
        format!("the permission prompt tool answered with something that is not JSON: {text}")
    })?;
    match value.get("behavior").and_then(Value::as_str) {
        Some("allow") => Ok(value.get("updatedInput").filter(|v| v.is_object()).cloned()),
        Some("deny") => Err(value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("The permission prompt tool denied this call.")
            .to_string()),
        _ => Err(format!(
            "the permission prompt tool gave no behavior: {text}"
        )),
    }
}

#[cfg(test)]
mod prompt_tool_tests {
    use super::*;

    #[test]
    fn allow_deny_and_junk() {
        assert_eq!(prompt_tool_answer(r#"{"behavior":"allow"}"#), Ok(None));
        let updated = prompt_tool_answer(r#"{"behavior":"allow","updatedInput":{"command":"ls"}}"#);
        assert_eq!(updated, Ok(Some(serde_json::json!({"command": "ls"}))));
        assert_eq!(
            prompt_tool_answer(r#"{"behavior":"deny","message":"no"}"#),
            Err("no".to_string())
        );
        assert!(prompt_tool_answer("allow").is_err());
        assert!(prompt_tool_answer(r#"{"behavior":"maybe"}"#).is_err());
    }
}
