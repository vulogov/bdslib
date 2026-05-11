//! Shared request/response types for every LLM provider.
//!
//! Providers see the same `CompletionRequest` regardless of upstream API
//! shape — the per-provider modules in `src/llm/providers/` translate to
//! and from Ollama / Anthropic / OpenAI JSON.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System    => "system",
            Role::User      => "user",
            Role::Assistant => "assistant",
            Role::Tool      => "tool",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role:    Role,
    pub content: String,
}

impl Message {
    pub fn system   (content: impl Into<String>) -> Self { Self { role: Role::System,    content: content.into() } }
    pub fn user     (content: impl Into<String>) -> Self { Self { role: Role::User,      content: content.into() } }
    pub fn assistant(content: impl Into<String>) -> Self { Self { role: Role::Assistant, content: content.into() } }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionOpts {
    pub temperature: Option<f32>,
    pub max_tokens:  Option<u32>,
    pub top_p:       Option<f32>,
    #[serde(default)]
    pub stop:        Vec<String>,
    pub seed:        Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub model:    String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub options:  CompletionOpts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub text:          String,
    pub model:         String,
    pub finish_reason: Option<String>,
    pub tokens_in:     Option<u32>,
    pub tokens_out:    Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedRequest {
    pub model: String,
    pub texts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedResponse {
    pub model:   String,
    pub vectors: Vec<Vec<f32>>,
    pub dim:     usize,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub chat:  bool,
    pub embed: bool,
}
