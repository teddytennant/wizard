//! What each signed-in subscription has used: `/usage`, and
//! `wizard usage --subscriptions [--json]` for the desktop app.
//!
//! A subscription here is an account sign-in (`/login xai`,
//! `wizard --login chatgpt`), not an API key. Each one reports its own plan
//! limits: xAI a weekly credit percent split by product, ChatGPT a 5-hour and
//! a weekly window. Every signed-in subscription gets a block whichever
//! provider is active, since the limits belong to the account and not to the
//! chat that asks.
//!
//! Claude and Cursor are subscriptions Wizard never signs in to: their
//! sign-ins belong to `claude` and `cursor-agent`, which the user may have on
//! the machine for reasons that have nothing to do with Wizard. So those two
//! are reported only once the provider has been added (`/provider`), and
//! reading another tool's credential follows the user asking for it rather
//! than the file being there. Claude reports a 5-hour and a weekly window
//! with the week split across Claude Code, chats and the rest; Cursor the
//! billing cycle with its auto and API meters.
//!
//! The numbers never come from the local token ledger. When an account did
//! not report a number the block says so instead of filling the gap.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The machine-readable form `wizard usage --subscriptions --json` prints.
/// `version` moves when a field changes meaning; new optional fields do not
/// move it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub version: u32,
    /// Signed-in subscriptions only. Empty when there are none.
    pub subscriptions: Vec<Subscription>,
}

pub const REPORT_VERSION: u32 = 1;

impl Report {
    pub fn new(subscriptions: Vec<Subscription>) -> Self {
        Self {
            version: REPORT_VERSION,
            subscriptions,
        }
    }
}

/// One signed-in subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    /// `xai`, `chatgpt`, `claude` or `cursor`.
    pub id: String,
    /// `xAI`, `ChatGPT`, `Claude` or `Cursor`.
    pub name: String,
    /// The plan as the account names it ("SuperGrok Heavy", "Pro").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The metered windows, short one first. Empty when there is no reading.
    #[serde(default)]
    pub windows: Vec<LimitWindow>,
    /// The labelled meters beside the windows: xAI's split across build, chat
    /// and voice, Claude's week split across Claude Code and chats, Cursor's
    /// auto and API percents. A split in the first two cases, two meters over
    /// one cycle in Cursor's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub products: Vec<ProductShare>,
    /// Where the windows came from, when there are any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// Why there are no windows, or what is off about them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Subscription {
    pub fn new(id: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            plan: None,
            windows: Vec::new(),
            products: Vec::new(),
            source: None,
            note: None,
        }
    }
}

/// One rate-limit window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    /// `5h`, `weekly`, `monthly`, …
    pub label: String,
    /// 0-100.
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
}

/// One labelled meter beside a window: a product's part of the usage, or a
/// second percent over the same period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductShare {
    pub label: String,
    pub used_percent: f64,
}

/// Where a reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// Asked the account just now.
    Account,
    /// The headers of the last reply this process received.
    LastReply,
}

/// Every subscription to report, read live.
///
/// `added` is the `kind` of every provider in the config file. Wizard's own
/// sign-ins are asked whether or not they are among them; Claude and Cursor
/// only when they are, since their credentials belong to another tool.
///
/// They are asked at once; the slowest decides how long `/usage` takes.
pub async fn collect(added: &[crate::config::ProviderKind]) -> Vec<Subscription> {
    use crate::config::ProviderKind;
    let has = |kind: &ProviderKind| added.contains(kind);
    let (xai, chatgpt, claude, cursor) = tokio::join!(
        crate::llm::xai_oauth::subscription(),
        chatgpt(),
        optional(has(&ProviderKind::CLAUDE_CODE), claude()),
        optional(has(&ProviderKind::CURSOR), cursor()),
    );
    [xai, chatgpt, claude, cursor]
        .into_iter()
        .flatten()
        .collect()
}

