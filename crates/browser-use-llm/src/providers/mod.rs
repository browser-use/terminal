//! Ergonomic provider facades.
//!
//! Each facade resolves deployment configuration (API key and base URL) *before*
//! model selection, then produces a fully-resolved [`Route`](crate::route::Route).
//! The shape is a small two-step builder:
//!
//! 1. `configure` (or a named-profile constructor) captures the deployment
//!    config that is shared across many models: credentials and base URL.
//! 2. A model selector (`responses`, `chat`, `model`) binds a protocol + path
//!    and returns a ready [`Route`](crate::route::Route).
//!
//! This ordering reflects reality: deployment config rarely changes, while the
//! protocol and model vary per call.

mod anthropic;
mod google;
mod openai;
mod openai_compatible;

pub use anthropic::{Anthropic, AnthropicConfig};
pub use google::{Google, GoogleConfig};
pub use openai::{OpenAi, OpenAiConfig};
pub use openai_compatible::OpenAiCompatible;
