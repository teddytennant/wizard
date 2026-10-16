//! Model catalog + effort mapping for Claude Code, ported from zeron's
//! `packages/harness/src/claude.ts` (which itself mirrors Claude Code's own
//! picker via t3code's catalog).
//!
//! Runtime initialize discovery decides which models are offered and in what
//! order: the CLI lists its recommended default first and the newest models
//! next, so a release shows up at the top of the picker the day the CLI ships
//! it. Curated rows retain their labels, effort ladders, and option sets because
//! the CLI can under-report supported modes. The shared initialize probe also
//! supplies slash commands and is cached by credential and binary context.

use zeron_proto::{Model, ModelOption, ModelOptionChoice, ReasoningLevel};

/// The ultrathink directive rides every user message as a prompt prefix — that
/// is how the mode actually works in Claude Code (a prompt convention, not an
/// effort flag). Applied to the initial prompt AND every steer.
pub(crate) const ULTRATHINK_PREFIX: &str = "Ultrathink:\n";

pub(crate) fn apply_ultrathink(reasoning: Option<ReasoningLevel>, text: &str) -> String {
    if reasoning == Some(ReasoningLevel::Ultrathink)
        && zeron_proto::invocation::leading_command(text).is_none()
    {
        format!("{ULTRATHINK_PREFIX}{text}")
    } else {
        text.to_owned()
    }
}

fn contains_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

/// Models whose CLI accepts `xhigh` natively; elsewhere it clamps to `max`
/// (mirroring Claude Code's own normalization). Substring port of claude.ts's
/// `/fable-5|opus-4-[7-9]|opus-[5-9]|sonnet-[5-9]/`.
pub(crate) fn supports_xhigh(model: &str) -> bool {
    contains_any(
        model,
        &[
            "fable-5", "opus-4-7", "opus-4-8", "opus-4-9", "opus-5", "opus-6", "opus-7", "opus-8",
            "opus-9", "sonnet-5", "sonnet-6", "sonnet-7", "sonnet-8", "sonnet-9",
        ],
    )
}

/// Map the unified level to the `--effort` flag value the CLI accepts for this
/// model. The special modes don't translate directly: `ultrathink` is a prompt
/// prefix (no flag), `ultracode` runs as `xhigh` plus the ultracode setting,
/// and `ultra` is a Codex-only tier (Claude tops out at `max`).
pub(crate) fn to_effort(
    reasoning: Option<ReasoningLevel>,
    model: Option<&str>,
) -> Option<&'static str> {
    let base = match reasoning? {
        ReasoningLevel::Ultrathink => return None,
        ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Ultracode => "xhigh",
        ReasoningLevel::Max | ReasoningLevel::Ultra => "max",
    };
    if base == "xhigh" && !model.is_some_and(supports_xhigh) {
        return Some("max");
    }
    Some(base)
}

/// A boolean toggle rendered as an off/on select (the Rust `ModelOption` wire
/// type has no dedicated boolean kind).
fn toggle(id: &str, label: &str) -> ModelOption {
    ModelOption {
        id: id.into(),
        label: label.into(),
        choices: vec![
            ModelOptionChoice {
                id: "off".into(),
                label: "Off".into(),
            },
            ModelOptionChoice {
                id: "on".into(),
                label: "On".into(),
            },
        ],
        default_choice: "off".into(),
    }
}

/// The 200K/1M context-window select carried by the long-context models. The
/// 1M window is selected via a model-id suffix (`<model>[1m]`), exactly how the
/// CLI itself does it.
pub(crate) fn context_window() -> ModelOption {
    ModelOption {
        id: "contextWindow".into(),
        label: "Context Window".into(),
        choices: vec![
            ModelOptionChoice {
                id: "200k".into(),
                label: "200K".into(),
            },
            ModelOptionChoice {
                id: "1m".into(),
                label: "1M".into(),
            },
        ],
        default_choice: "200k".into(),
    }
}

const FULL_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultracode,
    ReasoningLevel::Ultrathink,
];

/// opus-4-7 / sonnet-5+ tier (claude.ts `claudeEffortsFor`): xhigh native,
/// no ultracode.
const XHIGH_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultrathink,
];

