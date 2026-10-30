//! Claude Code names for tools Wizard already has.
//!
//! An [`AliasTool`] advertises Claude's name and input schema, translates the
//! arguments to the Wizard tool's, and runs that tool. Behaviour, access class
//! and checkpointing are the inner tool's.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{invalid, str_arg};
use crate::tools::registry::ToolRegistry;
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolKind, ToolOutput};

/// Turns Claude's arguments into the inner tool's, or says what is wrong.
type MapArgs = fn(&Value) -> Result<Value, String>;

/// Rewrites the inner tool's answer, given the arguments as Claude sent them.
type Finish = fn(ToolOutput, &Value) -> ToolOutput;

/// A Wizard tool under a Claude Code name.
pub struct AliasTool {
    name: &'static str,
    description: &'static str,
    schema: Value,
    inner: Arc<dyn Tool>,
    map: MapArgs,
    finish: Option<Finish>,
    /// Set when the alias must be callable where the inner tool is not (the
    /// plan-mode gate lets only read-only tools through, and `ExitPlanMode`
    /// has to get through it).
    access: Option<ToolAccess>,
}

#[async_trait]
impl Tool for AliasTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    fn access(&self) -> ToolAccess {
        self.access.unwrap_or_else(|| self.inner.access())
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Native
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args = if args.is_null() { json!({}) } else { args };
        let mapped = match (self.map)(&args) {
            Ok(mapped) => mapped,
            Err(message) => return Ok(invalid(self.name, message)),
        };
        let out = self.inner.execute(mapped, ctx).await?;
        Ok(match self.finish {
            Some(finish) => finish(out, &args),
            None => out,
        })
    }
}

/// Every alias whose inner tool is in `registry`.
pub fn aliases(registry: &ToolRegistry) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    let mut add = |inner: &str,
                   name: &'static str,
                   description: &'static str,
                   schema: Value,
                   map: MapArgs,
                   finish: Option<Finish>,
                   access: Option<ToolAccess>| {
        if let Some(inner) = registry.get(inner) {
            out.push(Arc::new(AliasTool {
                name,
                description,
                schema,
                inner: Arc::clone(inner),
                map,
                finish,
                access,
            }));
        }
    };

    add(
        "execute",
        "Bash",
        "Run a shell command in the project root. Returns stdout, stderr and the exit code. \
         A command still running after the foreground budget becomes a background task; read it \
         with `task_output`, stop it with TaskStop.",
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command to execute" },
                "timeout": { "type": "number", "description": "Optional timeout in milliseconds (max 600000 for a foreground command)" },
                "description": { "type": "string", "description": "What the command does, in a few plain words" },
                "run_in_background": { "type": "boolean", "description": "Set to true to run this command in the background." },
                "dangerouslyDisableSandbox": { "type": "boolean", "description": "Accepted and ignored; Wizard has no sandbox to disable." }
            },
            "required": ["command"]
        }),
        map_bash,
        Some(finish_bash),
        None,
    );
    add(
        "write_file",
        "Write",
        "Create or overwrite a file, creating parent directories as needed.",
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "The absolute path to the file to write (must be absolute, not relative)" },
                "content": { "type": "string", "description": "The content to write to the file" }
            },
            "required": ["file_path", "content"]
        }),
        map_write,
        None,
        None,
    );
    add(
        "web_fetch",
        "WebFetch",
        "Fetch a URL and return its content, HTML as markdown. Apply `prompt` to the content \
         yourself; unlike Claude Code there is no second model pass.",
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to fetch content from" },
                "prompt": { "type": "string", "description": "The prompt to run on the fetched content" },
                "offset": { "type": "integer", "minimum": 0, "description": "Character position in the page text to start reading from" }
            },
            "required": ["url", "prompt"]
        }),
        map_web_fetch,
        Some(finish_web_fetch),
        None,
    );
    add(
        "web_search",
        "WebSearch",
        "Search the web with the configured backend and return titles, URLs and snippets.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 2, "description": "The search query to use" },
                "allowed_domains": { "type": "array", "items": { "type": "string" }, "description": "Only include search results from these domains" },
                "blocked_domains": { "type": "array", "items": { "type": "string" }, "description": "Never include search results from these domains" }
            },
            "required": ["query"]
        }),
        map_web_search,
        None,
        None,
    );
    add(
        "spawn_subagent",
        "Agent",
        "Launch a subagent for a self-contained task and get its report. subagent_type picks \
         the definition; general-purpose, Explore and Plan all run Wizard's built-in worker.",
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "A short (3-5 word) description of the task" },
                "prompt": { "type": "string", "description": "The task for the agent to perform" },
                "subagent_type": { "type": "string", "description": "The type of specialized agent to use for this task" },
                "run_in_background": { "type": "boolean", "description": "Run detached and return at once; the report arrives later" },
                "model": { "type": "string", "description": "Accepted and ignored; the subagent uses the session model" },
                "effort": { "type": "string", "description": "Accepted and ignored" },
                "name": { "type": "string", "description": "Accepted and ignored" },
                "team_name": { "type": "string", "description": "Accepted and ignored" },
                "mode": { "type": "string", "description": "Accepted and ignored" },
                "isolation": { "type": "string", "enum": ["worktree", "remote"], "description": "Accepted and ignored" }
            },
            "required": ["description", "prompt"]
        }),
        map_agent,
        None,
        None,
    );
    add(
        "todo",
        "TodoWrite",
        "Replace the working todo list. Pass every item, completed ones included.",
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The updated todo list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                            "activeForm": { "type": "string" }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        }),
        map_todo_write,
        None,
        None,
    );
    add(
        "interview",
        "AskUserQuestion",
        "Ask the user one to six multiple-choice or free-text questions and wait for the answers.",
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string" },
                            "header": { "type": "string" },
                            "multiSelect": { "type": "boolean" },
                            "options": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string" },
                                        "description": { "type": "string" }
                                    },
                                    "required": ["label"]
                                }
                            }
                        },
                        "required": ["question"]
                    }
                }
            },
            "required": ["questions"]
        }),
        map_ask_user,
        Some(finish_ask_user),
        None,
    );
    add(
        "exit_plan",
        "ExitPlanMode",
        "Present the finished plan for review and leave plan mode on approval.",
        json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "The plan, as markdown" },
                "allowedPrompts": { "type": "array", "items": { "type": "object" }, "description": "Accepted and ignored" }
            }
        }),
        map_exit_plan,
        None,
        Some(ToolAccess::ReadOnly),
    );
    out
}