/// Run `ask` only when `wanted`. The future is still built either way, so
/// the four reads can go into one `join!`.
async fn optional(
    wanted: bool,
    ask: impl std::future::Future<Output = Option<Subscription>>,
) -> Option<Subscription> {
    if wanted { ask.await } else { None }
}

#[cfg(feature = "provider-chatgpt")]
async fn chatgpt() -> Option<Subscription> {
    crate::plugins::chatgpt::limits::subscription().await
}

#[cfg(not(feature = "provider-chatgpt"))]
async fn chatgpt() -> Option<Subscription> {
    None
}

#[cfg(feature = "provider-claude-code")]
async fn claude() -> Option<Subscription> {
    crate::plugins::claude_code::limits::subscription().await
}

#[cfg(not(feature = "provider-claude-code"))]
async fn claude() -> Option<Subscription> {
    None
}

#[cfg(feature = "provider-cursor")]
async fn cursor() -> Option<Subscription> {
    crate::plugins::cursor::limits::subscription().await
}

#[cfg(not(feature = "provider-cursor"))]
async fn cursor() -> Option<Subscription> {
    None
}

/// `wizard usage --subscriptions [--json]`. Exits 0 whatever the accounts
/// said: a failed read is a note in the output, not a failed command.
pub async fn run_cli(json: bool) -> anyhow::Result<i32> {
    let subscriptions = collect(&crate::config::Config::provider_kinds_on_disk()).await;
    if json {
        println!("{}", serde_json::to_string(&Report::new(subscriptions))?);
    } else {
        println!("{}", render(&subscriptions, None));
    }
    Ok(0)
}

/// Which subscription the active provider is, if it is one.
pub fn active_id(kind: &crate::config::ProviderKind) -> Option<&'static str> {
    use crate::config::ProviderKind;
    if *kind == ProviderKind::XAI_OAUTH {
        Some("xai")
    } else if *kind == ProviderKind::CHATGPT_OAUTH {
        Some("chatgpt")
    } else if *kind == ProviderKind::CLAUDE_CODE {
        Some("claude")
    } else if *kind == ProviderKind::CURSOR {
        Some("cursor")
    } else {
        None
    }
}

/// `/usage` as text: a block per subscription, the active one marked.
pub fn render(subscriptions: &[Subscription], active: Option<&str>) -> String {
    if subscriptions.is_empty() {
        return "no subscription signed in. /usage shows plan limits for account \
                sign-ins: /login xai, or `wizard --login chatgpt`. A Claude or \
                Cursor subscription shows once you add it with /provider"
            .to_string();
    }
    let mut text = String::new();
    for (ix, subscription) in subscriptions.iter().enumerate() {
        if ix > 0 {
            text.push('\n');
        }
        render_one(
            &mut text,
            subscription,
            active == Some(subscription.id.as_str()),
        );
    }
    text.trim_end().to_string()
}

fn render_one(text: &mut String, subscription: &Subscription, active: bool) {
    let mut head = subscription.name.clone();
    let mut tags = Vec::new();
    if let Some(plan) = &subscription.plan {
        tags.push(plan.clone());
    }
    if active {
        tags.push("active".to_string());
    }
    if !tags.is_empty() {
        let _ = write!(head, " ({})", tags.join(", "));
    }
    let _ = writeln!(text, "{head}");
    for window in &subscription.windows {
        let _ = write!(
            text,
            "  {}: {} used",
            window.label,
            fmt_percent(window.used_percent)
        );
        if let Some(reset) = window.resets_at {
            let _ = write!(text, ", resets {}", reset.format("%Y-%m-%d %H:%M UTC"));
        }
        text.push('\n');
    }
    if !subscription.products.is_empty() {
        let products: Vec<String> = subscription
            .products
            .iter()
            .map(|share| format!("{} {}", share.label, fmt_percent(share.used_percent)))
            .collect();
        let _ = writeln!(text, "  {}", products.join(", "));
    }
    if subscription.source == Some(Source::LastReply) && !subscription.windows.is_empty() {
        let _ = writeln!(text, "  as of the last reply");
    }
    if let Some(note) = &subscription.note {
        let _ = writeln!(text, "  {note}");
    }
}

