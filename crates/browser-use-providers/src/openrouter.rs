//! Per-provider model listing: fetch each provider's live `/models` endpoint and
//! cache the result on disk so the TUI model picker can offer a searchable,
//! always-current list — no hardcoded catalog. (Module name is historical; it now
//! covers every provider, not just OpenRouter.)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The on-disk cache is treated as stale after this long; the TUI still shows the
/// stale list immediately and refreshes in the background.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Which provider's model list to fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSource {
    OpenAi,
    Anthropic,
    Google,
    DeepSeek,
    OpenRouter,
    Codex,
}

impl ModelSource {
    /// Stable key used for the per-source cache filename.
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSource::OpenAi => "openai",
            ModelSource::Anthropic => "anthropic",
            ModelSource::Google => "google",
            ModelSource::DeepSeek => "deepseek",
            ModelSource::OpenRouter => "openrouter",
            ModelSource::Codex => "codex",
        }
    }
}

/// The credential to authenticate a `/models` request, if any.
#[derive(Clone, Debug)]
pub enum ProviderCredential {
    None,
    ApiKey(String),
    Oauth {
        access_token: String,
        account_id: String,
    },
}

/// A single model entry, with the capability bits the picker needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Accepts image input (vision). False when unknown.
    #[serde(default)]
    pub vision: bool,
    /// Tool/function-calling support: `Some(true/false)` when known (OpenRouter),
    /// `None` when the provider's `/models` endpoint doesn't report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_tools: Option<bool>,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    fetched_at_ms: u64,
    models: Vec<ProviderModel>,
}

// --- OpenAI-compatible `{ "data": [{ "id", "name"? }] }` shape ---
#[derive(Deserialize)]
struct OpenAiCompatibleResponse {
    data: Vec<OpenAiCompatibleEntry>,
}

#[derive(Deserialize)]
struct OpenAiCompatibleEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    // OpenRouter enriches each entry with capability metadata; plain OpenAI/
    // Anthropic/DeepSeek `/models` omit these (left as None).
    #[serde(default)]
    architecture: Option<Architecture>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Architecture {
    #[serde(default)]
    input_modalities: Vec<String>,
}

// --- Codex `{ "models": [{ "slug", "supported_in_api", "visibility" }] }` shape ---
#[derive(Deserialize)]
struct CodexModelsResponse {
    #[serde(default)]
    models: Vec<CodexModelEntry>,
}

#[derive(Deserialize)]
struct CodexModelEntry {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    supported_in_api: Option<bool>,
    #[serde(default)]
    visibility: Option<String>,
}

// --- Google `{ "models": [{ "name", "baseModelId", "displayName", "supportedGenerationMethods" }] }` shape ---
#[derive(Deserialize)]
struct GoogleModelsResponse {
    #[serde(default)]
    models: Vec<GoogleModelEntry>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct GoogleModelEntry {
    name: String,
    #[serde(default, rename = "baseModelId")]
    base_model_id: Option<String>,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(default, rename = "supportedGenerationMethods")]
    supported_generation_methods: Vec<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn cache_path(dir: &Path, source: ModelSource) -> PathBuf {
    dir.join(format!("models_cache_{}.json", source.as_str()))
}

fn sort_dedup(mut models: Vec<ProviderModel>) -> Vec<ProviderModel> {
    models.retain(|model| !model.id.trim().is_empty());
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    models
}

/// Parse an OpenAI-compatible `/models` body (OpenAI, Anthropic, DeepSeek,
/// OpenRouter all return this shape).
pub fn parse_openai_compatible_models(body: &str) -> Result<Vec<ProviderModel>> {
    let parsed: OpenAiCompatibleResponse =
        serde_json::from_str(body).context("parse /models response")?;
    Ok(sort_dedup(
        parsed
            .data
            .into_iter()
            .map(|entry| {
                let vision = entry
                    .architecture
                    .as_ref()
                    .is_some_and(|arch| arch.input_modalities.iter().any(|m| m == "image"));
                let supports_tools = entry
                    .supported_parameters
                    .as_ref()
                    .map(|params| params.iter().any(|p| p == "tools"));
                ProviderModel {
                    id: entry.id,
                    name: entry.name,
                    vision,
                    supports_tools,
                }
            })
            .collect(),
    ))
}

/// Parse the Codex `/codex/models` body, keeping API-supported, listed models.
pub fn parse_codex_models(body: &str) -> Result<Vec<ProviderModel>> {
    let parsed: CodexModelsResponse =
        serde_json::from_str(body).context("parse Codex /models response")?;
    Ok(sort_dedup(
        parsed
            .models
            .into_iter()
            .filter(|entry| entry.supported_in_api.unwrap_or(true))
            .filter(|entry| entry.visibility.as_deref().unwrap_or("list") != "hide")
            .map(|entry| ProviderModel {
                id: entry.slug,
                name: entry.display_name,
                vision: false,
                supports_tools: None,
            })
            .collect(),
    ))
}

/// Parse the Gemini `models.list` body, keeping models that can generate content.
pub fn parse_google_models(body: &str) -> Result<Vec<ProviderModel>> {
    Ok(parse_google_models_page(body)?.0)
}

fn parse_google_models_page(body: &str) -> Result<(Vec<ProviderModel>, Option<String>)> {
    let parsed: GoogleModelsResponse =
        serde_json::from_str(body).context("parse Google /models response")?;
    let next_page_token = parsed
        .next_page_token
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty());
    Ok((
        sort_dedup(
            parsed
                .models
                .into_iter()
                .filter(|entry| {
                    entry.supported_generation_methods.iter().any(|method| {
                        method == "generateContent" || method == "streamGenerateContent"
                    })
                })
                .map(|entry| {
                    let id = entry
                        .base_model_id
                        .filter(|id| !id.trim().is_empty())
                        .unwrap_or_else(|| {
                            entry
                                .name
                                .strip_prefix("models/")
                                .unwrap_or(&entry.name)
                                .to_string()
                        });
                    ProviderModel {
                        id,
                        name: entry.display_name,
                        vision: false,
                        supports_tools: None,
                    }
                })
                .collect(),
        ),
        next_page_token,
    ))
}

