//! LLM provider layer — Phase 0 of the v4/* inference surface.
//!
//! This module owns the *provider abstraction*: one [`providers::Provider`]
//! trait, three async impls (`OllamaProvider`, `AnthropicProvider`,
//! `OpenAIProvider`), and a process-wide [`manager::ProviderManager`] that
//! reads the `llm` block of `bds.hjson` and exposes registered providers
//! by name.  Helpers, RPCs, cache, dedup and job queue land in later
//! phases — see the proposal in conversation history.

pub mod cache;
pub mod chat;
pub mod context;
pub mod manager;
pub mod providers;
pub mod types;

pub use manager::{LlmConfig, ProviderManager};
pub use providers::Provider;
pub use types::{
    Capabilities, CompletionOpts, CompletionRequest, CompletionResponse,
    EmbedRequest, EmbedResponse, Message, Role,
};
