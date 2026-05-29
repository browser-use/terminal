use browser_use_core::ProviderBackend;
use browser_use_providers::{bundled_model_catalog, ModelCatalog, ModelPresetInfo};
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum AgentBackend {
    Codex,
    Openai,
    Anthropic,
    Openrouter,
    Deepseek,
    Opencode,
    OpencodeGo,
    Fake,
    None,
}

impl AgentBackend {
    pub(crate) fn as_setting(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Openrouter => "openrouter",
            Self::Deepseek => "deepseek",
            Self::Opencode => "opencode",
            Self::OpencodeGo => "opencode-go",
            Self::Fake => "fake",
            Self::None => "none",
        }
    }

    pub(crate) fn from_setting(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "openai" => Some(Self::Openai),
            "anthropic" => Some(Self::Anthropic),
            "openrouter" => Some(Self::Openrouter),
            "deepseek" => Some(Self::Deepseek),
            "opencode" => Some(Self::Opencode),
            "opencode-go" => Some(Self::OpencodeGo),
            "fake" => Some(Self::Fake),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

impl From<AgentBackend> for ProviderBackend {
    fn from(value: AgentBackend) -> Self {
        match value {
            AgentBackend::Codex => Self::Codex,
            AgentBackend::Openai => Self::Openai,
            AgentBackend::Anthropic => Self::Anthropic,
            AgentBackend::Openrouter => Self::Openrouter,
            AgentBackend::Deepseek => Self::Deepseek,
            AgentBackend::Opencode => Self::Opencode,
            AgentBackend::OpencodeGo => Self::OpencodeGo,
            AgentBackend::Fake => Self::Fake,
            AgentBackend::None => Self::None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModelChoiceGroup {
    Recommended,
    BringYourOwnKey,
    OpenCode,
    OpenCodeGo,
    OpenRouter,
    Deepseek,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    pub(crate) display: String,
    pub(crate) account: &'static str,
    pub(crate) backend: AgentBackend,
    pub(crate) provider_model: String,
    pub(crate) descriptor: String,
    pub(crate) group: ModelChoiceGroup,
}

pub(crate) const ACCOUNT_CODEX: &str = "Codex login";
pub(crate) const ACCOUNT_CLAUDE_CODE: &str = "Claude Code subscription";
pub(crate) const ACCOUNT_CLAUDE_CODE_LEGACY: &str = "Claude Code login";
pub(crate) const ACCOUNT_OPENAI: &str = "OpenAI API key";
pub(crate) const ACCOUNT_ANTHROPIC: &str = "Anthropic API key";
pub(crate) const ACCOUNT_OPENROUTER: &str = "OpenRouter API key";
pub(crate) const ACCOUNT_DEEPSEEK: &str = "DeepSeek API key";
pub(crate) const ACCOUNT_OPENCODE: &str = "OpenCode Zen API key";
pub(crate) const ACCOUNT_OPENCODE_GO: &str = "OpenCode Go API key";

pub(crate) const ACCOUNT_CHOICES: [&str; 7] = [
    ACCOUNT_CODEX,
    ACCOUNT_OPENAI,
    ACCOUNT_ANTHROPIC,
    ACCOUNT_OPENROUTER,
    ACCOUNT_DEEPSEEK,
    ACCOUNT_OPENCODE_GO,
    ACCOUNT_OPENCODE,
];

pub(crate) const BROWSER_USE_CLOUD: &str = "Browser Use cloud";
pub(crate) const BROWSER_USE_CLOUD_API_KEY_SETTING: &str = "auth.browser_use_cloud.api_key";
pub(crate) const BROWSER_USE_CLOUD_API_KEY_ENV: &str = "BROWSER_USE_API_KEY";
pub(crate) const BROWSER_LOCAL_CHROME: &str = "Local Chrome";
pub(crate) const BROWSER_CHOICES: [&str; 3] =
    [BROWSER_LOCAL_CHROME, BROWSER_USE_CLOUD, "Headless Chromium"];

pub(crate) fn browser_use_cloud_env_key_present() -> bool {
    std::env::var(BROWSER_USE_CLOUD_API_KEY_ENV).is_ok_and(|value| !value.trim().is_empty())
}

pub(crate) fn is_claude_code_account(account: &str) -> bool {
    account == ACCOUNT_CLAUDE_CODE || account == ACCOUNT_CLAUDE_CODE_LEGACY
}

pub(crate) fn model_choices_for_catalog(catalog: &ModelCatalog) -> Vec<ModelChoice> {
    let mut choices = Vec::new();
    let chatgpt_presets = catalog.presets(true);
    choices.extend(
        chatgpt_presets
            .iter()
            .filter(|preset| preset.show_in_picker)
            .map(|preset| {
                preset_choice(
                    preset,
                    ACCOUNT_CODEX,
                    AgentBackend::Codex,
                    ModelChoiceGroup::Recommended,
                )
            }),
    );
    let api_presets = catalog.presets(false);
    choices.extend(
        api_presets
            .iter()
            .filter(|preset| preset.show_in_picker)
            .map(|preset| {
                preset_choice(
                    preset,
                    ACCOUNT_OPENAI,
                    AgentBackend::Openai,
                    ModelChoiceGroup::BringYourOwnKey,
                )
            }),
    );
    choices.extend(static_external_model_choices());
    if choices.is_empty() {
        return model_choices_for_catalog(&bundled_model_catalog());
    }

    let group_priority = |g: &ModelChoiceGroup| match g {
        ModelChoiceGroup::Recommended => 0,
        ModelChoiceGroup::BringYourOwnKey => 1,
        ModelChoiceGroup::OpenCodeGo => 2,
        ModelChoiceGroup::OpenCode => 3,
        ModelChoiceGroup::OpenRouter => 4,
        ModelChoiceGroup::Deepseek => 5,
    };
    choices.sort_by_key(|choice| group_priority(&choice.group));

    choices
}

pub(crate) fn fallback_model_choices() -> Vec<ModelChoice> {
    model_choices_for_catalog(&bundled_model_catalog())
}

fn preset_choice(
    preset: &ModelPresetInfo,
    account: &'static str,
    backend: AgentBackend,
    group: ModelChoiceGroup,
) -> ModelChoice {
    let descriptor = if account == ACCOUNT_CODEX && preset.is_default {
        "best default".to_string()
    } else if preset.description.trim().is_empty() {
        if account == ACCOUNT_CODEX {
            "available".to_string()
        } else {
            "needs key".to_string()
        }
    } else {
        preset.description.trim().to_string()
    };
    ModelChoice {
        display: preset.display_name.clone(),
        account,
        backend,
        provider_model: preset.id.clone(),
        descriptor,
        group,
    }
}

fn static_external_model_choices() -> Vec<ModelChoice> {
    vec![
        // --- Go Plan models ---
        ModelChoice {
            display: "Go GLM-5".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "glm-5".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go GLM-5.1".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "glm-5.1".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go Kimi K2.5".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "kimi-k2.5".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go Kimi K2.6".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "kimi-k2.6".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go MiMo-V2.5".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "mimo-v2.5".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go MiMo-V2.5-Pro".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "mimo-v2.5-pro".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go MiniMax M2.5".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "minimax-m2.5".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go MiniMax M2.7".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "minimax-m2.7".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go Qwen3.6 Plus".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "qwen3.6-plus".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go Qwen3.7 Max".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "qwen3.7-max".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go DeepSeek V4 Pro".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "deepseek-v4-pro".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        ModelChoice {
            display: "Go DeepSeek V4 Flash".to_string(),
            account: ACCOUNT_OPENCODE_GO,
            backend: AgentBackend::OpencodeGo,
            provider_model: "deepseek-v4-flash".to_string(),
            descriptor: "Go plan".to_string(),
            group: ModelChoiceGroup::OpenCodeGo,
        },
        // --- Zen Plan Free models ---
        ModelChoice {
            display: "Zen Big Pickle".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "big-pickle".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen DeepSeek V4 Flash Free".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "deepseek-v4-flash-free".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen MiMo-V2.5 Free".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "mimo-v2.5-free".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Nemotron 3 Super Free".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "nemotron-3-super-free".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        // --- Zen Plan Paid models ---
        // OpenAI (GPT)
        ModelChoice {
            display: "Zen GPT 5.5".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5.5".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5.5 Pro".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5.5-pro".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5.4".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5.4".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5.4 Pro".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5.4-pro".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5.4 Mini".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5.4-mini".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5.4 Nano".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5.4-nano".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5.3 Codex".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5.3-codex".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5.3 Codex Spark".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5.3-codex-spark".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5.2".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5.2".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5.1".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5.1".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GPT 5 Nano".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gpt-5-nano".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        // Anthropic (Claude)
        ModelChoice {
            display: "Zen Claude Opus 4.8".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "claude-opus-4-8".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Claude Opus 4.7".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "claude-opus-4-7".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Claude Opus 4.6".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "claude-opus-4-6".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Claude Opus 4.5".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "claude-opus-4-5".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Claude Opus 4.1".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "claude-opus-4-1".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Claude Sonnet 4.6".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "claude-sonnet-4-6".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Claude Sonnet 4.5".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "claude-sonnet-4-5".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Claude Haiku 4.5".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "claude-haiku-4-5".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        // Google (Gemini)
        ModelChoice {
            display: "Zen Gemini 3.5 Flash".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gemini-3.5-flash".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Gemini 3.1 Pro".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gemini-3.1-pro".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Gemini 3 Flash".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "gemini-3-flash".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        // Other Providers
        ModelChoice {
            display: "Zen Qwen3.6 Plus".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "qwen3.6-plus".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Qwen3.5 Plus".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "qwen3.5-plus".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen MiniMax M2.7".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "minimax-m2.7".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen MiniMax M2.5".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "minimax-m2.5".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GLM 5.1".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "glm-5.1".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen GLM 5".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "glm-5".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Kimi K2.5".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "kimi-k2.5".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Kimi K2.6".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "kimi-k2.6".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        ModelChoice {
            display: "Zen Grok Build 0.1".to_string(),
            account: ACCOUNT_OPENCODE,
            backend: AgentBackend::Opencode,
            provider_model: "grok-build-0.1".to_string(),
            descriptor: "Zen gateway".to_string(),
            group: ModelChoiceGroup::OpenCode,
        },
        // --- BYOK models ---
        ModelChoice {
            display: "Claude Sonnet 4.6".to_string(),
            account: ACCOUNT_ANTHROPIC,
            backend: AgentBackend::Anthropic,
            provider_model: "claude-sonnet-4-6".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::BringYourOwnKey,
        },
        ModelChoice {
            display: "Claude Opus 4.7".to_string(),
            account: ACCOUNT_ANTHROPIC,
            backend: AgentBackend::Anthropic,
            provider_model: "claude-opus-4-7".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::BringYourOwnKey,
        },
        ModelChoice {
            display: "Qwen3.6 Plus".to_string(),
            account: ACCOUNT_OPENROUTER,
            backend: AgentBackend::Openrouter,
            provider_model: "qwen/qwen3.6-plus".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::OpenRouter,
        },
        ModelChoice {
            display: "Kimi K2.5".to_string(),
            account: ACCOUNT_OPENROUTER,
            backend: AgentBackend::Openrouter,
            provider_model: "moonshotai/kimi-k2.5".to_string(),
            descriptor: "vision + tools".to_string(),
            group: ModelChoiceGroup::OpenRouter,
        },
        ModelChoice {
            display: "DeepSeek V4 Pro".to_string(),
            account: ACCOUNT_OPENROUTER,
            backend: AgentBackend::Openrouter,
            provider_model: "deepseek/deepseek-v4-pro".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::OpenRouter,
        },
        ModelChoice {
            display: "DeepSeek V4 Pro".to_string(),
            account: ACCOUNT_DEEPSEEK,
            backend: AgentBackend::Deepseek,
            provider_model: "deepseek-v4-pro".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::Deepseek,
        },
    ]
}

pub(crate) fn provider_model_for_display(display: &str, choices: &[ModelChoice]) -> String {
    choices
        .iter()
        .find(|choice| choice.display == display)
        .map(|choice| choice.provider_model.clone())
        .unwrap_or_else(|| display.to_string())
}

pub(crate) fn display_model_for_provider_model(model: &str, choices: &[ModelChoice]) -> String {
    choices
        .iter()
        .find(|choice| choice.provider_model == model)
        .map(|choice| choice.display.clone())
        .unwrap_or_else(|| model.to_string())
}

pub(crate) fn display_and_provider_model_for_input(
    input: &str,
    choices: &[ModelChoice],
) -> (String, String) {
    if let Some(choice) = choices
        .iter()
        .find(|choice| choice.display == input || choice.provider_model == input)
    {
        return (choice.display.clone(), choice.provider_model.clone());
    }
    (input.to_string(), input.to_string())
}