fn models_url(source: ModelSource) -> &'static str {
    match source {
        ModelSource::OpenAi => "https://api.openai.com/v1/models",
        ModelSource::Anthropic => "https://api.anthropic.com/v1/models",
        ModelSource::Google => "https://generativelanguage.googleapis.com/v1beta/models",
        ModelSource::DeepSeek => "https://api.deepseek.com/v1/models",
        ModelSource::OpenRouter => "https://openrouter.ai/api/v1/models",
        ModelSource::Codex => "https://chatgpt.com/backend-api/codex/models?client_version=0.1.0",
    }
}

/// Fetch the live model list for a provider, authenticating per the credential.
pub fn fetch_provider_models(
    source: ModelSource,
    credential: ProviderCredential,
) -> Result<Vec<ProviderModel>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .context("build model-list http client")?;

    if source == ModelSource::Google {
        return fetch_google_provider_models(&client, credential);
    }

    let request = apply_model_list_auth(
        client
            .get(models_url(source))
            .header("accept", "application/json"),
        source,
        &credential,
    );
    let body = request
        .send()
        .with_context(|| format!("request {} /models", source.as_str()))?
        .error_for_status()
        .with_context(|| format!("{} /models returned an error status", source.as_str()))?
        .text()
        .with_context(|| format!("read {} /models body", source.as_str()))?;
    match source {
        ModelSource::Codex => parse_codex_models(&body),
        _ => parse_openai_compatible_models(&body),
    }
}

fn fetch_google_provider_models(
    client: &reqwest::blocking::Client,
    credential: ProviderCredential,
) -> Result<Vec<ProviderModel>> {
    let mut models = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let mut request = client
            .get(models_url(ModelSource::Google))
            .header("accept", "application/json")
            .query(&[("pageSize", "1000")]);
        if let Some(token) = page_token.as_deref() {
            request = request.query(&[("pageToken", token)]);
        }
        let request = apply_model_list_auth(request, ModelSource::Google, &credential);
        let body = request
            .send()
            .context("request google /models")?
            .error_for_status()
            .context("google /models returned an error status")?
            .text()
            .context("read google /models body")?;
        let (mut page_models, next_page_token) = parse_google_models_page(&body)?;
        models.append(&mut page_models);
        let Some(next_page_token) = next_page_token else {
            break;
        };
        page_token = Some(next_page_token);
    }

    Ok(sort_dedup(models))
}

fn apply_model_list_auth(
    mut request: reqwest::blocking::RequestBuilder,
    source: ModelSource,
    credential: &ProviderCredential,
) -> reqwest::blocking::RequestBuilder {
    if source == ModelSource::Anthropic {
        request = request.header("anthropic-version", "2023-06-01");
    }
    match credential {
        ProviderCredential::None => request,
        ProviderCredential::ApiKey(key) if source == ModelSource::Anthropic => {
            request.header("x-api-key", key.trim())
        }
        ProviderCredential::ApiKey(key) if source == ModelSource::Google => {
            request.header("x-goog-api-key", key.trim())
        }
        ProviderCredential::ApiKey(key) => {
            request.header("authorization", format!("Bearer {}", key.trim()))
        }
        ProviderCredential::Oauth {
            access_token,
            account_id,
        } => request
            .header("authorization", format!("Bearer {}", access_token.trim()))
            .header("chatgpt-account-id", account_id.trim()),
    }
}

