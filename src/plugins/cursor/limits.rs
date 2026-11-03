//! The Cursor subscription's plan limits: the included-usage meters the
//! Cursor dashboard shows for the current billing cycle.
//!
//! The provider itself never reads Cursor's files (see the module docs). This
//! does, and only this: `cursor-agent about` names the plan but no number, and
//! a `cursor-agent -p` run to ask would spend the budget it is reporting on.
//!
//! `~/.config/cursor/auth.json` holds the session token `cursor-agent login`
//! wrote. `GET cursor.com/api/usage-summary` answers it with the billing cycle,
//! the membership type, and `totalPercentUsed` next to the `auto` and `api`
//! meters behind the two sentences the dashboard prints. Reading costs no
//! tokens.
//!
//! The site authenticates a browser session, so the token goes back as the
//! `WorkosCursorSessionToken` cookie it was issued as: the account id from the
//! token's own `sub` claim, `::`, then the token. It is read, never
//! refreshed.

#[cfg(not(test))]
use std::path::PathBuf;

use serde_json::Value;

use crate::subscription_usage::{LimitWindow, ProductShare, Source, Subscription, plan_label};

/// The usage endpoint. Fixed rather than derived from a configured endpoint,
/// so the session token only ever goes to Cursor.
#[cfg(not(test))]
const USAGE_URL: &str = "https://cursor.com/api/usage-summary";
#[cfg(not(test))]
const USAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What the block says when `cursor-agent` is not signed in on this machine.
#[cfg(not(test))]
const NOT_SIGNED_IN: &str = "not signed in; run `cursor-agent login`";

/// `/usage`'s Cursor block. Always a block: this is only asked once the user
/// has added Cursor as a provider, so "nothing to report" is an answer they
/// are owed rather than a row to hide.
///
/// Tests never reach the network: the suite dispatches every slash command,
/// and a live call would read the account of whoever runs it. The parsing
/// below is tested with a captured body.
pub async fn subscription() -> Option<Subscription> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        let cookie = match read_cookie() {
            Ok(cookie) => cookie,
            Err(reason) => {
                return Some(Subscription {
                    note: Some(reason),
                    ..Subscription::new("cursor", "Cursor")
                });
            }
        };
        Some(to_subscription(fetch(&cookie).await))
    }
}

/// Where `cursor-agent` keeps the sign-in: the XDG config directory on every
/// platform, since the CLI writes it from the home directory rather than the
/// OS application-data path.
#[cfg(not(test))]
fn auth_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::home_dir()?.join(".config"),
    };
    Some(base.join("cursor").join("auth.json"))
}

/// Read the stored sign-in and build the cookie for it. The `Err` is the
/// sentence the block shows.
#[cfg(not(test))]
fn read_cookie() -> Result<String, String> {
    let path = auth_path().ok_or_else(|| NOT_SIGNED_IN.to_string())?;
    let raw = std::fs::read_to_string(&path).map_err(|_| NOT_SIGNED_IN.to_string())?;
    let body: Value = serde_json::from_str(&raw)
        .map_err(|_| format!("{} is not readable JSON", path.display()))?;
    let token = body
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| NOT_SIGNED_IN.to_string())?;
    session_cookie(token)
        .ok_or_else(|| format!("{NOT_SIGNED_IN} (the stored session is not one Wizard can read)"))
}

/// Ask the usage endpoint with the stored session, read-only.
#[cfg(not(test))]
async fn fetch(cookie: &str) -> Result<Value, String> {
    let client = crate::llm::oauth_http_builder(USAGE_TIMEOUT)
        .build()
        .map_err(|err| format!("building the usage client: {err}"))?;
    let response = client
        .get(USAGE_URL)
        .header(reqwest::header::COOKIE, cookie)
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

/// The cookie the site issued this token as: `<account id>::<token>`,
/// percent-encoded, where the account id is the token's own `sub` claim. A
/// token the claim cannot be read out of is no use here, so it is a miss
/// rather than a request that would come back unauthenticated.
pub fn session_cookie(token: &str) -> Option<String> {
    let subject = jwt_subject(token)?;
    Some(format!(
        "WorkosCursorSessionToken={}%3A%3A{token}",
        percent_encode(&subject)
    ))
}

/// The `sub` claim of a JWT, without verifying it: the signature is the
/// server's to check, and this only needs the name the cookie pairs the token
/// with.
fn jwt_subject(token: &str) -> Option<String> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .map(str::to_string)
}