/// `74%`, `12.5%`.
pub fn fmt_percent(number: f64) -> String {
    if number.fract().abs() < 0.05 {
        format!("{}%", number.round() as i64)
    } else {
        format!("{number:.1}%")
    }
}

/// A window's name from its length, the way the Codex CLI names them: within
/// 5% of 5 hours, a day, a week, 30 days or a year. Anything else is its
/// length in hours or days, and no length at all is `fallback`.
pub fn window_label(minutes: Option<i64>, fallback: &str) -> String {
    let Some(minutes) = minutes.filter(|m| *m > 0) else {
        return fallback.to_string();
    };
    const HOUR: i64 = 60;
    const DAY: i64 = 24 * HOUR;
    let near = |expected: i64| {
        let (m, e) = (minutes as f64, expected as f64);
        m >= e * 0.95 && m <= e * 1.05
    };
    if near(5 * HOUR) {
        "5h".into()
    } else if near(DAY) {
        "daily".into()
    } else if near(7 * DAY) {
        "weekly".into()
    } else if near(30 * DAY) {
        "monthly".into()
    } else if near(365 * DAY) {
        "annual".into()
    } else if minutes < 2 * DAY {
        format!("{}h", (minutes + HOUR / 2) / HOUR)
    } else {
        format!("{}d", (minutes + DAY / 2) / DAY)
    }
}