fn map_bash(args: &Value) -> Result<Value, String> {
    let command = str_arg(args, "command").ok_or("command is required")?;
    let mut mapped = json!({ "command": command });
    if let Some(ms) = args.get("timeout").and_then(Value::as_f64) {
        if ms <= 0.0 {
            return Err("timeout must be positive".to_string());
        }
        mapped["timeout_secs"] = json!(((ms / 1000.0).ceil() as u64).clamp(1, 600));
        // Claude's `timeout` kills the command; Wizard's own budget hands it
        // to a background task. Naming one picks Claude's.
        mapped["kill_on_timeout"] = json!(true);
    }
    if let Some(background) = args.get("run_in_background").and_then(Value::as_bool) {
        mapped["run_in_background"] = json!(background);
    }
    Ok(mapped)
}

/// Claude's layout for a finished command: the two streams merged under no
/// label, `Exit code N` on its own first line when the command failed, and a
/// fixed line for a command that printed nothing. Wizard's `execute` reports
/// the same facts in the other order (`exit code: N` last, a `stderr:` section).
fn finish_bash(mut out: ToolOutput, _args: &Value) -> ToolOutput {
    const EMPTY: &str = "(command succeeded with no output)";
    if out.content == EMPTY {
        out.content = "(Bash completed with no output)".to_string();
        return out;
    }
    let (body, last) = match out.content.rsplit_once('\n') {
        Some((head, last)) => (head, last),
        None => ("", out.content.as_str()),
    };
    let ending = if !out.is_error {
        None
    } else if let Some(code) = last.strip_prefix("exit code: ") {
        Some((code.trim().to_string(), None))
    } else if let Some(rest) = last.strip_prefix("command timed out after ") {
        rest.split_once("s and was killed").map(|(secs, _)| {
            (
                "143".to_string(),
                Some(format!("Command timed out after {secs}s")),
            )
        })
    } else {
        None
    };
    let (code, trailer) = match ending {
        Some(parts) => parts,
        None if out.is_error || !out.content.contains("stderr:") => return out,
        None => (String::new(), None),
    };
    let merged = if code.is_empty() {
        out.content.as_str()
    } else {
        body
    };
    let merged = match merged.strip_prefix("stderr:\n") {
        Some(rest) => rest.to_string(),
        None => merged.replacen("\nstderr:\n", "\n", 1),
    };
    let mut text = String::new();
    if !code.is_empty() {
        text.push_str(&format!("Exit code {code}"));
    }
    for part in [Some(merged), trailer].into_iter().flatten() {
        if part.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&part);
    }
    out.content = text;
    out
}