/// Load cached models for a source. Returns `(models, is_fresh)`.
pub fn load_cached_provider_models(
    dir: &Path,
    source: ModelSource,
) -> Option<(Vec<ProviderModel>, bool)> {
    let text = std::fs::read_to_string(cache_path(dir, source)).ok()?;
    let cache: CacheFile = serde_json::from_str(&text).ok()?;
    let fresh = now_ms().saturating_sub(cache.fetched_at_ms) < CACHE_TTL.as_millis() as u64;
    Some((cache.models, fresh))
}

/// Write a source's models to its on-disk cache, stamping the current time.
pub fn save_cached_provider_models(
    dir: &Path,
    source: ModelSource,
    models: &[ProviderModel],
) -> Result<()> {
    let cache = CacheFile {
        fetched_at_ms: now_ms(),
        models: models.to_vec(),
    };
    let text = serde_json::to_string(&cache).context("serialize model cache")?;
    std::fs::write(cache_path(dir, source), text).context("write model cache")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_compatible_body() {
        let body = r#"{
            "data": [
                {"id": "z-ai/glm-5", "name": "GLM 5"},
                {"id": "minimax/minimax-m2.5"},
                {"id": "  "},
                {"id": "z-ai/glm-5", "name": "GLM 5 dup"}
            ]
        }"#;
        let models = parse_openai_compatible_models(body).expect("parse");
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["minimax/minimax-m2.5", "z-ai/glm-5"]);
        assert_eq!(models[1].name.as_deref(), Some("GLM 5"));
    }

    #[test]
    fn parses_openrouter_vision_and_tool_capabilities() {
        let body = r#"{
            "data": [
                {"id": "vendor/vision-tools", "architecture": {"input_modalities": ["text","image"]}, "supported_parameters": ["tools","reasoning"]},
                {"id": "vendor/text-notools", "architecture": {"input_modalities": ["text"]}, "supported_parameters": ["temperature"]},
                {"id": "plain/no-arch"}
            ]
        }"#;
        let models = parse_openai_compatible_models(body).expect("parse");
        let by = |id: &str| models.iter().find(|m| m.id == id).unwrap().clone();
        let vt = by("vendor/vision-tools");
        assert!(vt.vision);
        assert_eq!(vt.supports_tools, Some(true));
        let tn = by("vendor/text-notools");
        assert!(!tn.vision);
        assert_eq!(tn.supports_tools, Some(false));
        // No architecture/params (plain OpenAI-style entry) → unknown, not false.
        let plain = by("plain/no-arch");
        assert!(!plain.vision);
        assert_eq!(plain.supports_tools, None);
    }

    #[test]
    fn parses_codex_body_filtering_hidden_and_unsupported() {
        let body = r#"{
            "models": [
                {"slug": "gpt-5.5", "supported_in_api": true, "visibility": "list"},
                {"slug": "internal-x", "supported_in_api": true, "visibility": "hide"},
                {"slug": "no-api", "supported_in_api": false, "visibility": "list"}
            ]
        }"#;
        let models = parse_codex_models(body).expect("parse");
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-5.5"]);
    }

    #[test]
    fn parses_google_models() {
        let body = r#"{
            "models": [
                {
                    "name": "models/gemini-3.5-flash-001",
                    "baseModelId": "gemini-3.5-flash",
                    "displayName": "Gemini 3.5 Flash",
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/text-embedding-004",
                    "displayName": "Text Embedding 004",
                    "supportedGenerationMethods": ["embedContent"]
                },
                {
                    "name": "models/gemini-3.1-pro",
                    "displayName": "Gemini 3.1 Pro",
                    "supportedGenerationMethods": ["streamGenerateContent"]
                }
            ]
        }"#;
        let models = parse_google_models(body).expect("parse");
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["gemini-3.1-pro", "gemini-3.5-flash"]);
        assert_eq!(models[1].name.as_deref(), Some("Gemini 3.5 Flash"));
    }

    #[test]
    fn parses_google_models_page_token() {
        let body = r#"{
            "models": [
                {
                    "name": "models/gemini-3.5-flash",
                    "baseModelId": "gemini-3.5-flash",
                    "supportedGenerationMethods": ["generateContent"]
                }
            ],
            "nextPageToken": " next-page "
        }"#;

        let (models, next_page_token) = parse_google_models_page(body).expect("parse");

        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["gemini-3.5-flash"]);
        assert_eq!(next_page_token.as_deref(), Some("next-page"));
    }

    #[test]
    fn per_source_cache_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = vec![ProviderModel {
            id: "gpt-5.5".to_string(),
            name: None,
            vision: false,
            supports_tools: None,
        }];
        save_cached_provider_models(dir.path(), ModelSource::OpenAi, &models).expect("save");
        // Different source has no cache.
        assert!(load_cached_provider_models(dir.path(), ModelSource::Anthropic).is_none());
        let (loaded, fresh) =
            load_cached_provider_models(dir.path(), ModelSource::OpenAi).expect("load");
        assert_eq!(loaded, models);
        assert!(fresh);
    }
}
