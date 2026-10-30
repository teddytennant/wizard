//! `CronCreate`, `CronList`, `CronDelete` and `ScheduleWakeup` on Wizard's
//! scheduler.
//!
//! Claude Code queues these prompts into the live session. Wizard has no
//! prompt queue to drain into a running session, but it has a scheduler: an
//! entry in `~/.wizard/schedule.toml` that `wizard scheduler` fires as a
//! headless run in the entry's directory. So a job made here is always
//! persisted (`durable` is accepted and changes nothing), runs as its own
//! headless session rather than inside this one, and needs the daemon running.
//! The result says all three, and warns when no daemon is.
//!
//! Jobs made here are the entries named `cron-<id>` (and `wakeup-<id>` for
//! `ScheduleWakeup`); entries made by hand or by `wizard schedule add` are
//! never listed or deleted by these tools.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Local, Timelike, Utc};
use serde_json::{Value, json};

use super::{invalid, str_arg};
use crate::schedule::{self, ScheduleEntry};
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput};

/// Recurring jobs delete themselves after this long, like Claude Code's.
const MAX_AGE_DAYS: i64 = 7;

/// Wall-clock cap on one fired run. A prompt a model queued for later is not
/// one to leave running unattended with no limit.
const RUN_HOURS: f64 = 1.0;

/// `ScheduleWakeup` clamps its delay to this range, as Claude Code does.
const WAKEUP_RANGE: std::ops::RangeInclusive<i64> = 60..=3600;

/// What `ScheduleWakeup` is handed instead of a prompt for an autonomous loop.
const AUTONOMOUS_SENTINEL: &str = "<<autonomous-loop-dynamic>>";

/// What an autonomous wakeup runs.
const AUTONOMOUS_PROMPT: &str = "Continue the autonomous loop in this directory. Check what \
changed since the last tick, advance the work, and when it needs another tick call ScheduleWakeup \
again; call it with stop: true when there is nothing left to do.";

const CRON_PREFIX: &str = "cron-";
const WAKEUP_PREFIX: &str = "wakeup-";

pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CronCreateTool(Store::default())),
        Arc::new(CronListTool(Store::default())),
        Arc::new(CronDeleteTool(Store::default())),
        Arc::new(ScheduleWakeupTool(Store::default())),
    ]
}

/// Where the schedule file is. The default is `~/.wizard/schedule.toml`; a
/// test points it at a temp dir.
#[derive(Default, Clone)]
struct Store(Option<PathBuf>);

impl Store {
    fn path(&self) -> Result<PathBuf, ToolOutput> {
        match &self.0 {
            Some(path) => Ok(path.clone()),
            None => crate::config::Config::schedule_path()
                .map_err(|err| ToolOutput::error(format!("no schedule file: {err:#}"))),
        }
    }

