//! `apiKeyHelper`: a command, set in a Claude Code `settings.json`, that
//! prints the API key. Wizard runs it once at startup and hands the answer to
//! the active provider through the variable that provider already reads its
//! key from, unless that variable is already set.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Run `command` through the shell and return its trimmed stdout, or `None`
/// when it fails, times out, or prints nothing.
pub fn run_helper(command: &str, timeout: Duration) -> Option<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait().ok()? {
            Some(status) => break status,
            None if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let key = out.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// Export the helper's key for the active provider.
///
/// # Safety
///
/// Writes the process environment, so it must run before any other thread
/// exists, as [`crate::cli_claude::ClaudeFlags::publish`] does.
pub unsafe fn export() {
    let Ok(root) = std::env::current_dir() else {
        return;
    };
    let settings = super::claude::load(&root, super::claude::Trust::Refused);
    let Some(helper) = settings.api_key_helper else {
        return;
    };
    let Ok(config) = super::Config::load() else {
        return;
    };
    let Some(var) = config.active().key_env_name() else {
        return;
    };
    if std::env::var_os(&var).is_some_and(|value| !value.is_empty()) {
        return;
    }
    if let Some(key) = run_helper(&helper, TIMEOUT) {
        unsafe { std::env::set_var(var, key) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_prints_the_key() {
        assert_eq!(
            run_helper("echo sk-test", Duration::from_secs(5)).as_deref(),
            Some("sk-test")
        );
    }

    #[test]
    fn a_failing_or_silent_helper_gives_no_key() {
        assert_eq!(run_helper("exit 3", Duration::from_secs(5)), None);
        assert_eq!(run_helper("true", Duration::from_secs(5)), None);
    }

    #[test]
    fn a_slow_helper_is_killed() {
        let started = Instant::now();
        assert_eq!(
            run_helper("sleep 5; echo late", Duration::from_millis(200)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
