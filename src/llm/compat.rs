//! Presets for cloud providers that speak the OpenAI-compatible Chat
//! Completions dialect. One table drives every surface that offers them —
//! onboarding's "More cloud providers" list, the `/provider` add picker, and
//! the GUI settings page — so adding a provider is a single row here.

/// A cloud provider reachable through [`crate::llm::wire::OpenAiProvider`]
/// with nothing but a base URL and an API key.
pub struct CompatPreset {
    /// Provider id: becomes [`ProviderConfig::name`](crate::config::ProviderConfig::name)
    /// and the key name in `~/.wizard/credentials.toml` when a key is pasted
    /// interactively.
    pub name: &'static str,
    /// Menu label.
    pub label: &'static str,
    /// Menu detail line.
    pub detail: &'static str,
    /// OpenAI-compatible endpoint root (no trailing slash).
    pub base_url: &'static str,
    /// Env var the key is read from when none is stored in credentials.
    pub key_env: &'static str,
    /// Model tags offered in pickers; the first is the default.
    pub models: &'static [&'static str],
}

impl CompatPreset {
    /// The default model tag (the first in [`Self::models`]).
    pub fn default_model(&self) -> &'static str {
        self.models.first().copied().unwrap_or_default()
    }
}

pub const PRESETS: &[CompatPreset] = &[
    CompatPreset {
        name: "gemini",
        label: "Google Gemini",
        detail: "gemini-3.5-flash via GEMINI_API_KEY",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_env: "GEMINI_API_KEY",
        models: &[
            "gemini-3.5-flash",
            "gemini-3.1-pro-preview",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
        ],
    },
    CompatPreset {
        name: "deepseek",
        label: "DeepSeek",
        detail: "deepseek-v4 via DEEPSEEK_API_KEY",
        base_url: "https://api.deepseek.com/v1",
        key_env: "DEEPSEEK_API_KEY",
        models: &["deepseek-v4-pro", "deepseek-v4-flash"],
    },
    CompatPreset {
        name: "groq",
        label: "Groq",
        detail: "fast open-model inference via GROQ_API_KEY",
        base_url: "https://api.groq.com/openai/v1",
        key_env: "GROQ_API_KEY",
        models: &[
            "openai/gpt-oss-120b",
            "llama-3.3-70b-versatile",
            "qwen/qwen3.6-27b",
        ],
    },
    CompatPreset {
        name: "mistral",
        label: "Mistral",
        detail: "mistral-medium & devstral via MISTRAL_API_KEY",
        base_url: "https://api.mistral.ai/v1",
        key_env: "MISTRAL_API_KEY",
        models: &[
            "mistral-medium-latest",
            "mistral-large-latest",
            "devstral-2512",
        ],
    },
    CompatPreset {
        name: "moonshot",
        label: "Moonshot AI",
        detail: "Kimi K3 via MOONSHOT_API_KEY",
        base_url: "https://api.moonshot.ai/v1",
        key_env: "MOONSHOT_API_KEY",
        models: &["kimi-k3"],
    },
    CompatPreset {
        name: "zai",
        label: "Z.AI",
        detail: "GLM 5.2 via ZAI_API_KEY",
        base_url: "https://api.z.ai/api/paas/v4",
        key_env: "ZAI_API_KEY",
        models: &["glm-5.2", "glm-5.1", "glm-5"],
    },
    CompatPreset {
        name: "minimax",
        label: "MiniMax",
        detail: "MiniMax M2.7 via MINIMAX_API_KEY",
        base_url: "https://api.minimax.io/v1",
        key_env: "MINIMAX_API_KEY",
        models: &["minimax-m2.7"],
    },
    CompatPreset {
        name: "together",
        label: "Together AI",
        detail: "open models via TOGETHER_API_KEY",
        base_url: "https://api.together.xyz/v1",
        key_env: "TOGETHER_API_KEY",
        models: &["openai/gpt-oss-120b"],
    },
    CompatPreset {
        name: "fireworks",
        label: "Fireworks AI",
        detail: "open models via FIREWORKS_API_KEY",
        base_url: "https://api.fireworks.ai/inference/v1",
        key_env: "FIREWORKS_API_KEY",
        models: &["accounts/fireworks/models/gpt-oss-120b"],
    },
    CompatPreset {
        name: "cerebras",
        label: "Cerebras",
        detail: "ultra-fast inference via CEREBRAS_API_KEY",
        base_url: "https://api.cerebras.ai/v1",
        key_env: "CEREBRAS_API_KEY",
        models: &["gpt-oss-120b"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for preset in PRESETS {
            assert!(
                seen.insert(preset.name),
                "duplicate preset name '{}'",
                preset.name
            );
            assert!(
                !preset.models.is_empty(),
                "'{}' has no model options",
                preset.name
            );
            assert!(
                preset.base_url.starts_with("https://") && !preset.base_url.ends_with('/'),
                "'{}' base_url must be https with no trailing slash",
                preset.name
            );
            assert!(
                preset
                    .key_env
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c == '_'),
                "'{}' key_env is not an UPPER_SNAKE env name",
                preset.name
            );
            assert!(!preset.default_model().is_empty());
        }
    }

    #[test]
    fn preset_names_do_not_collide_with_builtin_provider_ids() {
        // These ids are claimed by dedicated provider kinds; a compat preset
        // reusing one would silently shadow its credentials entry.
        const RESERVED: &[&str] = &[
            "local",
            "openai",
            "claude",
            "xai",
            "openrouter",
            "cloudflare",
            "chatgpt",
        ];
        for preset in PRESETS {
            assert!(
                !RESERVED.contains(&preset.name),
                "preset '{}' collides with a built-in provider id",
                preset.name
            );
        }
    }
}