    fn entries(&self, prefix: &str) -> Result<Vec<ScheduleEntry>, ToolOutput> {
        let file = schedule::load_schedule(&self.path()?)
            .map_err(|err| ToolOutput::error(format!("{err:#}")))?;
        Ok(file
            .entries
            .into_iter()
            .filter(|e| e.name.starts_with(prefix))
            .collect())
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// "Every hour at :07"-style description of the common cron shapes, else the
/// expression itself.
fn human_schedule(cron: &str) -> String {
    let f: Vec<&str> = cron.split_whitespace().collect();
    let [min, hour, dom, month, dow] = f[..] else {
        return cron.to_string();
    };
    let every_day = dom == "*" && month == "*" && dow == "*";
    if every_day && min == "*" && hour == "*" {
        return "Every minute".to_string();
    }
    if every_day
        && hour == "*"
        && let Some(n) = min.strip_prefix("*/").and_then(|n| n.parse::<u32>().ok())
    {
        return format!("Every {n} minutes");
    }
    if every_day
        && hour == "*"
        && let Ok(m) = min.parse::<u32>()
    {
        return format!("Every hour at :{m:02}");
    }
    if every_day && let (Ok(m), Ok(h)) = (min.parse::<u32>(), hour.parse::<u32>()) {
        return format!("Every day at {h}:{m:02}");
    }
    cron.to_string()
}

fn next_fire(entry: &ScheduleEntry) -> Option<DateTime<Local>> {
    schedule::parse_cron(&entry.cron)
        .ok()?
        .find_next_occurrence(&Local::now(), false)
        .ok()
}

fn scheduler_note() -> &'static str {
    if schedule::daemon_running() {
        ""
    } else {
        " The scheduler is not running, so nothing fires until `wizard scheduler` is started \
         (`wizard scheduler install` keeps it running)."
    }
}

fn add(store: &Store, entry: ScheduleEntry) -> Result<(), ToolOutput> {
    schedule::add_entry(&store.path()?, entry).map_err(|err| ToolOutput::error(format!("{err:#}")))
}

/// `CronCreate`.
pub struct CronCreateTool(Store);

#[async_trait]
impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        "CronCreate"
    }

    fn description(&self) -> &str {
        "Schedule a prompt for later, once or on a 5-field cron expression in local time \
         (minute hour day-of-month month day-of-week). recurring: false fires once and \
         deletes itself; a recurring job expires after 7 days. The job is written to \
         ~/.wizard/schedule.toml and runs as a headless Wizard session in this directory \
         (1 hour cap) when `wizard scheduler` is running; it does not run inside this \
         session. Pick a minute other than :00 and :30 unless the user named one."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cron": { "type": "string", "description": "5-field cron in local time: \"M H DoM Mon DoW\" (e.g. \"*/5 * * * *\" = every 5 minutes, \"30 14 28 2 *\" = Feb 28 at 2:30pm once)." },
                "prompt": { "type": "string", "description": "The prompt to run at each fire time." },
                "recurring": { "type": "boolean", "description": "true (default) = every match until deleted or 7 days old. false = fire once, then delete." },
                "durable": { "type": "boolean", "description": "Accepted and ignored: every job is persisted." }
            },
            "required": ["cron", "prompt"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let (Some(cron), Some(prompt)) = (str_arg(&args, "cron"), str_arg(&args, "prompt")) else {
            return Ok(invalid(self.name(), "cron and prompt are required"));
        };
        let recurring = args
            .get("recurring")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let cron = cron.trim();
        if cron.split_whitespace().count() != 5 || schedule::parse_cron(cron).is_err() {
            return Ok(invalid(
                self.name(),
                format!("Invalid cron expression '{cron}'. Expected 5 fields: M H DoM Mon DoW."),
            ));
        }
        let id = new_id();
        let entry = ScheduleEntry {
            name: format!("{CRON_PREFIX}{id}"),
            cron: cron.to_string(),
            prompt: prompt.to_string(),
            cwd: ctx.cwd.clone(),
            mode: "sovereign".to_string(),
            max_hours: Some(RUN_HOURS),
            enabled: true,
            once: !recurring,
            expires_at: Some(Utc::now() + ChronoDuration::days(MAX_AGE_DAYS)),
        };
        let Some(next) = next_fire(&entry) else {
            return Ok(invalid(
                self.name(),
                format!(
                    "Cron expression '{cron}' does not match any calendar date in the next year."
                ),
            ));
        };
        if let Err(out) = add(&self.0, entry) {
            return Ok(out);
        }
        let human = human_schedule(cron);
        let next = next.format("%Y-%m-%d %H:%M %Z");
        let text = if recurring {
            format!(
                "Scheduled recurring job {id} ({human}), next at {next}. Persisted to \
                 ~/.wizard/schedule.toml and run as a headless Wizard session in {}. \
                 Auto-expires after {MAX_AGE_DAYS} days. Use CronDelete to cancel sooner.{}",
                ctx.cwd.display(),
                scheduler_note()
            )
        } else {
            format!(
                "Scheduled one-shot task {id} ({human}), at {next}. Persisted to \
                 ~/.wizard/schedule.toml and run as a headless Wizard session in {}. \
                 It will fire once then auto-delete.{}",
                ctx.cwd.display(),
                scheduler_note()
            )
        };
        Ok(ToolOutput::ok(text))
    }
}

/// `CronList`.
pub struct CronListTool(Store);

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        "CronList"
    }

    fn description(&self) -> &str {
        "List the jobs made with CronCreate: id, schedule, recurring or one-shot, prompt."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let entries = match self.0.entries(CRON_PREFIX) {
            Ok(entries) => entries,
            Err(out) => return Ok(out),
        };
        if entries.is_empty() {
            return Ok(ToolOutput::ok("No scheduled jobs."));
        }
        let lines: Vec<String> = entries
            .iter()
            .map(|e| {
                let prompt: String = e.prompt.chars().take(80).collect();
                let ellipsis = if e.prompt.chars().count() > 80 {
                    "..."
                } else {
                    ""
                };
                format!(
                    "{} — {}{}: {prompt}{ellipsis}",
                    e.name.trim_start_matches(CRON_PREFIX),
                    human_schedule(&e.cron),
                    if e.once {
                        " (one-shot)"
                    } else {
                        " (recurring)"
                    },
                )
            })
            .collect();
        Ok(ToolOutput::ok(lines.join("\n")))
    }
}

