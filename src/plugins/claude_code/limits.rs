//! The Claude subscription's plan limits: the session and weekly windows
//! Claude Code shows in its own `/usage`.
//!
//! The provider itself never reads Claude Code's files (see the module docs).
//! This does, and only this: `/usage` has no other way to the numbers, since
//! `claude` has no subcommand that prints them and a `claude -p` run to ask
//! would spend the window it is reporting on.
//!
//! `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`) holds the OAuth
//! access token Claude Code signed in with. `GET /api/oauth/usage` on
//! `api.anthropic.com` answers it with `five_hour`, `seven_day` and
//! `seven_day_opus`, each `utilization` (0-100) and `resets_at`, plus a
//! `seven_day_breakdown` splitting the week across Claude Code, chats and the
//! rest. Reading costs no tokens.
//!
//! The token is read, never refreshed. Claude Code's refresh token is
//! single-use: spending it here would sign out the `claude` the user actually
//! works in. An expired token is a note instead.

#[cfg(not(test))]
use std::path::PathBuf;

use serde_json::Value;

use crate::subscription_usage::{LimitWindow, ProductShare, Source, Subscription, plan_label};

/// The usage endpoint. Fixed rather than derived from a configured base URL,
/// so the bearer only ever goes to Anthropic.
#[cfg(not(test))]
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The beta header Claude Code's own OAuth calls carry.
#[cfg(not(test))]
const OAUTH_BETA: &str = "oauth-2025-04-20";
#[cfg(not(test))]
const USAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What the block says when `claude` is not signed in on this machine.
#[cfg(not(test))]
const NOT_SIGNED_IN: &str = "not signed in; run `claude auth login`";

/// The sign-in as Claude Code stored it. Only these three fields are read;
/// the tokens never leave this module.
#[cfg(not(test))]
#[derive(Debug, Clone)]
struct StoredTokens {
    access_token: String,
    /// Unix milliseconds, as Claude Code writes it.
    expires_at: Option<i64>,
    /// `pro`, `max`, … as the sign-in named it.
    subscription_type: Option<String>,
}

/// `/usage`'s Claude block. Always a block: this is only asked once the user
/// has added Claude Code as a provider, so "nothing to report" is an answer
/// they are owed rather than a row to hide.
///
/// Tests never reach the network: the suite dispatches every slash command,
/// and a live call would spend the session of whoever runs it. The parsing
/// below is tested with a captured body.
pub async fn subscription() -> Option<Subscription> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        let stored = match read_credentials() {
            Ok(stored) => stored,
            Err(reason) => {
                return Some(Subscription {
                    note: Some(reason),
                    ..Subscription::new("claude", "Claude")
                });
            }
        };
        let plan = stored.subscription_type.clone();
        Some(to_subscription(fetch(&stored).await, plan))
    }
}

/// Where Claude Code keeps the sign-in, honouring `CLAUDE_CONFIG_DIR`.
#[cfg(not(test))]
fn credentials_path() -> Option<PathBuf> {
    crate::config::claude::claude_dir().map(|dir| dir.join(".credentials.json"))
}

/// Read the stored sign-in. The `Err` is the sentence the block shows, so it
/// names what is missing rather than how the read failed.
#[cfg(not(test))]
fn read_credentials() -> Result<StoredTokens, String> {
    let path = credentials_path().ok_or_else(|| NOT_SIGNED_IN.to_string())?;
    let raw = std::fs::read_to_string(&path).map_err(|_| {
        if cfg!(target_os = "macos") {
            // Claude Code on macOS can keep the sign-in in the Keychain
            // instead, which is not ours to open.
            format!(
                "{NOT_SIGNED_IN}, or the sign-in is in the macOS Keychain rather than {}",
                path.display()
            )
        } else {
            NOT_SIGNED_IN.to_string()
        }
    })?;
    let body: Value = serde_json::from_str(&raw)
        .map_err(|_| format!("{} is not readable JSON", path.display()))?;
    let oauth = body.get("claudeAiOauth").unwrap_or(&body);
    let access_token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| NOT_SIGNED_IN.to_string())?;
    Ok(StoredTokens {
        access_token: access_token.to_string(),
        expires_at: oauth.get("expiresAt").and_then(Value::as_i64),
        subscription_type: oauth
            .get("subscriptionType")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|plan| !plan.is_empty())
            .map(str::to_string),
    })
}

