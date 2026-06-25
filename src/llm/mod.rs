//! LLM client abstraction.
//!
//! Phase 0 only defines the trait and message type. The real
//! OpenAI-compatible implementation (chat completions, streaming, retry)
//! arrives in a later phase.

/// A single chat message in an OpenAI-style `role`/`content` exchange.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Chat + translation surface used by the UI.
#[allow(dead_code)]
pub trait LlmClient {
    /// Run a chat completion over the given messages and return the assistant
    /// reply.
    fn chat(&self, messages: &[ChatMessage]) -> anyhow::Result<String>;
    /// Translate the given text into Chinese (Simplified).
    fn translate_to_chinese(&self, text: &str) -> anyhow::Result<String>;
}