/// `CronDelete`.
pub struct CronDeleteTool(Store);

#[async_trait]
impl Tool for CronDeleteTool {
    fn name(&self) -> &str {
        "CronDelete"
    }

    fn description(&self) -> &str {
        "Cancel a job made with CronCreate, by the id CronCreate returned."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Job ID returned by CronCreate." } },
            "required": ["id"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(id) = str_arg(&args, "id") else {
            return Ok(invalid(self.name(), "id is required"));
        };
        let known = match self.0.entries(CRON_PREFIX) {
            Ok(entries) => entries
                .iter()
                .any(|e| e.name == format!("{CRON_PREFIX}{id}")),
            Err(out) => return Ok(out),
        };
        if !known {
            return Ok(invalid(
                self.name(),
                format!("No scheduled job with id '{id}'"),
            ));
        }
        let path = match self.0.path() {
            Ok(path) => path,
            Err(out) => return Ok(out),
        };
        match schedule::remove_entry(&path, &format!("{CRON_PREFIX}{id}")) {
            Ok(()) => Ok(ToolOutput::ok(format!("Cancelled job {id}."))),
            Err(err) => Ok(ToolOutput::error(format!("{err:#}"))),
        }
    }
}

/// `ScheduleWakeup`.
pub struct ScheduleWakeupTool(Store);

/// The pinned one-shot cron that fires at `at`, rounded up to the next minute
/// (the scheduler's resolution).
fn cron_at(at: DateTime<Local>) -> (String, DateTime<Local>) {
    let rounded = if at.second() > 0 || at.nanosecond() > 0 {
        at.with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .map(|t| t + ChronoDuration::minutes(1))
            .unwrap_or(at)
    } else {
        at
    };
    use chrono::Datelike;
    (
        format!(
            "{} {} {} {} *",
            rounded.minute(),
            rounded.hour(),
            rounded.day(),
            rounded.month()
        ),
        rounded,
    )
}

#[async_trait]
impl Tool for ScheduleWakeupTool {
    fn name(&self) -> &str {
        "ScheduleWakeup"
    }

