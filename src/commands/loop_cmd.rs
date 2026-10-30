//! `/loop`: run a prompt again and again on Wizard's scheduler.
//!
//! `/loop 5m check the deploy` writes a recurring entry to
//! `~/.wizard/schedule.toml`; `wizard scheduler` fires it as a headless run in
//! this directory every five minutes, for up to a week. `/loop check the
//! deploy` with no interval is self-paced: one run starts in a minute, and the
//! run itself decides when the next one is due (`ScheduleWakeup`) or stops.
//! Neither runs inside the live session: each tick is its own headless run, so
//! the daemon has to be up for anything to fire.

use std::path::Path;

use chrono::{Duration as ChronoDuration, Local, Timelike, Utc};

use crate::schedule::{self, ScheduleEntry};

const RECURRING: &str = "cron-loop-";
const SELF_PACED: &str = "wakeup-loop-";
const MAX_AGE_DAYS: i64 = 7;
const RUN_HOURS: f64 = 1.0;

/// `30s`, `5m`, `2h`, `1d` as seconds.
fn interval_seconds(word: &str) -> Option<u64> {
    let word = word.trim();
    let unit = word.chars().last()?;
    let count: u64 = word[..word.len() - unit.len_utf8()].parse().ok()?;
    let seconds = match unit {
        's' => count,
        'm' => count * 60,
        'h' => count * 3600,
        'd' => count * 86_400,
        _ => return None,
    };
    (seconds > 0).then_some(seconds)
}

/// The interval at the start of `args` (`5m` or `every 5m`) and the prompt
/// after it.
fn split_interval(args: &str) -> (Option<u64>, &str) {
    let args = args.trim();
    let after_every = args.strip_prefix("every ").unwrap_or(args).trim_start();
    let first = after_every.split_whitespace().next().unwrap_or("");
    match interval_seconds(first) {
        Some(seconds) => (Some(seconds), after_every[first.len()..].trim()),
        None => (None, args),
    }
}

/// A five-field cron expression for a repeat every `seconds`, rounded up to a
/// whole minute and down to a step cron can express.
fn cron_for(seconds: u64) -> (String, String) {
    let minutes = seconds.div_ceil(60).max(1);
    let (cron, label) = if minutes < 60 {
        (format!("*/{minutes} * * * *"), format!("{minutes}m"))
    } else if minutes < 24 * 60 {
        let hours = (minutes / 60).max(1);
        (format!("0 */{hours} * * *"), format!("{hours}h"))
    } else {
        let days = (minutes / (24 * 60)).max(1);
        (format!("0 0 */{days} * *"), format!("{days}d"))
    };
    (cron, label)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..6].to_string()
}

fn daemon_note() -> &'static str {
    if schedule::daemon_running() {
        ""
    } else {
        "\nthe scheduler is not running, so nothing fires until you start it with `wizard scheduler`"
    }
}

const SELF_PACED_SUFFIX: &str = "\n\nThis is a self-paced loop. When this pass is done, decide \
whether the task needs another one. If it does, call ScheduleWakeup with a delaySeconds that \
fits (60 to 3600) and this same prompt; if nothing is left, call ScheduleWakeup with stop: \
true, or schedule nothing.";

pub fn run(args: &str, root: &Path) -> Result<String, String> {
    let path = crate::config::Config::schedule_path().map_err(|err| format!("{err:#}"))?;
    run_at(&path, args, root)
}

pub fn run_at(path: &Path, args: &str, root: &Path) -> Result<String, String> {
    let args = args.trim();
    match args.split_whitespace().next() {
        None | Some("list") => return list(path),
        Some("stop") => return stop(path, args.trim_start_matches("stop").trim()),
        _ => {}
    }
    let (interval, prompt) = split_interval(args);
    if prompt.is_empty() {
        return Err("usage: /loop [interval] <prompt>, /loop list, /loop stop [id]".to_string());
    }
    let id = new_id();
    let now = Utc::now();
    let (name, cron, entry_prompt, once, summary) = match interval {
        Some(seconds) => {
            let (cron, label) = cron_for(seconds);
            let note = if seconds % 60 != 0 {
                " (rounded up to whole minutes; cron cannot fire faster)"
            } else {
                ""
            };
            (
                format!("{RECURRING}{id}"),
                cron,
                prompt.to_string(),
                false,
                format!("runs every {label}{note}, for up to {MAX_AGE_DAYS} days"),
            )
        }
        None => {
            let at = Local::now() + ChronoDuration::minutes(1);
            (
                format!("{SELF_PACED}{id}"),
                format!(
                    "{} {} {} {} *",
                    at.minute(),
                    at.hour(),
                    at.format("%-d"),
                    at.format("%-m")
                ),
                format!("{prompt}{SELF_PACED_SUFFIX}"),
                true,
                "starts in about a minute and paces itself from there".to_string(),
            )
        }
    };
    schedule::parse_cron(&cron).map_err(|err| format!("{err:#}"))?;
    let entry = ScheduleEntry {
        name: name.clone(),
        cron,
        prompt: entry_prompt,
        cwd: root.to_path_buf(),
        mode: "sovereign".to_string(),
        max_hours: Some(RUN_HOURS),
        enabled: true,
        once,
        expires_at: Some(now + ChronoDuration::days(MAX_AGE_DAYS)),
    };
    schedule::add_entry(path, entry).map_err(|err| format!("{err:#}"))?;
    Ok(format!(
        "loop {name}: {summary}\neach run is its own headless session in {}, capped at {RUN_HOURS} hour\n\
         /loop list shows loops, /loop stop {id} ends this one{}",
        root.display(),
        daemon_note()
    ))
}

