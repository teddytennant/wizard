//! `wizard gateway install|start|stop|restart|status|logs|uninstall`: the
//! gateway as a background service.
//!
//! The mechanism is [`crate::platform::service`], which knows about systemd
//! user units and launchd agents and nothing about Wizard. This module is the
//! other half: what the *gateway* specifically needs before a supervisor can
//! keep it alive with nobody watching.
//!
//! That is almost entirely one problem. A gateway turn needs a bot token, and
//! a service inherits no environment, so the shape of failure this module
//! exists to prevent is: the operator exports `WIZARD_TELEGRAM_TOKEN` in their
//! shell, `wizard --gateway` works, `wizard gateway install` succeeds, and the
//! service crash-loops with "Telegram bot token not set" in a journal nobody
//! is reading. The fix is *not* to put the token in the unit —
//! `~/.config/systemd/user/*.service` is 0644 and `systemctl --user cat`
//! prints it back — but to move it, once, into the store the service already
//! reads: `~/.wizard/credentials.toml` at 0600. See [`TokenPlan`].

use anyhow::{Context, Result, bail};

use crate::config::{Config, GatewayKind};
use crate::platform::service::{self, ServiceCmd, ServiceSpec};

/// Service name. `wizard-gateway.service` under systemd,
/// `com.teddytennant.wizard.gateway` under launchd.
pub const SERVICE_NAME: &str = "wizard-gateway";

/// The CLI that manages it, for the hints printed after an action.
const CLI: &str = "wizard gateway";

const DOCUMENTATION: &str = "https://github.com/teddytennant/wizard/blob/main/docs/services.md";

/// Describe the gateway service for this machine: this binary, `--gateway`,
/// and `working_dir` (defaulting to the current directory) as the project the
/// agent operates on.
///
/// The working directory is the whole reason `install` has to be run from
/// somewhere in particular: a gateway turn edits files, and a service whose
/// `WorkingDirectory` defaulted to `$HOME` — as the unit pasted in the old
/// `docs/gateway.md` did — points a sovereign agent at the home directory.
pub fn spec(working_dir: Option<std::path::PathBuf>) -> Result<ServiceSpec> {
    ServiceSpec::for_surface(
        SERVICE_NAME,
        "Wizard messaging gateway",
        DOCUMENTATION,
        CLI,
        &["--gateway"],
        working_dir,
    )
}

/// How the running service will find the bot token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenPlan {
    /// It is already in `~/.wizard/credentials.toml`, which the service reads
    /// because it runs as the same user. Nothing to do.
    Stored,
    /// It exists only in the installing shell's environment. A supervisor
    /// hands the service no environment, so the token is copied into
    /// `credentials.toml` (0600) — the same place onboarding puts it — rather
    /// than written into a world-readable unit.
    AdoptFromEnv { var: String, token: String },
    /// There is no token anywhere. Refuse: an installed service that cannot
    /// authenticate is a crash loop with a nice message in a log nobody opens.
    Missing { var: String },
}

/// Decide where the token comes from, given what is stored and what is in the
/// environment.
///
/// Pure, and separate from the doing, because the decision is the security
/// boundary: "copy it into the 0600 store" and "write it into the unit" look
/// equally convenient from the outside and only one of them is acceptable.
pub fn token_plan(stored: Option<&str>, env_value: Option<&str>, var: &str) -> TokenPlan {
    if stored.is_some_and(|token| !token.trim().is_empty()) {
        return TokenPlan::Stored;
    }
    match env_value.map(str::trim).filter(|token| !token.is_empty()) {
        Some(token) => TokenPlan::AdoptFromEnv {
            var: var.to_string(),
            token: token.to_string(),
        },
        None => TokenPlan::Missing {
            var: var.to_string(),
        },
    }
}

impl TokenPlan {
    /// Carry the plan out, returning the line to print (if any).
    fn apply(self) -> Result<Option<String>> {
        match self {
            TokenPlan::Stored => Ok(None),
            TokenPlan::AdoptFromEnv { var, token } => {
                crate::credentials::store(crate::credentials::GATEWAY_TOKEN, &token)
                    .context("storing the Telegram bot token for the service to read")?;
                Ok(Some(format!(
                    "copied the bot token from ${var} into {} (mode 0600) — a background \
                     service inherits no environment, and a token in a unit file would be \
                     world-readable",
                    crate::credentials::path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|_| "~/.wizard/credentials.toml".to_string())
                )))
            }
            TokenPlan::Missing { var } => bail!(
                "no Telegram bot token, so the service would start and immediately fail.\n\
                 Give Wizard the token first, either way round:\n\
                 \x20 wizard --onboard            # pick Telegram and paste it\n\
                 \x20 export {var}=<token> && {CLI} install   # copied into \
                 ~/.wizard/credentials.toml (0600) for the service to read\n\
                 Create a bot with @BotFather to get one."
            ),
        }
    }
}