fn model(
    id: &str,
    label: &str,
    description: &str,
    ladder: &[ReasoningLevel],
    options: Vec<ModelOption>,
) -> Model {
    Model {
        id: id.into(),
        label: label.into(),
        description: (!description.is_empty()).then(|| description.into()),
        reasoning_levels: ladder.to_vec(),
        options,
    }
}

/// The curated model list, mirroring claude.ts's `claudeEffortsFor` /
/// `claudeOptionsFor` ladders: full ladder (through ultracode/ultrathink) on
/// Fable 5, `max`-topped ladders on Opus/Sonnet, no efforts but a thinking
/// toggle on Haiku; context-window select on the long-context families and
/// fast mode on Opus 4.5+.
///
/// `pub`: besides the discovery-side enrichment here, the UI's display-side
/// normalization borrows these labels so alias rows served by older engines
/// still read with their version numbers ("Opus 5.5", not "Opus").
pub(crate) fn configured_models() -> Vec<Model> {
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::executable::home_or_current_dir().join(".claude"));
    models_with_settings(&root.join("settings.json"))
}

fn models_with_settings(path: &std::path::Path) -> Vec<Model> {
    let mut models = static_models();
    let settings = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .unwrap_or_default();
    let ids = std::iter::once(settings.get("model")).chain(
        [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        ]
        .into_iter()
        .map(|key| settings.get("env").and_then(|env| env.get(key))),
    );
    for id in ids
        .flatten()
        .filter_map(|v| v.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        if !models.iter().any(|m| m.id == id) {
            models.push(Model {
                id: id.into(),
                label: id.into(),
                description: None,
                reasoning_levels: FULL_LADDER.to_vec(),
                options: vec![],
            });
        }
    }
    models
}

#[cfg(test)]
#[test]
fn settings_models_keep_manifest_and_full_ladder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        serde_json::json!({"model":"gateway/model", "env": {
            "ANTHROPIC_MODEL":"gateway/model", "ANTHROPIC_SMALL_FAST_MODEL":"fast",
            "ANTHROPIC_DEFAULT_OPUS_MODEL":"opus", "ANTHROPIC_DEFAULT_SONNET_MODEL":"sonnet",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL":"haiku"
        }})
        .to_string(),
    )
    .unwrap();
    let models = models_with_settings(&path);
    assert_eq!(&models[..static_models().len()], static_models());
    assert_eq!(models.len(), static_models().len() + 5);
    for model in &models[static_models().len()..] {
        assert_eq!(model.label, model.id);
        assert_eq!(model.reasoning_levels, FULL_LADDER);
    }
    std::fs::write(&path, "invalid").unwrap();
    assert_eq!(models_with_settings(&path), static_models());
}

