//! Inline composer prompts and paused-turn modals: provider setup, the
//! plan-review modal, the interview modal, and a running command's console.

use crate::agent::{ConsoleGate, ConsoleWriter, InterviewQuestion, PlanVerdict, ultra};
use crate::config::ProviderKind;

/// A field being collected in the inline provider-setup prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PromptField {
    Name,
    /// Cloudflare account id — substituted into the base-URL template before
    /// the provider is built (Cloudflare setup only).
    AccountId,
    BaseUrl,
    Model,
    ApiKey,
}

/// In-progress provider setup driven by composer prompts. Each queued
/// [`PromptField`] is asked in turn; the answers fill the draft, and the last
/// answer emits a [`SlashCommand::ProviderSetup`](crate::commands::SlashCommand::ProviderSetup).
#[derive(Debug, Clone)]
pub struct ProviderPrompt {
    pub(super) kind: ProviderKind,
    pub(super) name: String,
    pub(super) base_url: String,
    pub(super) model: String,
    pub(super) api_key: Option<String>,
    /// Remaining fields to ask, in order.
    pub(super) queue: std::collections::VecDeque<PromptField>,
}

/// Sentinel value of the final "add provider" row in the level-1 provider
/// picker. The dispatch also keys off the last index, but matching the value
/// keeps it robust if the list grows.
pub(super) const PROVIDER_ADD_ROW: &str = "＋ Add provider…";

/// Value of the final row in the `/ultra config` picker: the judge, which is
/// not a lens and must not land in the roster. It is deliberately the same
/// string as [`crate::agent::ultra::JUDGE_NAME`] — `lens_catalog` excludes that
/// name from the lens rows, so this row cannot collide with one of them.
pub(super) const ULTRA_JUDGE_ROW: &str = ultra::JUDGE_NAME;

/// What a row of the level-2 provider-type menu starts when it is chosen.
///
/// The rows used to be dispatched by position: a `match picker.selected` with
/// eight arms and a comment asking the next person to keep two lists in step.
/// A menu that is filtered cannot do that — drop one row and every arm after
/// it starts a different provider than the one on the screen — so the row
/// carries what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderSetup {
    /// The OAuth flow, which adds the provider itself on success.
    XaiSignIn,
    XaiKey,
    OpenRouter,
    Cloudflare,
    OpenAi,
    Anthropic,
    /// Everything prompted, starting with the name.
    Custom,
    /// One of [`crate::llm::compat::PRESETS`], by index into that table. The
    /// index is into a `const` slice compiled into this binary, not into the
    /// filtered menu, so it cannot shift.
    Compat(usize),
}

/// One row of the level-2 provider-type menu.
pub(super) struct ProviderType {
    pub(super) label: String,
    pub(super) detail: String,
    pub(super) setup: ProviderSetup,
    /// The `kind` the row writes into `config.toml`. The row is offered when
    /// this kind is registered and dropped when it is not — an "Anthropic —
    /// API key" row on a build with no Anthropic plugin collects a key, saves
    /// it, and then fails at the first turn.
    pub(super) kind: ProviderKind,
}