fn map_write(args: &Value) -> Result<Value, String> {
    let path = str_arg(args, "file_path").ok_or("file_path is required")?;
    let content = str_arg(args, "content").ok_or("content is required")?;
    Ok(json!({ "path": path, "content": content }))
}

fn map_web_fetch(args: &Value) -> Result<Value, String> {
    let url = str_arg(args, "url").ok_or("url is required")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("Invalid URL: {url}"));
    }
    Ok(json!({ "url": url }))
}

/// Put the prompt above the page, since nothing here reads the page for it,
/// and honour `offset` by dropping that many characters from the front.
fn finish_web_fetch(mut out: ToolOutput, args: &Value) -> ToolOutput {
    if out.is_error {
        return out;
    }
    if let Some(offset) = args.get("offset").and_then(Value::as_u64) {
        let skipped: String = out.content.chars().skip(offset as usize).collect();
        out.content = skipped;
    }
    let prompt = str_arg(args, "prompt").unwrap_or_default();
    out.content = format!("Prompt for this page: {prompt}\n\n{}", out.content);
    out
}

fn map_web_search(args: &Value) -> Result<Value, String> {
    let query = str_arg(args, "query").ok_or("query is required")?;
    if query.chars().count() < 2 {
        return Err("query must be at least 2 characters".to_string());
    }
    let domains = |key: &str| -> Vec<String> {
        args.get(key)
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut query = query.to_string();
    let allowed = domains("allowed_domains");
    if !allowed.is_empty() {
        let sites: Vec<String> = allowed.iter().map(|d| format!("site:{d}")).collect();
        query = format!("{query} ({})", sites.join(" OR "));
    }
    for blocked in domains("blocked_domains") {
        query = format!("{query} -site:{blocked}");
    }
    Ok(json!({ "query": query }))
}

fn map_agent(args: &Value) -> Result<Value, String> {
    let prompt = str_arg(args, "prompt").ok_or("prompt is required")?;
    let subagent = match str_arg(args, "subagent_type") {
        None | Some("general-purpose" | "Explore" | "Plan" | "claude") => "worker",
        Some(other) => other,
    };
    let task = match str_arg(args, "description") {
        Some(description) if !description.trim().is_empty() => {
            format!("{description}\n\n{prompt}")
        }
        _ => prompt.to_string(),
    };
    let background = args
        .get("run_in_background")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(json!({ "subagent": subagent, "task": task, "background": background }))
}

fn map_todo_write(args: &Value) -> Result<Value, String> {
    let todos = args
        .get("todos")
        .and_then(Value::as_array)
        .ok_or("todos must be an array")?;
    let items: Vec<Value> = todos
        .iter()
        .map(|todo| {
            json!({
                "content": str_arg(todo, "content").unwrap_or_default(),
                "status": str_arg(todo, "status").unwrap_or("pending"),
            })
        })
        .collect();
    Ok(json!({ "action": "write", "items": items }))
}

/// Claude's sentence for answered questions:
/// `Your questions have been answered: "Q"="A", "Q2"="A2". You can now continue
/// with these answers in mind.` Wizard's `interview` reports the same answers as
/// `Q:`/`A:` pairs and tells the model to go on to `exit_plan`, which is a
/// plan-mode instruction an `AskUserQuestion` caller never asked for.
fn finish_ask_user(mut out: ToolOutput, _args: &Value) -> ToolOutput {
    let Some(body) = out
        .content
        .strip_prefix("The user answered your questions:\n")
    else {
        return out;
    };
    let mut pairs = Vec::new();
    let mut question = None;
    for line in body.lines() {
        if let Some(q) = line.strip_prefix("Q: ") {
            question = Some(q.to_string());
        } else if let (Some(a), Some(q)) = (line.strip_prefix("A: "), question.take()) {
            let answer = if a == "(skipped)" {
                "(no option selected)"
            } else {
                a
            };
            pairs.push(format!("\"{q}\"=\"{answer}\""));
        }
    }
    out.content = format!(
        "Your questions have been answered: {}. You can now continue with these answers in mind.",
        pairs.join(", ")
    );
    out
}

fn map_ask_user(args: &Value) -> Result<Value, String> {
    let questions = args
        .get("questions")
        .and_then(Value::as_array)
        .ok_or("questions must be an array")?;
    let mapped: Vec<Value> = questions
        .iter()
        .map(|question| {
            let mut text = str_arg(question, "question")
                .unwrap_or_default()
                .to_string();
            if question.get("multiSelect").and_then(Value::as_bool) == Some(true) {
                text.push_str(" (pick any number)");
            }
            let options: Vec<&str> = question
                .get("options")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(|o| str_arg(o, "label")).collect())
                .unwrap_or_default();
            let mut out = json!({ "question": text });
            if !options.is_empty() {
                out["options"] = json!(options);
            }
            out
        })
        .collect();
    Ok(json!({ "questions": mapped }))
}