/// Percent-encode everything a cookie value may not carry. Cursor's account
/// ids hold a `|`, which is one of them.
fn percent_encode(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// The block from what the endpoint said.
///
/// The percents are the endpoint's own, not a ratio worked out from `used`
/// and `limit`: those two are the dollar figures behind the cycle, and the
/// percents are what Cursor's dashboard shows against the plan's allowance.
pub fn to_subscription(fetched: Result<Value, String>) -> Subscription {
    let mut block = Subscription::new("cursor", "Cursor");
    let body = match fetched {
        Ok(body) => body,
        Err(reason) => {
            block.note = Some(format!("could not read usage: {reason}"));
            return block;
        }
    };
    block.plan = body
        .get("membershipType")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty() && *plan != "unknown")
        .map(plan_label);
    if body.get("isUnlimited").and_then(Value::as_bool) == Some(true) {
        block.note = Some("this plan has no metered limit".into());
        return block;
    }
    let plan_usage = body
        .get("individualUsage")
        .and_then(|usage| usage.get("plan"));
    let Some(used_percent) = plan_usage.and_then(|usage| percent(usage.get("totalPercentUsed")))
    else {
        block.note = Some("the account reported no metered limits".into());
        return block;
    };
    let (start, end) = (
        timestamp(body.get("billingCycleStart")),
        timestamp(body.get("billingCycleEnd")),
    );
    block.windows.push(LimitWindow {
        // The window is the billing cycle, so it is named for what it is
        // rather than for its length: a 28-day February is still the month.
        label: "monthly".to_string(),
        used_percent,
        window_minutes: start
            .zip(end)
            .map(|(start, end)| (end - start).num_minutes())
            .filter(|minutes| *minutes > 0),
        resets_at: end,
    });
    block.source = Some(Source::Account);
    // Not a split of the window above: these are the two meters Cursor runs
    // over the same cycle, one for auto-selected models and one for the rest.
    for (label, key) in [("auto", "autoPercentUsed"), ("API", "apiPercentUsed")] {
        if let Some(used_percent) = plan_usage.and_then(|usage| percent(usage.get(key))) {
            block.products.push(ProductShare {
                label: label.to_string(),
                used_percent,
            });
        }
    }
    if on_demand_enabled(&body) {
        block.note = Some("on-demand usage is on, so the cycle does not stop here".into());
    }
    block
}

fn on_demand_enabled(body: &Value) -> bool {
    body.get("individualUsage")
        .and_then(|usage| usage.get("onDemand"))
        .and_then(|on_demand| on_demand.get("enabled"))
        .and_then(Value::as_bool)
        == Some(true)
}