    fn description(&self) -> &str {
        "Pick when a self-paced loop runs again: delaySeconds from now (clamped to 60-3600; the \
         scheduler works in whole minutes), with the prompt to run then. stop: true ends the \
         loop and cancels pending wakeups. Like CronCreate it needs `wizard scheduler` \
         running and the wakeup is a headless session in this directory, not this one. Do not \
         use a short delay to poll work you started; long delays cost less."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "delaySeconds": { "type": "number", "description": "Seconds from now to wake up. Clamped to [60, 3600]. Required unless stop is true." },
                "reason": { "type": "string", "description": "One short sentence on why this delay. Required unless stop is true." },
                "prompt": { "type": "string", "description": "What to run on wake-up. For an autonomous loop with no user prompt, pass <<autonomous-loop-dynamic>>. Required unless stop is true." },
                "stop": { "type": "boolean", "description": "End the loop now; all other fields are ignored." },
                "noop": { "type": "boolean", "description": "true if nothing changed this tick. Required unless stop is true." }
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    fn deferred_by_default(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path = match self.0.path() {
            Ok(path) => path,
            Err(out) => return Ok(out),
        };
        // One wakeup is pending per directory: scheduling the next tick
        // replaces the last, and stop cancels it.
        let pending: Vec<String> = match self.0.entries(WAKEUP_PREFIX) {
            Ok(entries) => entries
                .into_iter()
                .filter(|e| e.cwd == ctx.cwd)
                .map(|e| e.name)
                .collect(),
            Err(out) => return Ok(out),
        };
        let cancel = |names: &[String]| {
            for name in names {
                let _ = schedule::remove_entry(&path, name);
            }
        };

        if args.get("stop").and_then(Value::as_bool) == Some(true) {
            cancel(&pending);
            return Ok(ToolOutput::ok(format!(
                "Loop stopped. Cancelled {} pending wakeup{}.",
                pending.len(),
                if pending.len() == 1 { "" } else { "s" }
            )));
        }
        let delay = args.get("delaySeconds").and_then(Value::as_f64);
        let (Some(delay), Some(reason), Some(prompt)) =
            (delay, str_arg(&args, "reason"), str_arg(&args, "prompt"))
        else {
            return Ok(ToolOutput::error(
                "`delaySeconds` and `reason` are required when `stop` is not true.",
            ));
        };
        let wanted = delay.round() as i64;
        let clamped = wanted.clamp(*WAKEUP_RANGE.start(), *WAKEUP_RANGE.end());
        let (cron, at) = cron_at(Local::now() + ChronoDuration::seconds(clamped));
        let prompt = if prompt == AUTONOMOUS_SENTINEL {
            AUTONOMOUS_PROMPT
        } else {
            prompt
        };
        cancel(&pending);
        let entry = ScheduleEntry {
            name: format!("{WAKEUP_PREFIX}{}", new_id()),
            cron,
            prompt: prompt.to_string(),
            cwd: ctx.cwd.clone(),
            mode: "sovereign".to_string(),
            max_hours: Some(RUN_HOURS),
            enabled: true,
            once: true,
            expires_at: Some(Utc::now() + ChronoDuration::days(1)),
        };
        if let Err(out) = add(&self.0, entry) {
            return Ok(out);
        }
        let clamp_note = if clamped != wanted {
            format!(" (clamped from {wanted}s)")
        } else {
            String::new()
        };
        Ok(ToolOutput::ok(format!(
            "Wakeup scheduled for {} after {clamped}s{clamp_note}: {reason}.{}",
            at.format("%H:%M %Z"),
            scheduler_note()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools_in(
        dir: &std::path::Path,
    ) -> (
        CronCreateTool,
        CronListTool,
        CronDeleteTool,
        ScheduleWakeupTool,
    ) {
        let store = Store(Some(dir.join("schedule.toml")));
        (
            CronCreateTool(store.clone()),
            CronListTool(store.clone()),
            CronDeleteTool(store.clone()),
            ScheduleWakeupTool(store),
        )
    }

    fn id_of(text: &str) -> String {
        text.split_whitespace()
            .find(|w| w.len() == 8 && w.chars().all(|c| c.is_ascii_hexdigit()))
            .unwrap_or_else(|| panic!("no id in {text}"))
            .to_string()
    }

    #[test]
    fn common_shapes_read_the_way_claude_code_words_them() {
        assert_eq!(human_schedule("* * * * *"), "Every minute");
        assert_eq!(human_schedule("*/5 * * * *"), "Every 5 minutes");
        assert_eq!(human_schedule("7 * * * *"), "Every hour at :07");
        assert_eq!(human_schedule("30 9 * * *"), "Every day at 9:30");
        assert_eq!(human_schedule("30 14 28 2 *"), "30 14 28 2 *");
    }

    #[tokio::test]
    async fn create_list_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (create, list, delete, _) = tools_in(dir.path());
        let ctx = ToolContext::new(dir.path());

        assert_eq!(
            list.execute(json!({}), &ctx).await.unwrap().content,
            "No scheduled jobs."
        );

        let out = create
            .execute(
                json!({ "cron": "7 * * * *", "prompt": "check the build" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.starts_with("Scheduled recurring job "),
            "{}",
            out.content
        );
        assert!(out.content.contains("Every hour at :07"));
        assert!(out.content.contains("Auto-expires after 7 days"));
        let id = id_of(&out.content);

        let listed = list.execute(json!({}), &ctx).await.unwrap().content;
        assert_eq!(
            listed,
            format!("{id} — Every hour at :07 (recurring): check the build")
        );

        let gone = delete.execute(json!({ "id": id }), &ctx).await.unwrap();
        assert_eq!(gone.content, format!("Cancelled job {id}."));
        assert_eq!(
            list.execute(json!({}), &ctx).await.unwrap().content,
            "No scheduled jobs."
        );

        let missing = delete.execute(json!({ "id": "nope" }), &ctx).await.unwrap();
        assert!(missing.is_error);
        assert_eq!(
            missing.content,
            "Tool CronDelete input is invalid: No scheduled job with id 'nope'"
        );
    }

    #[tokio::test]
    async fn one_shot_jobs_are_marked_and_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let (create, list, _, _) = tools_in(dir.path());
        let ctx = ToolContext::new(dir.path());
        let out = create
            .execute(
                json!({ "cron": "30 14 28 2 *", "prompt": "once", "recurring": false }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.content.starts_with("Scheduled one-shot task "),
            "{}",
            out.content
        );
        assert!(out.content.contains("fire once then auto-delete"));
        let stored = schedule::load_schedule(&dir.path().join("schedule.toml")).unwrap();
        assert!(stored.entries[0].once);
        assert!(
            list.execute(json!({}), &ctx)
                .await
                .unwrap()
                .content
                .contains("(one-shot)")
        );
    }

    #[tokio::test]
    async fn a_bad_cron_is_refused_in_claude_codes_words() {
        let dir = tempfile::tempdir().unwrap();
        let (create, ..) = tools_in(dir.path());
        let ctx = ToolContext::new(dir.path());
        let out = create
            .execute(json!({ "cron": "bad cron", "prompt": "x" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "Tool CronCreate input is invalid: Invalid cron expression 'bad cron'. Expected 5 fields: M H DoM Mon DoW."
        );
        let never = create
            .execute(json!({ "cron": "0 0 31 2 *", "prompt": "x" }), &ctx)
            .await
            .unwrap();
        assert!(never.is_error, "Feb 31 never happens: {}", never.content);
    }

    #[tokio::test]
    async fn hand_made_entries_are_not_listed_or_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let (_, list, delete, _) = tools_in(dir.path());
        let ctx = ToolContext::new(dir.path());
        schedule::add_entry(
            &dir.path().join("schedule.toml"),
            ScheduleEntry {
                name: "nightly".into(),
                cron: "0 3 * * *".into(),
                prompt: "tidy".into(),
                cwd: dir.path().to_path_buf(),
                mode: "sovereign".into(),
                max_hours: None,
                enabled: true,
                once: false,
                expires_at: None,
            },
        )
        .unwrap();
        assert_eq!(
            list.execute(json!({}), &ctx).await.unwrap().content,
            "No scheduled jobs."
        );
        assert!(
            delete
                .execute(json!({ "id": "nightly" }), &ctx)
                .await
                .unwrap()
                .is_error
        );
    }

    #[tokio::test]
    async fn wakeup_clamps_replaces_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _, _, wakeup) = tools_in(dir.path());
        let ctx = ToolContext::new(dir.path());
        let path = dir.path().join("schedule.toml");

        let first = wakeup
            .execute(
                json!({ "delaySeconds": 30, "reason": "watching CI", "prompt": "p", "noop": true }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!first.is_error, "{}", first.content);
        assert!(
            first
                .content
                .contains("after 60s (clamped from 30s): watching CI.")
        );

        wakeup
            .execute(
                json!({ "delaySeconds": 5000, "reason": "idle", "prompt": AUTONOMOUS_SENTINEL, "noop": true }),
                &ctx,
            )
            .await
            .unwrap();
        let stored = schedule::load_schedule(&path).unwrap().entries;
        assert_eq!(stored.len(), 1, "the second wakeup replaced the first");
        assert!(stored[0].prompt.starts_with("Continue the autonomous loop"));
        assert!(stored[0].once);

        let stopped = wakeup.execute(json!({ "stop": true }), &ctx).await.unwrap();
        assert_eq!(stopped.content, "Loop stopped. Cancelled 1 pending wakeup.");
        assert!(schedule::load_schedule(&path).unwrap().entries.is_empty());

        let missing = wakeup.execute(json!({}), &ctx).await.unwrap();
        assert!(missing.is_error);
        assert_eq!(
            missing.content,
            "`delaySeconds` and `reason` are required when `stop` is not true."
        );
    }

    #[test]
    fn a_wakeup_rounds_up_to_the_next_whole_minute() {
        use chrono::TimeZone;
        let at = Local.with_ymd_and_hms(2026, 3, 4, 10, 59, 20).unwrap();
        let (cron, rounded) = cron_at(at);
        assert_eq!(cron, "0 11 4 3 *");
        assert_eq!(rounded.minute(), 0);
        let exact = Local.with_ymd_and_hms(2026, 3, 4, 10, 15, 0).unwrap();
        assert_eq!(cron_at(exact).0, "15 10 4 3 *");
    }
}