/// Everything that must be true before the gateway is worth installing, in
/// the order an operator can act on. Returns the notes to print.
///
/// This runs on `install` only. `status`, `logs` and `stop` have to keep
/// working on a machine whose config has since been broken — being unable to
/// look at a running service because its config no longer parses is exactly
/// the wrong time to refuse.
fn preflight() -> Result<Vec<String>> {
    let config = Config::load().context("loading config to check the gateway is configured")?;
    if config.gateway.kind == GatewayKind::None {
        bail!(
            "no gateway is configured, so there is nothing to supervise.\n\
             Add to ~/.wizard/config.toml:\n\
             \x20 [gateway]\n\
             \x20 kind = \"telegram\"\n\
             \x20 allowed_chat_ids = [<your chat id>]\n\
             or run `wizard --onboard` and pick Telegram. See docs/gateway.md."
        );
    }

    let mut notes = Vec::new();
    let var = config.gateway.token_env().to_string();
    let plan = token_plan(
        crate::credentials::get(crate::credentials::GATEWAY_TOKEN).as_deref(),
        std::env::var(&var).ok().as_deref(),
        &var,
    );
    if let Some(note) = plan.apply()? {
        notes.push(note);
    }

    // Not fatal: an empty list is a legitimate state to install from, and the
    // fix is one command. Saying so is what stops it looking like a dead bot.
    if config.gateway.allowed_chat_ids.is_empty() {
        notes.push(format!(
            "warning: gateway.allowed_chat_ids is empty, so every message is refused. \
             Run `{CLI} setup` to discover your chat id and add it, then `{CLI} restart`."
        ));
    }
    Ok(notes)
}

/// `wizard gateway <cmd>`.
///
/// `--cwd` has already been applied by the dispatcher, so the current
/// directory is the project the operator meant.
pub fn run(cmd: ServiceCmd) -> Result<i32> {
    let spec = spec(None)?;
    if matches!(cmd, ServiceCmd::Install) {
        for note in preflight()? {
            println!("{note}");
        }
    }
    service::dispatch(&spec, cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_in_the_environment_is_moved_into_the_private_store_not_the_unit() {
        const TOKEN: &str = "7654321:AA-not-a-real-bot-token";

        // Already stored: nothing happens, and in particular the environment
        // does not overwrite what was pasted during onboarding (which is the
        // precedence `telegram::connect` uses too).
        assert_eq!(
            token_plan(Some(TOKEN), Some("something-else"), "WIZARD_TELEGRAM_TOKEN"),
            TokenPlan::Stored
        );
        // A stored blank is not a token.
        assert!(matches!(
            token_plan(Some("  "), Some(TOKEN), "V"),
            TokenPlan::AdoptFromEnv { .. }
        ));

        // Only in the environment: adopted, with the value carried so it can
        // be written to credentials.toml — never to the unit.
        assert_eq!(
            token_plan(None, Some(&format!(" {TOKEN} ")), "MY_VAR"),
            TokenPlan::AdoptFromEnv {
                var: "MY_VAR".to_string(),
                token: TOKEN.to_string(),
            }
        );

        // Nowhere: refuse, naming the variable the operator can set.
        assert_eq!(
            token_plan(None, None, "MY_VAR"),
            TokenPlan::Missing {
                var: "MY_VAR".to_string()
            }
        );
        assert_eq!(
            token_plan(None, Some(""), "MY_VAR"),
            TokenPlan::Missing {
                var: "MY_VAR".to_string()
            }
        );
        let err = format!(
            "{:#}",
            TokenPlan::Missing {
                var: "MY_VAR".to_string()
            }
            .apply()
            .expect_err("no token must refuse the install")
        );
        assert!(err.contains("MY_VAR"), "{err}");
        assert!(err.contains("onboard"), "{err}");
        // The refusal happens *before* anything is installed, so it must not
        // suggest that the unit could carry the token instead.
        assert!(!err.contains("Environment="), "{err}");
    }

    /// The unit the gateway actually installs, checked end to end: the real
    /// spec builder, the real renderer, and no secret in the result.
    #[test]
    fn the_gateway_unit_runs_this_binary_in_this_project_and_holds_no_token() {
        let project = tempfile::tempdir().expect("tempdir");
        let spec = spec(Some(project.path().to_path_buf())).expect("spec");
        assert_eq!(spec.name, SERVICE_NAME);
        assert_eq!(spec.args, vec!["--gateway".to_string()]);
        assert!(spec.exe.is_absolute(), "{}", spec.exe.display());
        assert!(
            spec.exe.exists(),
            "the unit points at a binary that exists: {}",
            spec.exe.display()
        );

        let installer =
            service::Installer::at(service::Manager::Systemd, project.path().join("units"));
        let unit = installer.render(&spec).expect("render");
        assert!(
            unit.contains(&format!("ExecStart={} --gateway", spec.exe.display()))
                || unit.contains(&format!("ExecStart=\"{}\" --gateway", spec.exe.display())),
            "{unit}"
        );
        assert!(
            unit.contains(&format!("WorkingDirectory={}", spec.working_dir.display())),
            "the captured project, not the home directory:\n{unit}"
        );
        let lower = unit.to_ascii_lowercase();
        assert!(!lower.contains("telegram"), "{unit}");
        assert!(!lower.contains("token"), "{unit}");
    }
}
