//! Wire-format protocols (added per WP 1.3–1.5) and the shared stream-decoding
//! utilities they all build on (`utils`).

pub mod anthropic_messages;
pub mod gemini_generate_content;
pub mod openai_chat;
pub mod openai_responses;
pub mod utils;

pub use anthropic_messages::AnthropicMessagesProtocol;
pub use gemini_generate_content::GeminiGenerateContentProtocol;
pub use openai_chat::OpenAiChatProtocol;
pub use openai_responses::OpenAiResponsesProtocol;
