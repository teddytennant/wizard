//! `PushNotification`, `SendUserFile` and `ReportFindings`: getting output to
//! the user by something other than the transcript.
//!
//! Wizard has no phone transport and no paired remote device, so what these do
//! is what the attached surface can: a [`AgentEvent::Notice`] every surface
//! renders, and a desktop notification where one is available. The results say
//! what was and was not reached.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{invalid, str_arg};
use crate::agent::AgentEvent;
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, resolve_path};

/// Mobile systems cut text past about this.
const NOTIFICATION_CHARS: usize = 200;

/// Most findings one `ReportFindings` call may carry.
const MAX_FINDINGS: usize = 32;

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(PushNotificationTool),
        Arc::new(SendUserFileTool),
        Arc::new(ReportFindingsTool),
    ]
}

async fn notice(ctx: &ToolContext, text: String) -> bool {
    match &ctx.events {
        Some(events) => events.send(AgentEvent::Notice(text)).await.is_ok(),
        None => false,
    }
}

/// Raise a desktop notification with whatever the platform offers. `false` when
/// nothing could be run; never an error.
async fn desktop_notify(title: &str, body: &str) -> bool {
    let (program, args): (&str, Vec<String>) = if cfg!(target_os = "macos") {
        let escape = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        (
            "osascript",
            vec![
                "-e".into(),
                format!(
                    "display notification \"{}\" with title \"{}\"",
                    escape(body),
                    escape(title)
                ),
            ],
        )
    } else {
        ("notify-send", vec![title.into(), body.into()])
    };
    let run = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    matches!(
        tokio::time::timeout(Duration::from_secs(3), run).await,
        Ok(Ok(status)) if status.success()
    )
}

/// `PushNotification`.
pub struct PushNotificationTool;

#[async_trait]
impl Tool for PushNotificationTool {
    fn name(&self) -> &str {
        "PushNotification"
    }

    fn description(&self) -> &str {
        "Tell the user something while they are away: a long task finished, or you need a \
         decision. Shows a notice in the session and raises a desktop notification when the \
         platform has one. There is no phone push. Keep message under 200 characters."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "What to tell the user. Under 200 characters." },
                "status": { "type": "string", "enum": ["proactive"] }
            },
            "required": ["message"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(message) = str_arg(&args, "message")
            .map(str::trim)
            .filter(|m| !m.is_empty())
        else {
            return Ok(invalid(self.name(), "message is required"));
        };
        let shown = notice(ctx, format!("notification: {message}")).await;
        let short: String = message.chars().take(NOTIFICATION_CHARS).collect();
        let desktop = desktop_notify("Wizard", &short).await;
        Ok(ToolOutput::ok(format!(
            "Notification sent. Session notice: {}. Desktop notification: {}. Phone push: no \
             transport in Wizard.",
            if shown {
                "shown"
            } else {
                "no surface attached"
            },
            if desktop { "sent" } else { "unavailable" },
        )))
    }
}

/// `SendUserFile`.
pub struct SendUserFileTool;

#[async_trait]
impl Tool for SendUserFileTool {
    fn name(&self) -> &str {
        "SendUserFile"
    }

    fn description(&self) -> &str {
        "Point the user at a file you made: a report, diagram, screenshot, build output. \
         Wizard has no remote device to push it to, so this checks the file exists and shows \
         the user its absolute path and your caption in the session."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file to send." },
                "caption": { "type": "string", "description": "A line saying what it is." },
                "display": { "type": "string", "enum": ["render", "attach"], "description": "Accepted and ignored." }
            },
            "required": ["path"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(path_arg) = str_arg(&args, "path") else {
            return Ok(invalid(self.name(), "path is required"));
        };
        let path = resolve_path(ctx, path_arg);
        let meta = match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => {
                return Ok(ToolOutput::error(format!(
                    "{} is not a file",
                    path.display()
                )));
            }
            Err(err) => {
                return Ok(ToolOutput::error(format!(
                    "cannot send {}: {err}",
                    path.display()
                )));
            }
        };
        let caption = str_arg(&args, "caption").unwrap_or_default();
        let line = if caption.is_empty() {
            format!("file: {} ({} bytes)", path.display(), meta.len())
        } else {
            format!("file: {} ({} bytes), {caption}", path.display(), meta.len())
        };
        let shown = notice(ctx, line).await;
        Ok(ToolOutput::ok(format!(
            "{} ({} bytes) {}. No remote device is attached, so it was not copied anywhere.",
            path.display(),
            meta.len(),
            if shown {
                "was shown to the user"
            } else {
                "was not shown: no surface attached"
            }
        )))
    }
}

/// `ReportFindings`.
pub struct ReportFindingsTool;

#[async_trait]
impl Tool for ReportFindingsTool {
    fn name(&self) -> &str {
        "ReportFindings"
    }