fn percent(value: Option<&Value>) -> Option<f64> {
    value?
        .as_f64()
        .filter(|percent| percent.is_finite() && *percent >= 0.0)
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

    /// An Ultra account's body, as `/api/usage-summary` answers it.
    fn body() -> Value {
        serde_json::json!({
            "billingCycleStart": "2026-10-11T18:52:08.000Z",
            "billingCycleEnd": "2026-11-11T18:52:08.000Z",
            "membershipType": "ultra",
            "limitType": "user",
            "isUnlimited": false,
            "autoModelSelectedDisplayMessage": "You've used 6% of your included total usage",
            "namedModelSelectedDisplayMessage": "You've used 15% of your included API usage",
            "individualUsage": {
                "plan": {
                    "enabled": true,
                    "used": 19138,
                    "limit": 40000,
                    "remaining": 20862,
                    "autoPercentUsed": 5.873666666666667,
                    "apiPercentUsed": 15.17,
                    "totalPercentUsed": 6.1735483870967744
                },
                "onDemand": {"enabled": false, "used": 0, "limit": null}
            },
            "teamUsage": {}
        })
    }

    #[test]
    fn the_cycle_and_both_meters_make_a_block() {
        let block = to_subscription(Ok(body()));
        assert_eq!(block.plan.as_deref(), Some("Ultra"));
        assert_eq!(block.source, Some(Source::Account));
        assert_eq!(block.windows[0].window_minutes, Some(44640));
        assert_eq!(
            crate::subscription_usage::render(&[block], Some("cursor")),
            "Cursor (Ultra, active)\n  \
             monthly: 6.2% used, resets 2026-11-11 18:52 UTC\n  \
             auto 5.9%, API 15.2%"
        );
    }

    #[test]
    fn the_percent_is_cursors_own_not_used_over_limit() {
        let block = to_subscription(Ok(body()));
        // 19138 of 40000 is 47.8%, and it is not what the dashboard shows.
        assert_eq!(block.windows[0].used_percent, 6.1735483870967744);
    }

    #[test]
    fn an_unlimited_plan_says_so_rather_than_showing_a_percent() {
        let mut body = body();
        body["isUnlimited"] = serde_json::json!(true);
        let block = to_subscription(Ok(body));
        assert_eq!(
            crate::subscription_usage::render(&[block], None),
            "Cursor (Ultra)\n  this plan has no metered limit"
        );
    }

    #[test]
    fn on_demand_usage_is_called_out_under_the_cycle() {
        let mut body = body();
        body["individualUsage"]["onDemand"]["enabled"] = serde_json::json!(true);
        let block = to_subscription(Ok(body));
        assert_eq!(block.windows.len(), 1);
        assert_eq!(
            block.note.as_deref(),
            Some("on-demand usage is on, so the cycle does not stop here")
        );
    }

    #[test]
    fn a_body_with_no_individual_usage_says_so() {
        let block = to_subscription(Ok(serde_json::json!({
            "membershipType": "ultra",
            "limitType": "team",
            "teamUsage": {"plan": {"totalPercentUsed": 3.0}}
        })));
        assert!(block.windows.is_empty());
        assert_eq!(
            block.note.as_deref(),
            Some("the account reported no metered limits")
        );
    }

    #[test]
    fn a_failed_read_names_the_failure() {
        let block = to_subscription(Err("usage endpoint: HTTP 401".into()));
        assert_eq!(
            crate::subscription_usage::render(&[block], None),
            "Cursor\n  could not read usage: usage endpoint: HTTP 401"
        );
    }

    #[test]
    fn a_cycle_without_dates_still_reports_its_percent() {
        let mut body = body();
        body["billingCycleEnd"] = Value::Null;
        let block = to_subscription(Ok(body));
        assert_eq!(block.windows[0].window_minutes, None);
        assert_eq!(block.windows[0].resets_at, None);
    }

    #[test]
    fn the_cookie_pairs_the_token_with_its_own_account_id() {
        // Payload: {"sub":"grok|user_01ABC","aud":"https://cursor.com"}
        let token = "eyJhbGciOiJIUzI1NiJ9.\
                     eyJzdWIiOiJncm9rfHVzZXJfMDFBQkMiLCJhdWQiOiJodHRwczovL2N1cnNvci5jb20ifQ.\
                     c2lnbmF0dXJl";
        assert_eq!(
            session_cookie(token).as_deref(),
            Some(format!("WorkosCursorSessionToken=grok%7Cuser_01ABC%3A%3A{token}").as_str())
        );
    }

    #[test]
    fn a_token_with_no_subject_is_not_worth_a_request() {
        assert_eq!(session_cookie("not-a-jwt"), None);
        // Payload: {"aud":"https://cursor.com"}
        assert_eq!(
            session_cookie("eyJhbGciOiJIUzI1NiJ9.eyJhdWQiOiJodHRwczovL2N1cnNvci5jb20ifQ.c2ln"),
            None
        );
    }
}
