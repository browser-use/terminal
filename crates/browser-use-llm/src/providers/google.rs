//! Facade for Google's Gemini API

use crate::protocols::GeminiGenerateContentProtocol;
use crate::route::{Auth, Endpoint, Route};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

#[derive(Debug, Clone)]
pub struct GoogleConfig {
    pub api_key: String,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Google {
    base_url: String,
    api_key: String,
}

impl Google {
    pub fn configure(config: GoogleConfig) -> Self {
        Self {
            base_url: config
                .base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            api_key: config.api_key,
        }
    }

    pub fn model(&self, model: impl Into<String>) -> Route {
        let model = normalize_model(model.into());
        Route::new(
            Box::new(GeminiGenerateContentProtocol::new()),
            Endpoint::new(
                self.base_url.clone(),
                format!("/models/{model}:streamGenerateContent"),
            )
            .with_query("alt", "sse"),
            Auth::header("x-goog-api-key", self.api_key.clone()),
        )
    }
}

fn normalize_model(model: String) -> String {
    model
        .trim()
        .strip_prefix("models/")
        .unwrap_or(model.trim())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(route: &Route, name: &str) -> Option<String> {
        route
            .auth
            .headers()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    #[test]
    fn route_targets_stream_generate_content_with_api_key_header() {
        let provider = Google::configure(GoogleConfig {
            api_key: "g-key".to_string(),
            base_url: None,
        });
        let route = provider.model("gemini-3.5-flash");

        assert_eq!(
            route.endpoint.url(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.5-flash:streamGenerateContent?alt=sse"
        );
        assert_eq!(header(&route, "x-goog-api-key").as_deref(), Some("g-key"));
        assert!(header(&route, "authorization").is_none());
    }

    #[test]
    fn route_accepts_models_prefix_and_base_override() {
        let provider = Google::configure(GoogleConfig {
            api_key: "g-key".to_string(),
            base_url: Some("https://gateway.example.com/google/v1beta".to_string()),
        });
        let route = provider.model("models/gemini-3.1-pro");

        assert_eq!(
            route.endpoint.url(),
            "https://gateway.example.com/google/v1beta/models/gemini-3.1-pro:streamGenerateContent?alt=sse"
        );
    }
}