    fn description(&self) -> &str {
        "Report code-review findings as a list, most severe first, once, when the review \
         instructions say to. Empty findings means none survived verification. Do not also \
         print them as text."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "level": { "type": "string", "enum": ["low", "medium", "high", "xhigh", "max"], "description": "Effort level the review ran at" },
                "findings": {
                    "type": "array",
                    "maxItems": MAX_FINDINGS,
                    "description": "Verified findings, most severe first; empty if none survived",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": { "type": "string", "description": "Repo-relative path" },
                            "line": { "type": "integer", "description": "1-indexed line the finding anchors to" },
                            "summary": { "type": "string", "description": "One sentence stating the defect" },
                            "short_summary": { "type": "string", "maxLength": 60, "description": "The claim alone, 60 characters or fewer" },
                            "failure_scenario": { "type": "string", "description": "Concrete inputs and state, then wrong output or crash" },
                            "category": { "type": "string", "maxLength": 40, "description": "kebab-case slug: correctness, simplification, efficiency, test-coverage" },
                            "verdict": { "type": "string", "enum": ["CONFIRMED", "PLAUSIBLE"] },
                            "outcome": { "type": "string", "enum": ["fixed", "skipped", "no_change_needed"], "description": "Only when re-reporting after applying fixes" }
                        },
                        "required": ["file", "summary", "failure_scenario"]
                    }
                }
            },
            "required": ["findings"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(findings) = args.get("findings").and_then(Value::as_array) else {
            return Ok(invalid(self.name(), "findings must be an array"));
        };
        if findings.len() > MAX_FINDINGS {
            return Ok(invalid(
                self.name(),
                format!(
                    "at most {MAX_FINDINGS} findings per call, got {}",
                    findings.len()
                ),
            ));
        }
        for (i, finding) in findings.iter().enumerate() {
            for key in ["file", "summary", "failure_scenario"] {
                if str_arg(finding, key).is_none() {
                    return Ok(invalid(
                        self.name(),
                        format!("findings[{i}].{key} is required"),
                    ));
                }
            }
        }
        if findings.is_empty() {
            return Ok(ToolOutput::ok("Reported 0 findings."));
        }
        let mut out = format!(
            "Reported {} finding{}{}:",
            findings.len(),
            if findings.len() == 1 { "" } else { "s" },
            str_arg(&args, "level")
                .map(|level| format!(" (review level {level})"))
                .unwrap_or_default()
        );
        for (i, finding) in findings.iter().enumerate() {
            let at = match finding.get("line").and_then(Value::as_i64) {
                Some(line) => format!("{}:{line}", str_arg(finding, "file").unwrap_or_default()),
                None => str_arg(finding, "file").unwrap_or_default().to_string(),
            };
            let tags: Vec<&str> = ["category", "verdict", "outcome"]
                .iter()
                .filter_map(|k| str_arg(finding, k))
                .collect();
            let tags = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join(", "))
            };
            out.push_str(&format!(
                "\n{}. {at}{tags} {}\n   Failure: {}",
                i + 1,
                str_arg(finding, "summary").unwrap_or_default(),
                str_arg(finding, "failure_scenario").unwrap_or_default()
            ));
        }
        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_push_notification_reaches_the_session_as_a_notice() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let ctx = ToolContext::new(std::env::temp_dir()).with_events(tx);
        let out = PushNotificationTool
            .execute(
                json!({ "message": "build done", "status": "proactive" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.contains("Session notice: shown"),
            "{}",
            out.content
        );
        assert!(out.content.contains("Phone push: no transport"));
        match rx.try_recv() {
            Ok(AgentEvent::Notice(text)) => assert_eq!(text, "notification: build done"),
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_notification_needs_a_message() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let out = PushNotificationTool
            .execute(json!({ "message": "  " }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn send_user_file_checks_the_file_and_says_nothing_was_copied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("r.md"), "# report").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let ctx = ToolContext::new(dir.path()).with_events(tx);
        let out = SendUserFileTool
            .execute(json!({ "path": "r.md", "caption": "the report" }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("was shown to the user"));
        assert!(out.content.contains("not copied anywhere"));
        match rx.try_recv() {
            Ok(AgentEvent::Notice(text)) => {
                assert!(text.ends_with("(8 bytes), the report"), "{text}");
            }
            other => panic!("expected a notice, got {other:?}"),
        }
        let missing = SendUserFileTool
            .execute(json!({ "path": "nope.md" }), &ctx)
            .await
            .unwrap();
        assert!(missing.is_error);
    }

    #[tokio::test]
    async fn report_findings_lists_them_in_order_and_checks_the_shape() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let none = ReportFindingsTool
            .execute(json!({ "findings": [] }), &ctx)
            .await
            .unwrap();
        assert_eq!(none.content, "Reported 0 findings.");

        let out = ReportFindingsTool
            .execute(
                json!({
                    "level": "high",
                    "findings": [
                        { "file": "a.rs", "line": 3, "summary": "off by one",
                          "failure_scenario": "len 0 panics", "category": "correctness",
                          "verdict": "CONFIRMED" },
                        { "file": "b.rs", "summary": "dead branch",
                          "failure_scenario": "never taken" }
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            out.content,
            "Reported 2 findings (review level high):\n\
             1. a.rs:3 [correctness, CONFIRMED] off by one\n   Failure: len 0 panics\n\
             2. b.rs dead branch\n   Failure: never taken"
        );

        let bad = ReportFindingsTool
            .execute(
                json!({ "findings": [{ "file": "a.rs", "summary": "s" }] }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(bad.is_error);
        assert!(
            bad.content
                .contains("findings[0].failure_scenario is required")
        );
    }
}