/// Order the catalog the way the CLI lists it, keeping curated metadata for
/// every model the curated list already knows. Curated and settings rows the
/// CLI did not mention stay selectable, after the ones it did.
pub(super) fn with_discovered_models(
    mut curated: Vec<Model>,
    response: &serde_json::Value,
) -> Result<Vec<Model>, crate::HarnessError> {
    let entries = response
        .get("response")
        .and_then(|v| v.get("models"))
        .and_then(serde_json::Value::as_array)
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| {
            crate::HarnessError::Protocol("Claude returned an empty model catalog".into())
        })?;
    let mut models: Vec<Model> = Vec::new();
    let mut default = None;
    let mut valid = false;
    for entry in entries {
        let text = |key: &str| {
            entry
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let Some(id) = text("resolvedModel").or_else(|| text("value")) else {
            continue;
        };
        // Bare aliases cannot become persisted selections. Prefer resolvedModel.
        if matches!(
            id.strip_suffix("[1m]").unwrap_or(id),
            "default" | "opus" | "sonnet" | "haiku" | "fable"
        ) {
            continue;
        }
        valid = true;
        // The CLI lists some models by dated snapshot ("claude-haiku-4-5-20251001");
        // fold those into the curated undated row instead of duplicating it.
        let id = snapshot_base(id)
            .filter(|base| models.iter().any(|model| model.id == *base))
            .unwrap_or(id);
        if text("value") == Some("default") {
            default = Some(id.to_owned());
        }
        if models.iter().any(|model| model.id == id) {
            continue;
        }
        if let Some(index) = curated.iter().position(|m| same_model(&m.id, id)) {
            models.push(curated.remove(index));
            continue;
        }
        let mut ladder = Vec::new();
        for effort in entry
            .get("supportedEffortLevels")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
        {
            let level = match effort {
                "low" => ReasoningLevel::Low,
                "medium" => ReasoningLevel::Medium,
                "high" => ReasoningLevel::High,
                "xhigh" => ReasoningLevel::XHigh,
                "max" => ReasoningLevel::Max,
                _ => continue,
            };
            if !ladder.contains(&level) {
                ladder.push(level);
            }
        }
        if ladder.contains(&ReasoningLevel::XHigh) {
            ladder.extend([ReasoningLevel::Ultracode, ReasoningLevel::Ultrathink]);
        }
        // Signed out, the CLI names its alias rows by family alone ("Sonnet")
        // and moves the version into the description. The id still has it.
        let label = match text("displayName") {
            Some(name) if name.chars().any(|c| c.is_ascii_digit()) => name.to_owned(),
            name => version_label(id).unwrap_or_else(|| name.unwrap_or(id).to_owned()),
        };
        models.push(Model {
            id: id.into(),
            label,
            description: text("description").map(str::to_owned),
            reasoning_levels: ladder,
            options: vec![],
        });
    }
    models.extend(curated);
    if !valid {
        return Err(crate::HarnessError::Protocol(
            "Claude returned an empty model catalog".into(),
        ));
    }
    if let Some(default) = default {
        // The picker folds long-context duplicates into their curated base.
        // Put that base immediately behind the concrete default so folding
        // keeps the CLI's default family first without changing curated rows.
        if let Some(base) = default.strip_suffix("[1m]")
            && let Some(index) = models.iter().position(|m| m.id == base)
        {
            let model = models.remove(index);
            models.insert(0, model);
        }
        if let Some(index) = models.iter().position(|m| m.id == default) {
            let model = models.remove(index);
            models.insert(0, model);
        }
    }
    Ok(models)
}

/// `claude-haiku-4-5-20251001` -> `claude-haiku-4-5`.
fn snapshot_base(id: &str) -> Option<&str> {
    let (base, date) = id.rsplit_once('-')?;
    (date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit())).then_some(base)
}

/// Whether a curated id and a discovered one name the same model: equal, or
/// the discovered id is the curated one pinned to a snapshot date
/// (`claude-haiku-4-5` and `claude-haiku-4-5-20251001`).
fn same_model(curated: &str, discovered: &str) -> bool {
    curated == discovered
        || discovered.strip_prefix(curated).is_some_and(|rest| {
            rest.len() == 9
                && rest.starts_with('-')
                && rest[1..].chars().all(|c| c.is_ascii_digit())
        })
}