/// The provider-type menu this build can actually carry out, in display
/// order.
///
/// Order is the written order, then the OpenAI-compatible presets from
/// [`crate::llm::compat::PRESETS`]. Those are all `kind = "openai"`, so they
/// stand or fall with that one plugin — which is also why they are appended
/// rather than interleaved: they are one backend's worth of base URLs, not
/// seven backends.
///
/// The labels stay written out. A descriptor's `display_name` answers "what
/// is this backend called" ("xAI", "OpenAI-compatible"); these rows answer
/// "which of these should you pick", and "xAI (Grok) — sign in" versus "xAI
/// (Grok) — API key" is a distinction between two ways of paying for the same
/// endpoint that no single descriptor can draw. What is derived from the
/// registry is which rows exist.
pub(super) fn provider_types() -> Vec<ProviderType> {
    let row = |label: &str, detail: &str, setup, kind| ProviderType {
        label: label.to_string(),
        detail: detail.to_string(),
        setup,
        kind,
    };
    let fixed = [
        row(
            "xAI (Grok) — sign in",
            "OAuth · no API key",
            ProviderSetup::XaiSignIn,
            ProviderKind::XAI_OAUTH,
        ),
        row(
            "xAI (Grok) — API key",
            "stored in ~/.wizard/credentials.toml",
            ProviderSetup::XaiKey,
            ProviderKind::XAI,
        ),
        row(
            "OpenRouter — API key",
            "openrouter.ai",
            ProviderSetup::OpenRouter,
            ProviderKind::OPENROUTER,
        ),
        row(
            "Cloudflare Workers AI — API token",
            "GLM 5.2 · account id + token",
            ProviderSetup::Cloudflare,
            ProviderKind::CLOUDFLARE,
        ),
        row(
            "OpenAI — API key",
            "api.openai.com",
            ProviderSetup::OpenAi,
            ProviderKind::OPENAI,
        ),
        row(
            "Anthropic (Claude) — API key",
            "api.anthropic.com",
            ProviderSetup::Anthropic,
            ProviderKind::ANTHROPIC,
        ),
        row(
            "OpenAI-compatible — custom",
            "any base URL + key",
            ProviderSetup::Custom,
            ProviderKind::OPENAI,
        ),
    ];
    let presets = crate::llm::compat::PRESETS
        .iter()
        .enumerate()
        .map(|(index, preset)| {
            row(
                &format!("{} — API key", preset.label),
                preset.detail,
                ProviderSetup::Compat(index),
                ProviderKind::OPENAI,
            )
        });

    let installed = crate::llm::registry::kinds();
    fixed
        .into_iter()
        .chain(presets)
        .filter(|row| installed.contains(&row.kind))
        .collect()
}

/// The `web_search` backend menu (`/settings`): `(id, label, detail)`. The id
/// is what gets written to `[web] search_backend` and (for keyed backends) the
/// `~/.wizard/credentials.toml` key name; the order is the display order.
pub(super) const WEB_BACKENDS: &[(&str, &str, &str)] = &[
    ("duckduckgo", "DuckDuckGo", "free · no API key"),
    ("brave", "Brave Search", "API key · brave.com/search/api"),
    ("tavily", "Tavily", "API key · tavily.com"),
    ("exa", "Exa", "API key · exa.ai"),
    ("serper", "Serper (Google)", "API key · serper.dev"),
    ("xai", "xAI (Grok)", "sign in with xAI, or API key"),
];

/// Display label for a `web_search` backend id (falls back to the id itself).
pub(super) fn web_backend_label(id: &str) -> &str {
    match id {
        "grok" => "xAI (Grok)",
        other => WEB_BACKENDS
            .iter()
            .find(|(value, _, _)| *value == other)
            .map(|(_, label, _)| *label)
            .unwrap_or(other),
    }
}

/// Whether a keyed `web_search` backend needs a pasted API key (vs DuckDuckGo,
/// which needs none, and xAI, which can use the OAuth session).
pub(super) fn web_backend_needs_key(id: &str) -> bool {
    matches!(id, "brave" | "tavily" | "exa" | "serper")
}