fn loops(path: &Path) -> Result<Vec<ScheduleEntry>, String> {
    let file = schedule::load_schedule(path).map_err(|err| format!("{err:#}"))?;
    Ok(file
        .entries
        .into_iter()
        .filter(|entry| entry.name.starts_with(RECURRING) || entry.name.starts_with(SELF_PACED))
        .collect())
}

fn list(path: &Path) -> Result<String, String> {
    let entries = loops(path)?;
    if entries.is_empty() {
        return Ok("no loops. /loop 5m <prompt> starts one".to_string());
    }
    let mut out = String::from("loops\n");
    for entry in entries {
        let shown = entry
            .prompt
            .split("\n\nThis is a self-paced")
            .next()
            .unwrap_or("");
        out.push_str(&format!(
            "  {:<18} {:<14} {}\n",
            entry.name,
            entry.cron,
            shown.lines().next().unwrap_or("")
        ));
    }
    Ok(out)
}

fn stop(path: &Path, which: &str) -> Result<String, String> {
    let entries = loops(path)?;
    let chosen: Vec<&ScheduleEntry> = if which.is_empty() || which == "all" {
        entries.iter().collect()
    } else {
        entries
            .iter()
            .filter(|entry| entry.name.ends_with(which) || entry.name == which)
            .collect()
    };
    if chosen.is_empty() {
        return Err(format!("no loop matches '{which}'"));
    }
    for entry in &chosen {
        schedule::remove_entry(path, &entry.name).map_err(|err| format!("{err:#}"))?;
    }
    Ok(format!("stopped {} loop(s)", chosen.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_parse_with_their_unit() {
        assert_eq!(interval_seconds("30s"), Some(30));
        assert_eq!(interval_seconds("5m"), Some(300));
        assert_eq!(interval_seconds("2h"), Some(7200));
        assert_eq!(interval_seconds("1d"), Some(86_400));
        assert_eq!(interval_seconds("0m"), None);
        assert_eq!(interval_seconds("m"), None);
        assert_eq!(interval_seconds("check"), None);
    }

    #[test]
    fn the_interval_comes_off_the_front_of_the_prompt() {
        assert_eq!(
            split_interval("5m check the deploy"),
            (Some(300), "check the deploy")
        );
        assert_eq!(
            split_interval("every 2h /babysit"),
            (Some(7200), "/babysit")
        );
        assert_eq!(
            split_interval("check the deploy"),
            (None, "check the deploy")
        );
        assert_eq!(split_interval("5m"), (Some(300), ""));
    }

    #[test]
    fn an_interval_becomes_a_cron_that_parses() {
        for (seconds, cron) in [
            (30, "*/1 * * * *"),
            (300, "*/5 * * * *"),
            (3600, "0 */1 * * *"),
            (7200, "0 */2 * * *"),
            (86_400, "0 0 */1 * *"),
        ] {
            let (got, _) = cron_for(seconds);
            assert_eq!(got, cron);
            schedule::parse_cron(&got).unwrap();
        }
    }

    #[test]
    fn a_fixed_loop_is_a_recurring_entry_and_can_be_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("schedule.toml");
        let said = run_at(&file, "5m check the deploy", tmp.path()).unwrap();
        assert!(said.contains("every 5m"), "{said}");
        let entries = loops(&file).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].name.starts_with(RECURRING));
        assert!(!entries[0].once);
        assert_eq!(entries[0].prompt, "check the deploy");
        assert!(entries[0].expires_at.is_some());
        assert!(
            run_at(&file, "list", tmp.path())
                .unwrap()
                .contains("check the deploy")
        );
        let id = entries[0].name.trim_start_matches(RECURRING).to_string();
        assert!(
            run_at(&file, &format!("stop {id}"), tmp.path())
                .unwrap()
                .contains("stopped 1")
        );
        assert!(loops(&file).unwrap().is_empty());
        assert!(run_at(&file, "stop nothing", tmp.path()).is_err());
    }

    #[test]
    fn a_self_paced_loop_fires_once_and_tells_the_run_to_reschedule() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("schedule.toml");
        run_at(&file, "keep the build green", tmp.path()).unwrap();
        let entries = loops(&file).unwrap();
        assert!(entries[0].once);
        assert!(entries[0].name.starts_with(SELF_PACED));
        assert!(entries[0].prompt.starts_with("keep the build green"));
        assert!(entries[0].prompt.contains("ScheduleWakeup"));
        assert!(
            run_at(&file, "list", tmp.path())
                .unwrap()
                .contains("keep the build green")
        );
    }

    #[test]
    fn a_bare_interval_is_a_usage_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_at(&tmp.path().join("s.toml"), "5m", tmp.path()).unwrap_err();
        assert!(err.starts_with("usage:"), "{err}");
    }
}
