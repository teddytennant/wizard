//! Preference log of the user's judgments.
//!
//! Same on-disk JSONL as the `turing` CLI. Compile is offline. Writes go
//! through [`crate::turing`], never through a persona store.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::commands::TuringAction;
use crate::turing;

use super::{
    MAX_OUTPUT_BYTES, Tool, ToolContext, ToolError, ToolOutput, parse_args, truncate_output,
};

/// Native `turing` tool.
pub struct TuringTool;

#[derive(Debug, Deserialize)]
struct TuringArgs {
    action: String,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    why: Option<String>,
    #[serde(default)]
    excerpt: Option<String>,
    #[serde(default)]
    a: Option<String>,
    #[serde(default)]
    b: Option<String>,
    #[serde(default)]
    winner: Option<String>,
    #[serde(default)]
    skeleton: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[async_trait]
impl Tool for TuringTool {
    fn name(&self) -> &str {
        "turing"
    }

    fn description(&self) -> &str {
        "Preference log of the user's judgments (answers, A/B pairs, vetoes, \
         endorsements, attractors). Same JSONL as the turing CLI. Compile a \
         short slice before writing user-facing prose. On reject, veto with a \
         why. Never dump the whole log, never ICL old prose, never save a \
         persona. Read `manual` topic `turing` before first use."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "compile",
                        "status",
                        "question",
                        "answer",
                        "veto",
                        "endorse",
                        "pair",
                        "attractor"
                    ],
                    "description": "compile (optional task), status, question, answer (id+text), veto (target+why, optional excerpt), endorse (target), pair (a+b+winner+why), attractor (skeleton, optional note)"
                },
                "task": { "type": "string", "description": "optional compile ranking hint" },
                "id": { "type": "string", "description": "question id for answer" },
                "text": { "type": "string", "description": "answer text" },
                "target": { "type": "string", "description": "veto or endorse target" },
                "why": { "type": "string", "description": "veto or pair why" },
                "excerpt": { "type": "string", "description": "optional veto excerpt" },
                "a": { "type": "string", "description": "pair option a" },
                "b": { "type": "string", "description": "pair option b" },
                "winner": { "type": "string", "description": "a or b" },
                "skeleton": { "type": "string", "description": "attractor skeleton" },
                "note": { "type": "string", "description": "optional attractor note" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let parsed: TuringArgs = parse_args(self.name(), args)?;
        let action = action_from_args(&parsed).map_err(|message| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message,
        })?;
        match turing::run(&action) {
            Ok(text) => Ok(ToolOutput::ok(truncate_output(text, MAX_OUTPUT_BYTES))),
            Err(err) => Ok(ToolOutput::error(format!("{err:#}"))),
        }
    }
}

fn action_from_args(args: &TuringArgs) -> Result<TuringAction, String> {
    match args.action.as_str() {
        "compile" => Ok(TuringAction::Compile(args.task.clone().unwrap_or_default())),
        "status" => Ok(TuringAction::Status),
        "question" => Ok(TuringAction::Question),
        "answer" => Ok(TuringAction::Answer {
            id: required(&args.id, "answer needs id")?,
            text: required(&args.text, "answer needs text")?,
        }),
        "veto" => Ok(TuringAction::Veto {
            target: required(&args.target, "veto needs target")?,
            why: required(&args.why, "veto needs why")?,
            excerpt: args.excerpt.clone(),
        }),
        "endorse" => Ok(TuringAction::Endorse {
            target: required(&args.target, "endorse needs target")?,
        }),
        "pair" => Ok(TuringAction::Pair {
            a: required(&args.a, "pair needs a")?,
            b: required(&args.b, "pair needs b")?,
            winner: required(&args.winner, "pair needs winner")?,
            why: required(&args.why, "pair needs why")?,
        }),
        "attractor" => Ok(TuringAction::Attractor {
            skeleton: required(&args.skeleton, "attractor needs skeleton")?,
            note: args.note.clone(),
        }),
        other => Err(format!(
            "unknown action '{other}': compile, status, question, answer, veto, endorse, pair, attractor"
        )),
    }
}

fn required(value: &Option<String>, message: &str) -> Result<String, String> {
    match value {
        Some(text) if !text.trim().is_empty() => Ok(text.clone()),
        _ => Err(message.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir())
    }

    #[test]
    fn description_points_at_the_manual() {
        let desc = TuringTool.description();
        assert!(desc.contains("`manual` topic `turing`"), "{desc}");
        assert!(desc.contains("persona"), "{desc}");
        assert!(!desc.contains("PRINCIPAL.md"), "{desc}");
    }

    #[tokio::test]
    async fn compile_empty_log_is_honest() {
        let _g = crate::turing::TestLogGuard::new();
        let out = TuringTool
            .execute(json!({"action": "compile"}), &ctx())
            .await
            .expect("execute");
        assert!(out.content.contains("no rows"), "{}", out.content);
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn veto_then_compile_keeps_the_why() {
        let _g = crate::turing::TestLogGuard::new();
        let tool = TuringTool;
        tool.execute(
            json!({
                "action": "veto",
                "target": "src/jokes.rs",
                "why": "the pun does not land"
            }),
            &ctx(),
        )
        .await
        .expect("veto");
        let out = tool
            .execute(json!({"action": "compile", "task": "jokes"}), &ctx())
            .await
            .expect("compile");
        assert!(
            out.content.contains("the pun does not land"),
            "{}",
            out.content
        );
        assert!(
            !out.content.to_lowercase().contains("write like"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn unknown_action_is_invalid_args() {
        let err = TuringTool
            .execute(json!({"action": "persona"}), &ctx())
            .await
            .expect_err("persona is not an action");
        match err {
            ToolError::InvalidArgs { tool, message } => {
                assert_eq!(tool, "turing");
                assert!(message.contains("unknown action"), "{message}");
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pair_records_the_winner() {
        let _g = crate::turing::TestLogGuard::new();
        let out = TuringTool
            .execute(
                json!({
                    "action": "pair",
                    "a": "short commit",
                    "b": "essay commit",
                    "winner": "a",
                    "why": "the body restated the title"
                }),
                &ctx(),
            )
            .await
            .expect("pair");
        assert!(!out.is_error, "{}", out.content);
        let status = TuringTool
            .execute(json!({"action": "status"}), &ctx())
            .await
            .expect("status");
        assert!(status.content.contains("pair: 1"), "{}", status.content);
    }
}
