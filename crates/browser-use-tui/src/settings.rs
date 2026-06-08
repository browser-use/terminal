use browser_use_agent::config_overrides::ProviderBackend;
use browser_use_agent::history::browser_use_terminal_home_dir;
use browser_use_providers::{bundled_model_catalog, ModelCatalog, ModelPresetInfo};
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum AgentBackend {
    Codex,
    Openai,
    Anthropic,
    Openrouter,
    Deepseek,
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
            AgentBackend::Fake => Self::Fake,
            AgentBackend::None => Self::None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModelChoiceGroup {
    Recommended,
    BringYourOwnKey,
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

/// A starter pick surfaced at the top of the provider screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecommendedModel {
    pub(crate) display: &'static str,
    pub(crate) account: &'static str,
    pub(crate) provider_model: &'static str,
}

pub(crate) const RECOMMENDED_MODELS: &[RecommendedModel] = &[
    RecommendedModel {
        display: "GPT-5.5",
        account: ACCOUNT_CODEX,
        provider_model: "gpt-5.5",
    },
    RecommendedModel {
        display: "Claude Opus 4.8",
        account: ACCOUNT_ANTHROPIC,
        provider_model: "claude-opus-4-8",
    },
    RecommendedModel {
        display: "Gemini 3.1 Pro",
        account: ACCOUNT_OPENROUTER,
        provider_model: "google/gemini-3.1-pro-preview",
    },
];

pub(crate) fn recommended_models_for_codex_availability(
    codex_available: bool,
) -> Vec<RecommendedModel> {
    let mut models = RECOMMENDED_MODELS.to_vec();
    if !codex_available {
        for model in &mut models {
            if model.account == ACCOUNT_CODEX {
                model.account = ACCOUNT_OPENAI;
            }
        }
    }
    models
}

pub(crate) const ACCOUNT_CODEX: &str = "Codex login";
pub(crate) const ACCOUNT_CLAUDE_CODE: &str = "Claude Code subscription";
pub(crate) const ACCOUNT_CLAUDE_CODE_LEGACY: &str = "Claude Code login";
pub(crate) const ACCOUNT_OPENAI: &str = "OpenAI API key";
pub(crate) const ACCOUNT_ANTHROPIC: &str = "Anthropic API key";
pub(crate) const ACCOUNT_OPENROUTER: &str = "OpenRouter API key";
pub(crate) const ACCOUNT_DEEPSEEK: &str = "DeepSeek API key";

pub(crate) const BROWSER_USE_CLOUD: &str = "Browser Use Cloud";
pub(crate) const BROWSER_USE_CLOUD_API_KEY_SETTING: &str = "auth.browser_use_cloud.api_key";
pub(crate) const BROWSER_USE_CLOUD_API_KEY_ENV: &str = "BROWSER_USE_API_KEY";
pub(crate) const AUTH_CHOICES: [&str; 6] = [
    ACCOUNT_CODEX,
    ACCOUNT_OPENAI,
    ACCOUNT_ANTHROPIC,
    ACCOUNT_OPENROUTER,
    ACCOUNT_DEEPSEEK,
    BROWSER_USE_CLOUD,
];
pub(crate) const BROWSER_LOCAL_CHROME: &str = "Local Chrome";

pub(crate) fn browser_use_cloud_env_key_present() -> bool {
    std::env::var(BROWSER_USE_CLOUD_API_KEY_ENV).is_ok_and(|value| !value.trim().is_empty())
}

pub(crate) fn is_claude_code_account(account: &str) -> bool {
    account == ACCOUNT_CLAUDE_CODE || account == ACCOUNT_CLAUDE_CODE_LEGACY
}

/// Catalog (OpenAI/Codex) model slugs to keep out of the picker even when the
/// catalog marks them `visibility: "list"`. Trims the GPT section down to the
/// ones we want to surface (`gpt-5.5`, `gpt-5.4-mini`) without touching the
/// catalog file or the models' capabilities — they remain runnable via config.
const PICKER_HIDDEN_CATALOG_SLUGS: &[&str] = &["gpt-5.4", "gpt-5.3-codex", "gpt-5.2"];

fn preset_hidden_from_picker(preset: &ModelPresetInfo) -> bool {
    PICKER_HIDDEN_CATALOG_SLUGS.contains(&preset.id.as_str())
}

pub(crate) fn model_choices_for_catalog(catalog: &ModelCatalog) -> Vec<ModelChoice> {
    let mut choices = Vec::new();
    let chatgpt_presets = catalog.presets(true);
    choices.extend(
        chatgpt_presets
            .iter()
            .filter(|preset| preset.show_in_picker && !preset_hidden_from_picker(preset))
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
            .filter(|preset| preset.show_in_picker && !preset_hidden_from_picker(preset))
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
    // Keep the stored order identical to the grouped render order
    // (Recommended → BringYourOwnKey → OpenRouter → Deepseek). The picker treats
    // `selected_row` as an index into this vec (navigation clamp, Enter/save),
    // while `render::model_lines` highlights rows by a grouped row counter.
    // Without this stable regroup the two index spaces diverge for interleaved
    // rows (e.g. the BYOK Claude rows that sit between OpenRouter entries),
    // which would highlight one model but save another.
    choices.sort_by_key(|choice| group_render_rank(&choice.group));
    choices
}

/// Rank of a picker group in the order the picker renders its sections, so a
/// stable sort by this key makes a choice's index match its highlighted row.
fn group_render_rank(group: &ModelChoiceGroup) -> u8 {
    match group {
        ModelChoiceGroup::Recommended => 0,
        ModelChoiceGroup::BringYourOwnKey => 1,
        ModelChoiceGroup::OpenRouter => 2,
        ModelChoiceGroup::Deepseek => 3,
    }
}

pub(crate) fn fallback_model_choices() -> Vec<ModelChoice> {
    model_choices_for_catalog(&bundled_model_catalog())
}

/// Build the model picker rows for the active config profile.
///
/// Honors a `config.toml` `model_catalog_json` file pointer: when the active
/// profile's config (under `$BROWSER_USE_TERMINAL_HOME` / `~/.browser-use-terminal`)
/// sets `model_catalog_json = "<path>"`, the referenced JSON is parsed into the
/// providers-crate [`ModelCatalog`] and used to drive the picker. Otherwise (no
/// config, no pointer, or any load/parse failure) falls back to the bundled
/// catalog via [`fallback_model_choices`], leaving default users unaffected.
pub(crate) fn model_choices_for_config(config_profile: Option<&str>) -> Vec<ModelChoice> {
    match load_config_model_catalog(config_profile) {
        Some(catalog) => model_choices_for_catalog(&catalog),
        None => fallback_model_choices(),
    }
}

/// Load the providers-crate [`ModelCatalog`] referenced by the active profile's
/// `config.toml` `model_catalog_json` pointer, if present and parseable.
///
/// The base config lives at `$BROWSER_USE_TERMINAL_HOME/config.toml`
/// (`~/.browser-use-terminal/config.toml`); a named profile reads
/// `<name>.config.toml` from the same directory. The `model_catalog_json`
/// pointer is resolved relative to the config file's directory. Returns `None`
/// when there is no config file, no `model_catalog_json` key, or any read/parse
/// step fails.
fn load_config_model_catalog(config_profile: Option<&str>) -> Option<ModelCatalog> {
    let home = browser_use_terminal_home_dir()?;
    let config_file = match config_profile {
        Some(profile) if !profile.trim().is_empty() => format!("{}.config.toml", profile.trim()),
        _ => "config.toml".to_string(),
    };
    let config_path = home.join(config_file);
    let config_text = std::fs::read_to_string(&config_path).ok()?;
    let parsed = config_text.parse::<toml::Value>().ok()?;
    let pointer = parsed.get("model_catalog_json")?.as_str()?.trim();
    if pointer.is_empty() {
        return None;
    }
    let pointer_path = std::path::Path::new(pointer);
    let resolved = if pointer_path.is_absolute() {
        pointer_path.to_path_buf()
    } else {
        config_path.parent()?.join(pointer_path)
    };
    let json = std::fs::read_to_string(&resolved).ok()?;
    serde_json::from_str::<ModelCatalog>(&json).ok()
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
        ModelChoice {
            display: "GPT-5.5".to_string(),
            account: ACCOUNT_OPENROUTER,
            backend: AgentBackend::Openrouter,
            provider_model: "openai/gpt-5.5".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::OpenRouter,
        },
        ModelChoice {
            display: "Claude Sonnet 4.6".to_string(),
            account: ACCOUNT_ANTHROPIC,
            backend: AgentBackend::Anthropic,
            provider_model: "claude-sonnet-4-6".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::BringYourOwnKey,
        },
        ModelChoice {
            display: "Claude Opus 4.8".to_string(),
            account: ACCOUNT_ANTHROPIC,
            backend: AgentBackend::Anthropic,
            provider_model: "claude-opus-4-8".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::BringYourOwnKey,
        },
        ModelChoice {
            display: "Claude Haiku 4.5".to_string(),
            account: ACCOUNT_ANTHROPIC,
            backend: AgentBackend::Anthropic,
            provider_model: "claude-haiku-4-5".to_string(),
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
            display: "Gemini 3.1 Pro".to_string(),
            account: ACCOUNT_OPENROUTER,
            backend: AgentBackend::Openrouter,
            provider_model: "google/gemini-3.1-pro-preview".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::OpenRouter,
        },
        ModelChoice {
            display: "GLM-5".to_string(),
            account: ACCOUNT_OPENROUTER,
            backend: AgentBackend::Openrouter,
            provider_model: "z-ai/glm-5".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::OpenRouter,
        },
        ModelChoice {
            display: "GLM-4.7".to_string(),
            account: ACCOUNT_OPENROUTER,
            backend: AgentBackend::Openrouter,
            provider_model: "z-ai/glm-4.7".to_string(),
            descriptor: "needs key".to_string(),
            group: ModelChoiceGroup::OpenRouter,
        },
        ModelChoice {
            display: "MiniMax M2.5".to_string(),
            account: ACCOUNT_OPENROUTER,
            backend: AgentBackend::Openrouter,
            provider_model: "minimax/minimax-m2.5".to_string(),
            descriptor: "needs key".to_string(),
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

/// The model rows belonging to a single provider/account, in picker order. Used
/// by the provider-scoped model screen so each provider shows only its models.
pub(crate) fn provider_model_choices<'a>(
    account: &str,
    choices: &'a [ModelChoice],
) -> Vec<&'a ModelChoice> {
    choices
        .iter()
        .filter(|choice| choice.account == account)
        .collect()
}

/// The backend that serves a given provider account.
pub(crate) fn provider_backend_for_account(account: &str) -> AgentBackend {
    if account == ACCOUNT_CODEX {
        AgentBackend::Codex
    } else if account == ACCOUNT_OPENAI {
        AgentBackend::Openai
    } else if account == ACCOUNT_ANTHROPIC {
        AgentBackend::Anthropic
    } else if account == ACCOUNT_DEEPSEEK {
        AgentBackend::Deepseek
    } else {
        AgentBackend::Openrouter
    }
}

/// Build a `ModelChoice` for a (dynamically-fetched or recommended) model id on a
/// given provider account, so it flows through the normal save/persist path.
pub(crate) fn model_choice_for(
    account: &'static str,
    provider_model: &str,
    display: &str,
) -> ModelChoice {
    ModelChoice {
        display: display.to_string(),
        account,
        backend: provider_backend_for_account(account),
        provider_model: provider_model.to_string(),
        descriptor: "needs key".to_string(),
        group: ModelChoiceGroup::Recommended,
    }
}

/// A synthetic choice for a free-text OpenRouter model id typed by the user.
#[allow(dead_code)] // superseded by model_choice_for; retained for a unit test
pub(crate) fn custom_openrouter_choice(model_id: &str) -> ModelChoice {
    ModelChoice {
        display: model_id.to_string(),
        account: ACCOUNT_OPENROUTER,
        backend: AgentBackend::Openrouter,
        provider_model: model_id.to_string(),
        descriptor: "custom".to_string(),
        group: ModelChoiceGroup::OpenRouter,
    }
}

pub(crate) fn bundled_openai_model_ids() -> Vec<String> {
    [
        "gpt-5.5",
        "gpt-5.5-pro",
        "gpt-5.4",
        "gpt-5.4-pro",
        "gpt-5.4-nano",
        "gpt-5.4-mini",
        "gpt-5.3-codex",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

pub(crate) fn bundled_codex_login_model_ids() -> Vec<String> {
    bundled_openai_model_ids()
        .into_iter()
        .filter(|id| {
            !matches!(
                id.as_str(),
                "gpt-5.5-pro" | "gpt-5.4-pro" | "gpt-5.4-nano" | "gpt-5.3-codex"
            )
        })
        .collect()
}

pub(crate) fn bundled_openrouter_model_ids() -> Vec<String> {
    [
        // Anthropic
        "anthropic/claude-sonnet-4.6", // verified
        "anthropic/claude-opus-4.8",
        "anthropic/claude-haiku-4.5",
        // OpenAI
        "openai/gpt-5.5", // verified
        "openai/gpt-5.1",
        // Google
        "google/gemini-3.1-pro-preview", // verified
        "google/gemini-2.5-pro",         // verified
        "google/gemini-3.5-flash",
        // xAI
        "x-ai/grok-4.3", // verified
        "x-ai/grok-4.20",
        // DeepSeek
        "deepseek/deepseek-v4-pro",
        "deepseek/deepseek-v3.2",
        // Qwen
        "qwen/qwen3-max", // verified
        "qwen/qwen3.7-max",
        // Moonshot (Kimi)
        "moonshotai/kimi-k2.5", // verified
        "moonshotai/kimi-k2.6",
        // Z-AI (GLM)
        "z-ai/glm-5", // verified
        "z-ai/glm-5.1",
        "z-ai/glm-4.7",
        // MiniMax
        "minimax/minimax-m2.5",
        "minimax/minimax-m3",
        // Meta (Llama)
        "meta-llama/llama-4-maverick",
        // Mistral
        "mistralai/mistral-large",
        "mistralai/mistral-medium-3.1",
    ]
    .iter()
    .map(|id| id.to_string())
    .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_model_choices_scopes_to_account() {
        let choices = fallback_model_choices();
        let openrouter = provider_model_choices(ACCOUNT_OPENROUTER, &choices);
        assert!(!openrouter.is_empty());
        assert!(openrouter
            .iter()
            .all(|choice| choice.account == ACCOUNT_OPENROUTER));
        // The Codex provider must not leak OpenRouter rows and vice versa.
        let codex = provider_model_choices(ACCOUNT_CODEX, &choices);
        assert!(codex.iter().all(|choice| choice.account == ACCOUNT_CODEX));
    }

    #[test]
    fn custom_openrouter_choice_wraps_raw_id() {
        let choice = custom_openrouter_choice("vendor/some-model");
        assert_eq!(choice.account, ACCOUNT_OPENROUTER);
        assert_eq!(choice.backend, AgentBackend::Openrouter);
        assert_eq!(choice.provider_model, "vendor/some-model");
        assert_eq!(choice.display, "vendor/some-model");
    }

    #[test]
    fn bundled_openrouter_ids_are_the_verified_curated_set() {
        let ids = bundled_openrouter_model_ids();
        // Verified frontier + top open-source picks.
        for id in [
            "anthropic/claude-sonnet-4.6",
            "openai/gpt-5.5",
            "x-ai/grok-4.3",
            "moonshotai/kimi-k2.5",
            "qwen/qwen3-max",
        ] {
            assert!(ids.iter().any(|got| got == id), "missing {id}");
        }
        // Every curated id carries a vendor prefix (vendor/model).
        assert!(ids.iter().all(|id| id.contains('/')));
    }

    #[test]
    fn bundled_openai_ids_are_the_curated_provider_set() {
        assert_eq!(
            bundled_openai_model_ids(),
            vec![
                "gpt-5.5",
                "gpt-5.5-pro",
                "gpt-5.4",
                "gpt-5.4-pro",
                "gpt-5.4-nano",
                "gpt-5.4-mini",
                "gpt-5.3-codex",
            ]
        );
    }

    #[test]
    fn bundled_codex_login_ids_exclude_login_only_unsupported_models() {
        assert_eq!(
            bundled_codex_login_model_ids(),
            vec!["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"]
        );
    }

    #[test]
    fn recommended_models_have_a_matching_provider_row() {
        let choices = fallback_model_choices();
        for rec in RECOMMENDED_MODELS {
            let scoped = provider_model_choices(rec.account, &choices);
            assert!(
                scoped
                    .iter()
                    .any(|choice| choice.provider_model == rec.provider_model),
                "recommended {} has no row under {}",
                rec.provider_model,
                rec.account
            );
        }
    }

    #[test]
    fn recommended_openai_model_uses_api_key_when_codex_is_unavailable() {
        let models = recommended_models_for_codex_availability(false);
        assert_eq!(models[0].display, "GPT-5.5");
        assert_eq!(models[0].account, ACCOUNT_OPENAI);
        assert_eq!(models[0].provider_model, "gpt-5.5");
    }

    #[test]
    fn recommended_openai_model_uses_codex_when_codex_is_available() {
        let models = recommended_models_for_codex_availability(true);
        assert_eq!(models[0].display, "GPT-5.5");
        assert_eq!(models[0].account, ACCOUNT_CODEX);
        assert_eq!(models[0].provider_model, "gpt-5.5");
    }
}
