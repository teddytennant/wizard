//! What a tool call's text means beyond its status: a refusal, an answered question, an
//! approved plan. The backend only gives output text, so these read it.

use agent_core::ToolCall;
use serde_json::Value;

/// Who refused a call, and what they said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denial {
    /// A person said no at the permission panel; `note` is what they told the model.
    User { note: String },
    /// A rule or the permission mode refused it without asking.
    Rule { why: String },
}

impl Denial {
    pub fn note(&self) -> &str {
        match self {
            Denial::User { note } => note,
            Denial::Rule { why } => why,
        }
    }
}

/// `The user denied this tool call [and said: ...]` is what `backend-claude` answers with.
/// The CLI's own wordings are recognised too, for sessions replayed from disk.
pub fn denial(output: &str) -> Option<Denial> {
    let t = output.trim();
    if let Some(rest) = t.strip_prefix("The user denied this tool call") {
        let note = rest
            .trim_start_matches(" and said:")
            .trim_start_matches('.')
            .trim();
        return Some(Denial::User { note: note.into() });
    }
    if t.starts_with("The user doesn't want to proceed with this tool use") {
        let note = t
            .split_once("the user said:")
            .map(|(_, n)| n.trim().to_string())
            .unwrap_or_default();
        return Some(Denial::User { note });
    }
    if t.starts_with("Permission for this tool use was denied")
        || (t.starts_with("Permission to use ") && t.contains("denied"))
    {
        let why = t.lines().next().unwrap_or("").to_string();
        return Some(Denial::Rule { why });
    }
    None
}

/// One question and what was picked, for the `Ask` card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answered {
    pub header: String,
    pub question: String,
    pub answer: String,
}

pub fn is_ask(c: &ToolCall) -> bool {
    c.name == "AskUserQuestion"
}

pub fn is_plan(c: &ToolCall) -> bool {
    c.name == "ExitPlanMode"
}

/// The answers out of `Your questions have been answered: "Q"="A", "Q2"="B". You can now...`.
pub fn answers(c: &ToolCall) -> Vec<Answered> {
    let out = c.output.as_deref().unwrap_or("");
    let qs: Vec<(String, String)> = c
        .input
        .get("questions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|q| {
                    let s = |k: &str| q.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    (s("header"), s("question"))
                })
                .collect()
        })
        .unwrap_or_default();
    let Some((_, body)) = out.split_once("have been answered:") else {
        return Vec::new();
    };
    let body = body.split(". You can now").next().unwrap_or(body);
    let mut res = Vec::new();
    for (header, q) in &qs {
        let key = format!("\"{q}\"=\"");
        if let Some(i) = body.find(&key) {
            let rest = &body[i + key.len()..];
            // The answer ends at the quote that is followed by `, "` or the end.
            let end = rest
                .find("\", \"")
                .or_else(|| rest.rfind('"'))
                .unwrap_or(rest.len());
            res.push(Answered {
                header: header.clone(),
                question: q.clone(),
                answer: rest[..end].to_string(),
            });
        }
    }
    res
}

/// First line of a plan without markdown marks, for a one-row target.
pub fn plan_title(plan: &str) -> String {
    plan.lines()
        .map(|l| l.trim().trim_start_matches(['#', '-', '*']).trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

pub fn plan_text(c: &ToolCall) -> Option<&str> {
    c.input.get("plan").and_then(Value::as_str)
}

/// `Exit code 2` on the first line of a failed shell call.
pub fn exit_code(out: &str) -> Option<i32> {
    out.lines()
        .next()?
        .strip_prefix("Exit code ")?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn denials_in_both_wordings() {
        assert_eq!(
            denial("The user denied this tool call and said: use a flag"),
            Some(Denial::User {
                note: "use a flag".into()
            })
        );
        assert_eq!(
            denial("The user denied this tool call."),
            Some(Denial::User {
                note: String::new()
            })
        );
        assert!(matches!(
            denial("The user doesn't want to proceed with this tool use. The tool use was rejected.\nTo tell you how to proceed, the user said:\nrun it later"),
            Some(Denial::User { note }) if note == "run it later"
        ));
        assert!(matches!(
            denial("Permission for this tool use was denied. The user has not allowed it."),
            Some(Denial::Rule { .. })
        ));
        assert_eq!(denial("error[E0425]: cannot find value"), None);
    }

    #[test]
    fn answers_come_back_out_of_the_result_text() {
        let c = ToolCall {
            name: "AskUserQuestion".into(),
            input: json!({"questions": [
                {"header": "Lang", "question": "Which language?"},
                {"header": "Extras", "question": "Which extras?"}]}),
            output: Some("Your questions have been answered: \"Which language?\"=\"Rust\", \"Which extras?\"=\"Tests, CI\". You can now continue with these answers in mind.".into()),
            ..Default::default()
        };
        let a = answers(&c);
        assert_eq!(a.len(), 2);
        assert_eq!(
            (a[0].header.as_str(), a[0].answer.as_str()),
            ("Lang", "Rust")
        );
        assert_eq!(a[1].answer, "Tests, CI");
    }

    #[test]
    fn exit_codes_and_plan_titles() {
        assert_eq!(exit_code("Exit code 101\nerror"), Some(101));
        assert_eq!(exit_code("boom"), None);
        assert_eq!(plan_title("\n## Add a flag\n- x"), "Add a flag");
    }
}