/// Ask the usage endpoint with the stored token, read-only.
#[cfg(not(test))]
async fn fetch(stored: &StoredTokens) -> Result<Value, String> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    if stored.expires_at.is_some_and(|at| at <= now_ms) {
        return Err("the Claude Code sign-in has expired; open `claude` to refresh it".into());
    }
    let client = crate::llm::oauth_http_builder(USAGE_TIMEOUT)
        .build()
        .map_err(|err| format!("building the usage client: {err}"))?;
    let response = client
        .get(USAGE_URL)
        .bearer_auth(&stored.access_token)
        .header("anthropic-beta", OAUTH_BETA)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| "the usage endpoint did not answer".to_string())?;
    let status = response.status();
    if !status.is_success() {
        // The body is dropped: an error page is no place to find numbers, and
        // it must not land in the chat.
        return Err(format!("usage endpoint: HTTP {}", status.as_u16()));
    }
    response
        .json()
        .await
        .map_err(|_| "the usage endpoint did not answer JSON".to_string())
}

/// The block from what the endpoint said. `plan` is the subscription type the
/// sign-in was made with, which is the only place Anthropic names the plan.
pub fn to_subscription(fetched: Result<Value, String>, plan: Option<String>) -> Subscription {
    let mut block = Subscription {
        plan: plan
            .as_deref()
            .map(str::trim)
            .filter(|plan| !plan.is_empty() && *plan != "unknown")
            .map(plan_label),
        ..Subscription::new("claude", "Claude")
    };
    let body = match fetched {
        Ok(body) => body,
        Err(reason) => {
            block.note = Some(format!("could not read usage: {reason}"));
            return block;
        }
    };
    // Short window first, the order every block is read in.
    for (key, label, minutes) in [
        ("five_hour", "5h", 300),
        ("seven_day", "weekly", 10080),
        ("seven_day_opus", "weekly (Opus)", 10080),
    ] {
        if let Some(window) = window(&body, key, label, minutes) {
            block.windows.push(window);
        }
    }
    if block.windows.is_empty() {
        block.note = Some("the account reported no metered limits".into());
        return block;
    }
    block.source = Some(Source::Account);
    block.products = breakdown(&body);
    block
}

/// One named window: `utilization` out of 100, with the moment it resets.
fn window(body: &Value, key: &str, label: &str, minutes: i64) -> Option<LimitWindow> {
    let raw = body.get(key)?;
    let used_percent = raw
        .get("utilization")
        .and_then(Value::as_f64)
        .filter(|percent| percent.is_finite() && *percent >= 0.0)?;
    Some(LimitWindow {
        label: label.to_string(),
        used_percent,
        window_minutes: Some(minutes),
        resets_at: timestamp(raw.get("resets_at")),
    })
}