/// Whether an xAI OAuth session already exists on disk (`wizard --login xai`),
/// so web search can reuse it without a fresh sign-in.
pub(super) fn xai_oauth_session_present() -> bool {
    crate::llm::xai_oauth::token_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

/// Human-readable provider name for a kind, used in inline prompt questions.
///
/// The backend's own descriptor answers this now. A kind nothing has
/// registered falls back to its id, which is the string the user typed or
/// configured — worse than "OpenAI-compatible" but never wrong, and it keeps
/// a prompt for an unknown backend readable instead of blank.
pub(super) fn provider_display(kind: &ProviderKind) -> String {
    crate::llm::registry::installed(kind)
        .map(|descriptor| descriptor.display_name().to_string())
        .unwrap_or_else(|| kind.to_string())
}

/// The question shown when collecting `field` for the in-progress `prompt`.
pub(super) fn prompt_question(field: PromptField, prompt: &ProviderPrompt) -> String {
    match field {
        PromptField::Name => "Provider name (id):".to_string(),
        PromptField::AccountId => {
            "Cloudflare account ID (dash.cloudflare.com → Workers AI → account id):".to_string()
        }
        PromptField::BaseUrl => "Base URL:".to_string(),
        PromptField::Model => "Model:".to_string(),
        PromptField::ApiKey => format!(
            "Paste your {} API key, then Enter (input hidden):",
            provider_display(&prompt.kind)
        ),
    }
}

/// In-flight plan review (plan mode): the model called `exit_plan` and the
/// turn is paused inside the tool until a [`PlanVerdict`] is sent back.
#[derive(Debug)]
pub struct PlanReview {
    /// The plan markdown, rendered in the review modal.
    pub plan: String,
    /// Verdict channel back into the paused `exit_plan` call; taken exactly
    /// once when the review finishes.
    pub(super) respond: Option<tokio::sync::oneshot::Sender<PlanVerdict>>,
    /// `Some` while collecting rejection feedback (the text typed so far).
    pub feedback: Option<String>,
    /// Scroll offset from the top of the plan.
    pub scroll: u16,
}

/// In-flight interview (plan mode): the model called `interview` and the turn
/// is paused inside the tool until the user answers every question or
/// dismisses the modal.
#[derive(Debug)]
pub struct Interview {
    /// The questions, in order.
    pub questions: Vec<InterviewQuestion>,
    /// Answers collected so far, one per answered question (parallel to
    /// `questions[..current]`).
    pub answers: Vec<String>,
    /// Index of the question currently being answered.
    pub current: usize,
    /// The answer being typed for the current question.
    pub input: String,
    /// Answer channel back into the paused `interview` call; taken exactly
    /// once when the interview finishes (`Some(answers)`) or is dismissed
    /// (`None`).
    pub(super) respond: Option<tokio::sync::oneshot::Sender<Option<Vec<String>>>>,
}

impl Interview {
    /// The question now being answered, if any remain.
    pub fn current_question(&self) -> Option<&InterviewQuestion> {
        self.questions.get(self.current)
    }
}

/// A running foreground shell command whose stdin this composer is driving
/// ([`AgentEvent::ConsoleOpened`](crate::agent::AgentEvent::ConsoleOpened)).
///
/// This is not a modal. The transcript keeps scrolling, the agent keeps
/// working, and the only thing that changes is where Enter goes — which is
/// exactly why the composer has to *say* it changed, and why
/// [`crate::ui::draw`] repaints the prompt glyph, the rule above it and the
/// status hints for as long as this is `Some`. A composer that quietly meant
/// something else would be a worse bug than the one consoles fix.
#[derive(Debug)]
pub struct Console {
    /// The command line, so the banner can name what is being typed at.
    pub command: String,
    /// The ticket this console arrived on. Kept so a `ConsoleClosed` for some
    /// *other* command cannot close this one — two `execute` calls in one turn
    /// are sequential today, but "today" is not a thing to key correctness on.
    pub gate: ConsoleGate,
    /// Writer into the child's stdin. Dropping it detaches: the command keeps
    /// running, and its timeout clock — stopped while somebody was there to
    /// answer it — starts again.
    pub(super) writer: ConsoleWriter,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/provider` → add offers a backend exactly when its plugin is
    /// compiled in.
    ///
    /// The bug this pins is the one the whole picker was rewritten for: a
    /// build without `provider-anthropic` used to draw an "Anthropic (Claude)
    /// — API key" row, take a pasted key, store it in
    /// `~/.wizard/credentials.toml`, write the provider into `config.toml`,
    /// and only then fail — at the first turn, with the key already on disk.
    ///
    /// One row per feature rather than a count, because the interesting
    /// builds are the leave-one-out ones
    /// (`contrib/check-provider-plugins.sh`) and a count agrees with itself
    /// on all of them.
    #[test]
    fn the_add_provider_menu_offers_a_backend_exactly_when_its_plugin_is_compiled_in() {
        let offered: Vec<ProviderKind> = provider_types()
            .iter()
            .map(|row| row.kind.clone())
            .collect();
        for (compiled_in, kind) in [
            (
                cfg!(feature = "provider-anthropic"),
                ProviderKind::ANTHROPIC,
            ),
            (
                cfg!(feature = "provider-cloudflare"),
                ProviderKind::CLOUDFLARE,
            ),
            (cfg!(feature = "provider-openai"), ProviderKind::OPENAI),
            (cfg!(feature = "provider-openai"), ProviderKind::OPENROUTER),
            (cfg!(feature = "provider-xai"), ProviderKind::XAI),
            (cfg!(feature = "provider-xai"), ProviderKind::XAI_OAUTH),
        ] {
            assert_eq!(
                offered.contains(&kind),
                compiled_in,
                "`{kind}` in the add-provider menu"
            );
        }
    }

    /// Dispatch follows the row, not the row's position.
    ///
    /// The old handler was a `match picker.selected` against a fixed array,
    /// which a filtered menu silently breaks: drop the two xAI rows and
    /// "OpenRouter" moves to index 0, where the handler starts an xAI OAuth
    /// sign-in. Nothing about that fails to compile, and on the screen it
    /// looks like the wrong provider was clicked.
    #[test]
    fn every_offered_row_carries_its_own_setup() {
        let rows = provider_types();
        let setups: Vec<ProviderSetup> = rows.iter().map(|row| row.setup).collect();
        let mut seen = setups.clone();
        seen.sort_by_key(|setup| format!("{setup:?}"));
        seen.dedup();
        assert_eq!(seen.len(), setups.len(), "two rows share a setup");

        // And a row's setup agrees with the kind it is gated on, which is what
        // makes the gate mean anything.
        for row in &rows {
            let expected = match row.setup {
                ProviderSetup::XaiSignIn => ProviderKind::XAI_OAUTH,
                ProviderSetup::XaiKey => ProviderKind::XAI,
                ProviderSetup::OpenRouter => ProviderKind::OPENROUTER,
                ProviderSetup::Cloudflare => ProviderKind::CLOUDFLARE,
                ProviderSetup::OpenAi | ProviderSetup::Custom | ProviderSetup::Compat(_) => {
                    ProviderKind::OPENAI
                }
                ProviderSetup::Anthropic => ProviderKind::ANTHROPIC,
            };
            assert_eq!(row.kind, expected, "{}", row.label);
        }
    }

    /// A stock build's menu is the one it has always been, in the order it
    /// was written: the seven fixed rows, then the compat presets.
    #[test]
    #[cfg(all(
        feature = "provider-anthropic",
        feature = "provider-cloudflare",
        feature = "provider-openai",
        feature = "provider-xai",
    ))]
    fn a_stock_build_offers_the_add_provider_menu_it_always_did() {
        let labels: Vec<String> = provider_types().into_iter().map(|row| row.label).collect();
        let fixed = [
            "xAI (Grok) — sign in",
            "xAI (Grok) — API key",
            "OpenRouter — API key",
            "Cloudflare Workers AI — API token",
            "OpenAI — API key",
            "Anthropic (Claude) — API key",
            "OpenAI-compatible — custom",
        ];
        assert_eq!(&labels[..fixed.len()], &fixed);
        assert_eq!(
            labels.len(),
            fixed.len() + crate::llm::compat::PRESETS.len()
        );
        for (row, preset) in labels[fixed.len()..]
            .iter()
            .zip(crate::llm::compat::PRESETS.iter())
        {
            assert_eq!(row, &format!("{} — API key", preset.label));
        }
    }
}
