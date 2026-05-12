//! Provider trait + per-provider implementations.
//!
//! Every provider exposes the same async surface (`complete`, `embed`);
//! callers route through [`crate::llm::manager::ProviderManager`] and
//! never construct provider objects directly outside this module.

pub mod anthropic;
pub mod deepseek;
pub mod ollama;
pub mod openai;

use crate::common::error::Result;
use crate::llm::types::{Capabilities, CompletionRequest, CompletionResponse, EmbedRequest, EmbedResponse};
use async_trait::async_trait;

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn default_model(&self) -> &str;
    fn capabilities(&self) -> Capabilities;

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse>;

    /// Default returns an unsupported error.  Providers that *do* expose
    /// embeddings override this; callers that need embeddings should check
    /// `capabilities().embed` first.
    async fn embed(&self, _req: EmbedRequest) -> Result<EmbedResponse> {
        Err(crate::common::error::err_msg(format!(
            "provider {:?} does not support embeddings",
            self.id()
        )))
    }
}

pub use anthropic::AnthropicProvider;
pub use deepseek::DeepSeekProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAIProvider;
