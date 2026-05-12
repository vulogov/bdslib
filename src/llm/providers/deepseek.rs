//! DeepSeek provider — chat completions only (no embeddings).
//!
//! DeepSeek's API is OpenAI-compatible at the wire level, so this
//! provider mirrors [`crate::llm::providers::OpenAIProvider`] almost
//! line-for-line, with three differences:
//!
//! 1. `id()` returns `"deepseek"` so the manager can route by name.
//! 2. `capabilities()` reports `embed: false` — DeepSeek does not
//!    ship an embeddings endpoint.
//! 3. The default base URL is `https://api.deepseek.com` and the
//!    default model is `deepseek-chat`.  `deepseek-reasoner` is also
//!    selectable per request.
//!
//! Endpoint:  `https://api.deepseek.com/v1/chat/completions`
//! Auth:      `Authorization: Bearer <api-key>`
//! Docs:      https://api-docs.deepseek.com/
//!
//! Credential resolution is handled by
//! [`crate::llm::manager::ProviderManager::from_config`]: it tries the
//! env var named by `api_key_env` first, then falls back to the
//! plaintext `api_key` field if the env var is unset.  This module
//! does not read env vars or config files itself — the constructor
//! takes the already-resolved key string.

use crate::common::error::{err_msg, Result};
use crate::llm::providers::Provider;
use crate::llm::types::{
    Capabilities, CompletionRequest, CompletionResponse, EmbedRequest, EmbedResponse,
};
use async_trait::async_trait;
use serde_json::{json, Value as JsonValue};
use std::time::Duration;

pub struct DeepSeekProvider {
    base_url:      String,
    api_key:       String,
    default_model: String,
    http:          reqwest::Client,
}

impl DeepSeekProvider {
    pub fn new(base_url: &str, api_key: &str, default_model: &str) -> Result<Self> {
        if api_key.is_empty() {
            return Err(err_msg("deepseek: api_key is empty — set DEEPSEEK_API_KEY env \
                                or `llm.providers.deepseek.api_key` in bds.hjson"));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| err_msg(format!("deepseek: reqwest client build: {e}")))?;
        Ok(Self {
            base_url:      base_url.trim_end_matches('/').to_owned(),
            api_key:       api_key.to_owned(),
            default_model: default_model.to_owned(),
            http,
        })
    }

    fn auth_header(&self) -> String { format!("Bearer {}", self.api_key) }
}

#[async_trait]
impl Provider for DeepSeekProvider {
    fn id(&self) -> &str { "deepseek" }
    fn default_model(&self) -> &str { &self.default_model }
    fn capabilities(&self) -> Capabilities {
        // DeepSeek exposes chat (incl. `deepseek-reasoner`) but no
        // embeddings endpoint.  Callers that need vectors must use
        // `fastembed` (the in-process default) or an external provider.
        Capabilities { chat: true, embed: false }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        let messages: Vec<JsonValue> = req.messages.iter()
            .map(|m| json!({"role": m.role.as_str(), "content": m.content}))
            .collect();

        let mut body = serde_json::Map::new();
        body.insert("model".into(),    json!(req.model));
        body.insert("messages".into(), JsonValue::Array(messages));
        if let Some(t) = req.options.temperature { body.insert("temperature".into(), json!(t)); }
        if let Some(m) = req.options.max_tokens  { body.insert("max_tokens".into(),  json!(m)); }
        if let Some(p) = req.options.top_p       { body.insert("top_p".into(),       json!(p)); }
        if let Some(s) = req.options.seed        { body.insert("seed".into(),        json!(s)); }
        if !req.options.stop.is_empty()          { body.insert("stop".into(),        json!(req.options.stop)); }

        let body_str = serde_json::to_string(&JsonValue::Object(body))
            .map_err(|e| err_msg(format!("deepseek: serialize chat body: {e}")))?;
        let resp = self.http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await
            .map_err(|e| err_msg(format!("deepseek: HTTP send: {e}")))?;
        let status = resp.status();
        let text = resp.text().await
            .map_err(|e| err_msg(format!("deepseek: read body: {e}")))?;
        if !status.is_success() {
            return Err(err_msg(format!("deepseek: HTTP {status}: {text}")));
        }
        let json: JsonValue = serde_json::from_str(&text)
            .map_err(|e| err_msg(format!("deepseek: parse response: {e}: body={text}")))?;

        let choice = json.pointer("/choices/0")
            .ok_or_else(|| err_msg(format!("deepseek: response missing choices[0]: {text}")))?;
        let content = choice.pointer("/message/content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| err_msg(format!(
                "deepseek: response missing choices[0].message.content: {text}")))?
            .to_owned();
        Ok(CompletionResponse {
            text:          content,
            model:         json.get("model").and_then(|v| v.as_str())
                              .map(str::to_owned).unwrap_or(req.model),
            finish_reason: choice.get("finish_reason").and_then(|v| v.as_str())
                              .map(str::to_owned),
            tokens_in:     json.pointer("/usage/prompt_tokens")
                              .and_then(|v| v.as_u64()).map(|n| n as u32),
            tokens_out:    json.pointer("/usage/completion_tokens")
                              .and_then(|v| v.as_u64()).map(|n| n as u32),
        })
    }

    async fn embed(&self, _req: EmbedRequest) -> Result<EmbedResponse> {
        Err(err_msg("deepseek: embeddings not supported — use fastembed or openai"))
    }
}