fn map_exit_plan(args: &Value) -> Result<Value, String> {
    match str_arg(args, "plan") {
        Some(plan) if !plan.trim().is_empty() => Ok(json!({ "plan": plan })),
        _ => Err("pass the plan as `plan`".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_timeout_is_milliseconds_rounded_up_to_seconds() {
        let mapped = map_bash(&json!({ "command": "ls", "timeout": 1500 })).unwrap();
        assert_eq!(mapped["timeout_secs"], 2);
        let capped = map_bash(&json!({ "command": "ls", "timeout": 9_000_000 })).unwrap();
        assert_eq!(capped["timeout_secs"], 600);
        assert!(map_bash(&json!({})).is_err());
    }

    fn bash(content: &str, is_error: bool) -> ToolOutput {
        let out = if is_error {
            ToolOutput::error(content)
        } else {
            ToolOutput::ok(content)
        };
        finish_bash(out, &json!({ "command": "x" }))
    }

    #[test]
    fn bash_names_a_timeout_it_will_enforce() {
        let mapped = map_bash(&json!({ "command": "ls", "timeout": 1500 })).unwrap();
        assert_eq!(mapped["kill_on_timeout"], true);
        let plain = map_bash(&json!({ "command": "ls" })).unwrap();
        assert!(plain.get("kill_on_timeout").is_none());
    }

    #[test]
    fn bash_failures_lead_with_the_exit_code() {
        // oracle: `echo out; echo err >&2; exit 3` through claude mcp serve and
        // through a model turn is "Exit code 3\nout\nerr"
        let out = bash("out\nstderr:\nerr\nexit code: 3", true);
        assert_eq!(out.content, "Exit code 3\nout\nerr");
        assert!(out.is_error);
        assert_eq!(bash("exit code: 1", true).content, "Exit code 1");
        assert_eq!(
            bash("stderr:\nbash: zz: command not found\nexit code: 127", true).content,
            "Exit code 127\nbash: zz: command not found"
        );
    }

    #[test]
    fn bash_merges_stderr_into_the_output() {
        // oracle: `echo out; echo err >&2` is "out\nerr", no label
        assert_eq!(bash("out\nstderr:\nerr", false).content, "out\nerr");
        assert_eq!(bash("stderr:\nonly", false).content, "only");
    }

    #[test]
    fn bash_says_when_there_was_no_output() {
        // oracle: "(Bash completed with no output)"
        assert_eq!(
            bash("(command succeeded with no output)", false).content,
            "(Bash completed with no output)"
        );
    }

    #[test]
    fn bash_timeouts_read_like_claudes() {
        // oracle: `sleep 5` with timeout 1000 is "Exit code 143\nCommand timed out after 1s"
        let out = bash(
            "command timed out after 1s and was killed (no output produced)",
            true,
        );
        assert_eq!(out.content, "Exit code 143\nCommand timed out after 1s");
        let partial = bash(
            "a\ncommand timed out after 2s and was killed; output above is partial",
            true,
        );
        assert_eq!(
            partial.content,
            "Exit code 143\na\nCommand timed out after 2s"
        );
    }

    #[test]
    fn bash_leaves_a_background_start_alone() {
        let text = "Background task #3 started: sleep 9\nYou will be notified when it finishes";
        assert_eq!(bash(text, false).content, text);
    }

    #[test]
    fn web_fetch_rejects_a_non_url_like_claude_code() {
        let err = map_web_fetch(&json!({ "url": "notaurl", "prompt": "p" })).unwrap_err();
        assert_eq!(err, "Invalid URL: notaurl");
    }

    #[test]
    fn web_fetch_puts_the_prompt_above_the_page_and_honours_offset() {
        let out = finish_web_fetch(
            ToolOutput::ok("abcdef"),
            &json!({ "prompt": "summarise", "offset": 2 }),
        );
        assert_eq!(out.content, "Prompt for this page: summarise\n\ncdef");
    }

    #[test]
    fn web_search_domains_become_search_operators() {
        let mapped = map_web_search(&json!({
            "query": "rust async",
            "allowed_domains": ["docs.rs", "rust-lang.org"],
            "blocked_domains": ["spam.example"],
        }))
        .unwrap();
        assert_eq!(
            mapped["query"],
            "rust async (site:docs.rs OR site:rust-lang.org) -site:spam.example"
        );
        assert!(map_web_search(&json!({ "query": "x" })).is_err());
    }

    #[test]
    fn agent_maps_claude_types_to_the_builtin_worker() {
        for kind in ["general-purpose", "Explore", "Plan"] {
            let mapped =
                map_agent(&json!({ "description": "d", "prompt": "do it", "subagent_type": kind }))
                    .unwrap();
            assert_eq!(mapped["subagent"], "worker");
            assert_eq!(mapped["task"], "d\n\ndo it");
        }
        let custom = map_agent(&json!({ "prompt": "p", "subagent_type": "reviewer" })).unwrap();
        assert_eq!(custom["subagent"], "reviewer");
        assert_eq!(custom["background"], false);
    }

    #[test]
    fn todo_write_becomes_a_todo_write_action() {
        let mapped = map_todo_write(&json!({
            "todos": [{ "content": "a", "status": "in_progress", "activeForm": "doing a" }]
        }))
        .unwrap();
        assert_eq!(mapped["action"], "write");
        assert_eq!(mapped["items"][0]["status"], "in_progress");
        assert!(mapped["items"][0].get("activeForm").is_none());
    }

    #[test]
    fn answered_questions_read_like_claudes() {
        // oracle: AskUserQuestion answered "A" to "Which?" in an interactive turn
        let out = finish_ask_user(
            ToolOutput::ok(
                "The user answered your questions:\n\nQ: Which?\nA: A\n\nQ: Why?\nA: (skipped)\n\nIncorporate these answers into your plan, then call exit_plan.",
            ),
            &json!({}),
        );
        assert_eq!(
            out.content,
            "Your questions have been answered: \"Which?\"=\"A\", \"Why?\"=\"(no option selected)\". You can now continue with these answers in mind."
        );
        let declined = finish_ask_user(
            ToolOutput::ok("The user dismissed the interview"),
            &json!({}),
        );
        assert_eq!(declined.content, "The user dismissed the interview");
    }

    #[test]
    fn ask_user_question_keeps_the_option_labels() {
        let mapped = map_ask_user(&json!({
            "questions": [{
                "question": "Which?",
                "multiSelect": true,
                "options": [{ "label": "A", "description": "x" }, { "label": "B" }]
            }]
        }))
        .unwrap();
        assert_eq!(
            mapped["questions"][0]["question"],
            "Which? (pick any number)"
        );
        assert_eq!(mapped["questions"][0]["options"], json!(["A", "B"]));
    }

    #[test]
    fn exit_plan_mode_forwards_the_plan() {
        let mapped = map_exit_plan(&json!({ "plan": "# plan" })).unwrap();
        assert_eq!(mapped["plan"], "# plan");
    }
}