/// "Sonnet 5.5" for `claude-sonnet-5-5`: the family, then the version parts
/// up to a snapshot date or the long-context suffix. `None` for ids that do
/// not follow the `claude-<family>-<version>` shape.
fn version_label(id: &str) -> Option<String> {
    let mut parts = id
        .strip_prefix("claude-")?
        .trim_end_matches("[1m]")
        .split('-');
    let family = parts
        .next()
        .filter(|f| f.chars().all(|c| c.is_ascii_alphabetic()))?;
    let version: Vec<&str> = parts
        .take_while(|p| p.len() < 8 && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .collect();
    if version.is_empty() {
        return None;
    }
    let mut chars = family.chars();
    let first = chars.next()?.to_ascii_uppercase();
    Some(format!("{first}{} {}", chars.as_str(), version.join(".")))
}

/// Newest first within each family, flagship families first — the order the
/// CLI's own picker uses, so the offline fallback reads the same and the UI's
/// alias labels ("sonnet") resolve to the newest row of their family.
pub fn static_models() -> Vec<Model> {
    vec![
        model(
            "claude-opus-5-5",
            "Opus 5.5",
            "Best for everyday, complex tasks",
            FULL_LADDER,
            vec![context_window(), toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-fable-5-1",
            "Fable 5.1",
            "Most intelligent model for building agents",
            FULL_LADDER,
            vec![context_window()],
        ),
        model(
            "claude-sonnet-5-5",
            "Sonnet 5.5",
            "Most efficient for simpler tasks",
            XHIGH_LADDER,
            vec![],
        ),
        model(
            "claude-haiku-4-5",
            "Haiku 4.5",
            "Fastest model for everyday tasks",
            &[],
            vec![toggle("thinking", "Thinking")],
        ),
        model(
            "claude-fable-5",
            "Fable 5",
            "Previous generation Fable",
            FULL_LADDER,
            vec![context_window()],
        ),
        model(
            "claude-opus-4-8",
            "Opus 4.8",
            "Previous generation Opus",
            FULL_LADDER,
            vec![toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-opus-4-7",
            "Opus 4.7",
            "Older generation Opus",
            XHIGH_LADDER,
            vec![toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-sonnet-5",
            "Sonnet 5",
            "Balanced speed and intelligence",
            XHIGH_LADDER,
            vec![context_window()],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(models: &[Model]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn version_labels_come_from_the_id() {
        assert_eq!(
            version_label("claude-sonnet-5-5").as_deref(),
            Some("Sonnet 5.5")
        );
        assert_eq!(version_label("claude-opus-5").as_deref(), Some("Opus 5"));
        assert_eq!(
            version_label("claude-haiku-4-5-20251001").as_deref(),
            Some("Haiku 4.5")
        );
        assert_eq!(
            version_label("claude-fable-5-1[1m]").as_deref(),
            Some("Fable 5.1")
        );
        assert_eq!(version_label("gateway/new"), None);
        assert_eq!(version_label("claude-sonnet"), None);
    }

    #[test]
    fn a_snapshot_date_names_the_same_model() {
        assert!(same_model("claude-haiku-4-5", "claude-haiku-4-5"));
        assert!(same_model("claude-haiku-4-5", "claude-haiku-4-5-20251001"));
        assert!(!same_model("claude-opus-5", "claude-opus-5-5"));
        assert!(!same_model("claude-sonnet-5", "claude-sonnet-5-20"));
    }

    /// What `initialize` answers while signed in (Claude Code 2.1.284).
    #[test]
    fn the_cli_order_wins_and_new_models_are_not_buried() {
        let response = serde_json::json!({"response": {"models": [
            {"value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)"},
            {"value": "opus", "resolvedModel": "claude-opus-5-5", "displayName": "Opus 5.5"},
            {"value": "claude-fable-5-1", "resolvedModel": "claude-fable-5-1", "displayName": "Fable 5.1"},
            {"value": "sonnet", "resolvedModel": "claude-sonnet-6", "displayName": "Sonnet 6",
             "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]},
            {"value": "haiku", "resolvedModel": "claude-haiku-4-5-20251001", "displayName": "Haiku 4.5"},
            {"value": "claude-sonnet-5-5", "resolvedModel": "claude-sonnet-5-5", "displayName": "Sonnet 5.5"},
            {"value": "claude-opus-4-6", "resolvedModel": "claude-opus-4-6", "displayName": "Opus 4.6"}
        ]}});
        let models = with_discovered_models(static_models(), &response).unwrap();
        assert_eq!(
            ids(&models[..6]),
            [
                "claude-opus-5-5",
                "claude-fable-5-1",
                "claude-sonnet-6",
                "claude-haiku-4-5",
                "claude-sonnet-5-5",
                "claude-opus-4-6",
            ]
        );
        assert_eq!(models[2].label, "Sonnet 6");
        // Curated rows keep their metadata wherever the CLI puts them…
        for curated in static_models() {
            assert_eq!(models.iter().find(|m| m.id == curated.id), Some(&curated));
        }
        // …and the dated Haiku folds into the curated one instead of doubling it.
        assert!(!models.iter().any(|m| m.id == "claude-haiku-4-5-20251001"));
        assert_eq!(models.len(), static_models().len() + 2);
    }

    /// Signed out, the alias rows carry the family alone as their name.
    #[test]
    fn a_family_only_name_gets_its_version_back() {
        let response = serde_json::json!({"response": {"models": [
            {"value": "sonnet", "resolvedModel": "claude-sonnet-6", "displayName": "Sonnet",
             "description": "Sonnet 6 · Efficient for routine tasks · $2/$10 per Mtok"}
        ]}});
        let models = with_discovered_models(static_models(), &response).unwrap();
        assert_eq!(models[0].id, "claude-sonnet-6");
        assert_eq!(models[0].label, "Sonnet 6");
    }

    #[test]
    fn effort_maps_special_modes() {
        assert_eq!(to_effort(None, None), None);
        assert_eq!(to_effort(Some(ReasoningLevel::Ultrathink), None), None);
        assert_eq!(
            to_effort(Some(ReasoningLevel::Minimal), Some("claude-fable-5")),
            Some("low")
        );
        assert_eq!(
            to_effort(Some(ReasoningLevel::Ultra), Some("claude-fable-5")),
            Some("max")
        );
        // ultracode -> xhigh where supported…
        assert_eq!(
            to_effort(Some(ReasoningLevel::Ultracode), Some("claude-fable-5")),
            Some("xhigh")
        );
        // …and xhigh clamps to max elsewhere.
        assert_eq!(
            to_effort(Some(ReasoningLevel::XHigh), Some("claude-opus-4-5")),
            Some("max")
        );
        assert_eq!(to_effort(Some(ReasoningLevel::XHigh), None), Some("max"));
    }

    #[test]
    fn dated_snapshots_fold_into_curated_rows() {
        let response = serde_json::json!({"response": {"models": [
            {"value": "claude-haiku-4-5-20251001", "resolvedModel": "claude-haiku-4-5-20251001",
             "displayName": "Haiku 4.5", "description": "Fastest for quick answers"},
            {"value": "claude-haiku-9-9-20990101", "displayName": "Haiku 9.9"},
        ]}});
        let models = with_discovered_models(static_models(), &response).unwrap();
        assert_eq!(models.iter().filter(|m| m.label == "Haiku 4.5").count(), 1);
        // Snapshots with no curated base still surface as-is.
        assert!(models.iter().any(|m| m.id == "claude-haiku-9-9-20990101"));
    }

    #[test]
    fn xhigh_family_matching() {
        assert!(supports_xhigh("claude-fable-5"));
        assert!(supports_xhigh("claude-fable-5-1"));
        assert!(supports_xhigh("claude-opus-5"));
        assert!(supports_xhigh("claude-opus-5-5"));
        assert!(supports_xhigh("claude-opus-5-5[1m]"));
        assert!(supports_xhigh("claude-opus-4-7-20260101"));
        assert!(!supports_xhigh("claude-opus-4-5"));
        assert!(!supports_xhigh("claude-sonnet-4-5"));
    }

    #[test]
    fn ultrathink_preserves_leading_commands_and_arguments() {
        for command in ["/compact", "/review focus on tests"] {
            for prefix in ["", " ", "   ", "\n", "\r\n  "] {
                let text = format!("{prefix}{command}");
                assert_eq!(
                    apply_ultrathink(Some(ReasoningLevel::Ultrathink), &text),
                    text
                );
            }
        }
        for literal in [
            "    /compact",
            "\t/compact",
            "\n    /compact",
            "\u{a0}/compact",
        ] {
            assert_eq!(
                apply_ultrathink(Some(ReasoningLevel::Ultrathink), literal),
                format!("{ULTRATHINK_PREFIX}{literal}")
            );
        }
    }

    #[test]
    fn ultrathink_prefixes_prompt() {
        assert_eq!(
            apply_ultrathink(Some(ReasoningLevel::Ultrathink), "do it"),
            "Ultrathink:\ndo it"
        );
        assert_eq!(
            apply_ultrathink(Some(ReasoningLevel::Max), "do it"),
            "do it"
        );
    }
}