/// `pro` → `Pro`, `self_serve_business_usage_based` → `Self serve business
/// usage based`. The plan id is the account's; this only makes it readable.
pub fn plan_label(raw: &str) -> String {
    let spaced = raw.trim().replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> Option<DateTime<Utc>> {
        Utc.timestamp_opt(secs, 0).single()
    }

    fn xai() -> Subscription {
        Subscription {
            plan: Some("SuperGrok Heavy".into()),
            windows: vec![LimitWindow {
                label: "weekly".into(),
                used_percent: 74.0,
                window_minutes: Some(10080),
                resets_at: at(1_790_283_999),
            }],
            products: vec![
                ProductShare {
                    label: "build".into(),
                    used_percent: 63.0,
                },
                ProductShare {
                    label: "chat".into(),
                    used_percent: 10.0,
                },
                ProductShare {
                    label: "voice".into(),
                    used_percent: 1.0,
                },
            ],
            source: Some(Source::Account),
            ..Subscription::new("xai", "xAI")
        }
    }

    fn chatgpt() -> Subscription {
        Subscription {
            plan: Some("Plus".into()),
            windows: vec![
                LimitWindow {
                    label: "5h".into(),
                    used_percent: 12.5,
                    window_minutes: Some(300),
                    resets_at: at(1_790_550_600),
                },
                LimitWindow {
                    label: "weekly".into(),
                    used_percent: 41.0,
                    window_minutes: Some(10080),
                    resets_at: at(1_790_985_600),
                },
            ],
            source: Some(Source::Account),
            ..Subscription::new("chatgpt", "ChatGPT")
        }
    }

    #[test]
    fn xai_alone_shows_the_week_and_the_products() {
        assert_eq!(
            render(&[xai()], Some("xai")),
            "xAI (SuperGrok Heavy, active)\n  \
             weekly: 74% used, resets 2026-09-24 21:06 UTC\n  \
             build 63%, chat 10%, voice 1%"
        );
    }

    #[test]
    fn chatgpt_alone_shows_both_windows() {
        assert_eq!(
            render(&[chatgpt()], None),
            "ChatGPT (Plus)\n  \
             5h: 12.5% used, resets 2026-09-27 23:10 UTC\n  \
             weekly: 41% used, resets 2026-10-03 00:00 UTC"
        );
    }

    #[test]
    fn both_get_a_block_whichever_is_active() {
        let text = render(&[xai(), chatgpt()], Some("chatgpt"));
        assert!(text.starts_with("xAI (SuperGrok Heavy)\n"), "{text}");
        assert!(text.contains("\nChatGPT (Plus, active)\n"), "{text}");
        assert_eq!(text.matches("resets").count(), 3);
    }

    #[test]
    fn neither_says_how_to_sign_in() {
        let text = render(&[], None);
        assert!(text.starts_with("no subscription signed in"), "{text}");
        assert!(text.contains("/login xai"));
        assert!(text.contains("wizard --login chatgpt"));
    }

    #[test]
    fn signed_in_without_a_reading_says_so_plainly() {
        let waiting = Subscription {
            plan: Some("Pro".into()),
            note: Some("no request yet this session; limits show after the first reply".into()),
            ..Subscription::new("chatgpt", "ChatGPT")
        };
        assert_eq!(
            render(&[waiting], None),
            "ChatGPT (Pro)\n  no request yet this session; limits show after the first reply"
        );
    }

    #[test]
    fn a_reading_from_the_last_reply_says_where_it_came_from() {
        let from_reply = Subscription {
            source: Some(Source::LastReply),
            ..chatgpt()
        };
        assert!(render(&[from_reply], None).ends_with("\n  as of the last reply"));
    }

    #[test]
    fn the_json_schema_is_stable() {
        let report = Report::new(vec![xai(), chatgpt()]);
        let value = serde_json::to_value(&report).expect("json");
        assert_eq!(value["version"], 1);
        let first = &value["subscriptions"][0];
        assert_eq!(first["id"], "xai");
        assert_eq!(first["name"], "xAI");
        assert_eq!(first["plan"], "SuperGrok Heavy");
        assert_eq!(first["source"], "account");
        assert_eq!(first["windows"][0]["label"], "weekly");
        assert_eq!(first["windows"][0]["usedPercent"], 74.0);
        assert_eq!(first["windows"][0]["windowMinutes"], 10080);
        assert_eq!(first["windows"][0]["resetsAt"], "2026-09-24T21:06:39Z");
        assert_eq!(first["products"][1]["label"], "chat");
        let second = &value["subscriptions"][1];
        assert!(
            second.get("products").is_none(),
            "empty products are left out"
        );
        assert!(second.get("note").is_none());
        let back: Report = serde_json::from_value(value).expect("round trip");
        assert_eq!(back, report);
    }

    #[test]
    fn a_note_only_subscription_serializes_with_empty_windows() {
        let value = serde_json::to_value(Report::new(vec![Subscription {
            note: Some("could not read usage".into()),
            ..Subscription::new("xai", "xAI")
        }]))
        .expect("json");
        assert_eq!(value["subscriptions"][0]["windows"], serde_json::json!([]));
        assert_eq!(value["subscriptions"][0]["note"], "could not read usage");
    }

    #[test]
    fn windows_are_named_by_length() {
        assert_eq!(window_label(Some(300), "usage"), "5h");
        assert_eq!(window_label(Some(10080), "usage"), "weekly");
        assert_eq!(window_label(Some(43200), "usage"), "monthly");
        assert_eq!(window_label(Some(1440), "usage"), "daily");
        assert_eq!(window_label(Some(180), "usage"), "3h");
        assert_eq!(window_label(Some(4320), "usage"), "3d");
        assert_eq!(window_label(None, "usage"), "usage");
        assert_eq!(window_label(Some(0), "secondary"), "secondary");
    }

    #[test]
    fn plan_ids_read_as_words() {
        assert_eq!(plan_label("pro"), "Pro");
        assert_eq!(plan_label("edu_plus"), "Edu plus");
        assert_eq!(plan_label(""), "");
    }

    #[test]
    fn subscription_kinds_are_the_oauth_ones() {
        use crate::config::ProviderKind;
        assert_eq!(active_id(&ProviderKind::XAI_OAUTH), Some("xai"));
        assert_eq!(active_id(&ProviderKind::CHATGPT_OAUTH), Some("chatgpt"));
        assert_eq!(active_id(&ProviderKind::XAI), None);
    }
}
