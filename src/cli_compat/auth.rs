//! `wizard auth login | logout | status`.
//!
//! `login` is `wizard --login <provider>` under another name and is rewritten
//! into it, so the one OAuth flow stays in one place. `status` reads the token
//! files and the provider table; it prints JSON unless asked for text, like
//! `claude auth status`.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::json;

use super::AuthCmd;
use crate::config::{Config, Credentials};

/// The provider `auth login` means, in the names `--login` takes.
pub fn login_provider(
    provider: Option<&str>,
    claudeai: bool,
    console: bool,
    email: Option<&str>,
    sso: bool,
) -> Result<String> {
    if console {
        bail!(
            "Wizard has no Anthropic Console sign-in. Export ANTHROPIC_API_KEY, or store a key \
             with /provider, and add an `anthropic` provider."
        );
    }
    if email.is_some() || sso {
        eprintln!("wizard: --email and --sso belong to Anthropic's login page; ignored");
    }
    Ok(match (provider, claudeai) {
        (Some(name), _) => name.to_string(),
        (None, true) => "claude".to_string(),
        (None, false) => "xai".to_string(),
    })
}

/// File each account sign-in keeps its tokens in, under `~/.wizard`.
fn token_file(login: &str) -> Option<&'static str> {
    match login {
        "xai" => Some("xai_oauth.json"),
        "chatgpt" => Some("chatgpt_oauth.json"),
        _ => None,
    }
}

fn signed_in(dir: &Path, login: &str) -> bool {
    token_file(login).is_some_and(|file| dir.join(file).is_file())
}

struct Row {
    name: String,
    kind: String,
    method: &'static str,
    signed_in: bool,
}

fn rows(dir: &Path, config: Option<&Config>) -> Vec<Row> {
    let mut out: Vec<Row> = ["xai", "chatgpt"]
        .into_iter()
        .map(|login| Row {
            name: login.to_string(),
            kind: login.to_string(),
            method: "oauth",
            signed_in: signed_in(dir, login),
        })
        .collect();
    let Some(config) = config else { return out };
    for provider in &config.providers {
        let (method, ok) = match provider.credentials() {
            Credentials::Local => ("none", true),
            Credentials::ApiKey { .. } => ("apiKey", !provider.api_key().is_empty()),
            Credentials::Account { login } => ("oauth", signed_in(dir, &login)),
        };
        if method == "oauth" && out.iter().any(|row| row.kind == provider.kind.as_str()) {
            continue;
        }
        out.push(Row {
            name: provider.name.clone(),
            kind: provider.kind.to_string(),
            method,
            signed_in: ok,
        });
    }
    out
}

fn status(json_out: bool, text: bool) -> Result<i32> {
    let dir = Config::wizard_dir()?;
    let config = Config::load().ok();
    let rows = rows(&dir, config.as_ref());
    let any = rows.iter().any(|row| row.signed_in && row.method != "none");
    if text && !json_out {
        if rows.is_empty() {
            println!("No providers.");
        }
        for row in &rows {
            let state = match (row.method, row.signed_in) {
                ("none", _) => "local, no sign-in needed",
                (_, true) => "signed in",
                (_, false) => "not signed in",
            };
            println!("{} ({}, {}): {state}", row.name, row.kind, row.method);
        }
        if !any {
            println!("Not logged in. Run wizard auth login to authenticate.");
        }
        return Ok(i32::from(!any));
    }
    let providers: Vec<_> = rows
        .iter()
        .map(|row| {
            json!({
                "name": row.name,
                "kind": row.kind,
                "authMethod": row.method,
                "loggedIn": row.signed_in,
            })
        })
        .collect();
    let report = json!({
        "loggedIn": any,
        "activeProvider": config.as_ref().and_then(|c| c.active_provider.clone()),
        "providers": providers,
        "configDirectory": dir,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(i32::from(!any))
}

fn logout(provider: Option<&str>) -> Result<i32> {
    let dir = Config::wizard_dir()?;
    let name = match provider {
        Some(name) => name.to_string(),
        None => {
            let live: Vec<&str> = ["xai", "chatgpt"]
                .into_iter()
                .filter(|login| signed_in(&dir, login))
                .collect();
            match live.as_slice() {
                [] => {
                    println!("Not signed in to any account.");
                    return Ok(0);
                }
                [one] => (*one).to_string(),
                many => bail!(
                    "signed in to {}; name one: wizard auth logout <provider>",
                    many.join(" and ")
                ),
            }
        }
    };
    if token_file(&name).is_some() {
        println!(
            "{}",
            crate::commands::claude::sign_out(&name).map_err(anyhow::Error::msg)?
        );
        return Ok(0);
    }
    if crate::credentials::get(&name).is_some() {
        crate::credentials::remove(&name)?;
        println!("removed the stored key for {name}");
        return Ok(0);
    }
    println!(
        "nothing stored for {name}; an API key exported in the environment is not Wizard's to remove"
    );
    Ok(0)
}

pub fn run(cmd: AuthCmd) -> Result<i32> {
    match cmd {
        AuthCmd::Status { json, text } => status(json, text),
        AuthCmd::Logout { provider } => logout(provider.as_deref()),
        AuthCmd::Login { .. } => unreachable!("`auth login` is rewritten to `--login` first"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_defaults_to_xai_and_maps_the_claude_flags() {
        assert_eq!(
            login_provider(None, false, false, None, false).unwrap(),
            "xai"
        );
        assert_eq!(
            login_provider(None, true, false, None, false).unwrap(),
            "claude"
        );
        assert_eq!(
            login_provider(Some("chatgpt"), true, false, None, false).unwrap(),
            "chatgpt"
        );
        assert!(login_provider(None, false, true, None, false).is_err());
    }

    #[test]
    fn a_token_file_means_signed_in() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!signed_in(tmp.path(), "xai"));
        std::fs::write(tmp.path().join("xai_oauth.json"), "{}").unwrap();
        assert!(signed_in(tmp.path(), "xai"));
        assert!(!signed_in(tmp.path(), "chatgpt"));
        assert!(!signed_in(tmp.path(), "claude"));
        let rows = rows(tmp.path(), None);
        assert!(rows.iter().any(|r| r.name == "xai" && r.signed_in));
    }
}