/// How the week splits across Claude Code, chats and the rest. Anthropic
/// names the rows; they are shown as they come, in its order.
fn breakdown(body: &Value) -> Vec<ProductShare> {
    let Some(rows) = body
        .get("seven_day_breakdown")
        .and_then(|breakdown| breakdown.get("rows"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let label = row
                .get("display_name")
                .or_else(|| row.get("key"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|label| !label.is_empty())?;
            let used_percent = row
                .get("percent")
                .and_then(Value::as_f64)
                .filter(|percent| percent.is_finite() && *percent >= 0.0)?;
            Some(ProductShare {
                label: label.to_string(),
                used_percent,
            })
        })
        .collect()
}

fn timestamp(value: Option<&Value>) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = value?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.with_timezone(&chrono::Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Max account's body, as `/api/oauth/usage` answers it. The fields
    /// this module does not read are left in so a future one can be added
    /// against the real shape.
    fn body() -> Value {
        serde_json::json!({
            "five_hour": {
                "utilization": 37.0,
                "resets_at": "2026-11-03T19:00:00.113702+00:00",
                "limit_dollars": null
            },
            "seven_day": {
                "utilization": 100.0,
                "resets_at": "2026-11-05T16:00:00.113702+00:00",
                "limit_dollars": null,
                "locked_reason": null
            },
            "seven_day_opus": null,
            "extra_usage": {"is_enabled": false},
            "seven_day_breakdown": {
                "as_of": "2026-11-03T16:30:17.584004+00:00",
                "rows": [
                    {"key": "claude_code", "display_name": "Claude Code", "percent": 94},
                    {"key": "chat", "display_name": "Chats", "percent": 6},
                    {"key": "cowork", "display_name": "Cowork", "percent": 0},
                    {"key": "other", "display_name": "Other", "percent": 0}
                ]
            }
        })
    }

    #[test]
    fn both_windows_and_the_week_split_make_a_block() {
        let block = to_subscription(Ok(body()), Some("max".into()));
        assert_eq!(block.plan.as_deref(), Some("Max"));
        assert_eq!(block.source, Some(Source::Account));
        assert_eq!(
            crate::subscription_usage::render(&[block], Some("claude")),
            "Claude (Max, active)\n  \
             5h: 37% used, resets 2026-11-03 19:00 UTC\n  \
             weekly: 100% used, resets 2026-11-05 16:00 UTC\n  \
             Claude Code 94%, Chats 6%, Cowork 0%, Other 0%"
        );
    }

    #[test]
    fn the_opus_week_is_its_own_window_when_the_plan_meters_one() {
        let mut body = body();
        body["seven_day_opus"] = serde_json::json!({
            "utilization": 12.5,
            "resets_at": "2026-11-05T16:00:00+00:00"
        });
        let block = to_subscription(Ok(body), Some("max".into()));
        let labels: Vec<&str> = block.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["5h", "weekly", "weekly (Opus)"]);
        assert_eq!(block.windows[2].used_percent, 12.5);
    }

    #[test]
    fn a_window_the_plan_does_not_meter_is_left_out_not_shown_as_zero() {
        let mut body = body();
        body["five_hour"] = Value::Null;
        let block = to_subscription(Ok(body), None);
        let labels: Vec<&str> = block.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["weekly"]);
        assert_eq!(block.plan, None);
    }

    #[test]
    fn a_body_with_no_windows_says_so_rather_than_showing_nothing() {
        let block = to_subscription(
            Ok(serde_json::json!({"extra_usage": {}})),
            Some("pro".into()),
        );
        assert!(block.windows.is_empty());
        assert_eq!(
            crate::subscription_usage::render(&[block], None),
            "Claude (Pro)\n  the account reported no metered limits"
        );
    }

    #[test]
    fn a_failed_read_keeps_the_plan_and_names_the_failure() {
        let block = to_subscription(Err("usage endpoint: HTTP 401".into()), Some("max".into()));
        assert_eq!(block.plan.as_deref(), Some("Max"));
        assert_eq!(
            block.note.as_deref(),
            Some("could not read usage: usage endpoint: HTTP 401")
        );
        assert!(block.windows.is_empty());
        assert_eq!(block.source, None);
    }

    #[test]
    fn a_garbled_percent_is_skipped_rather_than_read_as_zero() {
        let mut body = body();
        body["five_hour"]["utilization"] = serde_json::json!("lots");
        let block = to_subscription(Ok(body), None);
        let labels: Vec<&str> = block.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["weekly"]);
    }

    #[test]
    fn a_window_without_a_reset_still_reports_its_percent() {
        let body = serde_json::json!({"seven_day": {"utilization": 4.0}});
        let block = to_subscription(Ok(body), None);
        assert_eq!(block.windows[0].resets_at, None);
        assert_eq!(
            crate::subscription_usage::render(&[block], None),
            "Claude\n  weekly: 4% used"
        );
    }
}
