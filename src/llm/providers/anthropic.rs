//! Anthropic Messages API provider.
//!
//! Endpoint: `https://api.anthropic.com/v1/messages`
//! Auth:     `x-api-key: <key>`, `anthropic-version: 2023-06-01`
//! Docs:     https://docs.anthropic.com/en/api/messages
//!
//! Anthropic has no native embeddings endpoint; `embed` falls through to
//! the trait default (returns "not supported").

use crate::common::error::{err_msg, Result};
use crate::llm::providers::Provider;
use crate::llm::types::{
    Capabilities, CompletionRequest, CompletionResponse, Role,
};
use async_trait::async_trait;
use serde_json::{json, Value as JsonValue};
use std::time::Duration;

const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicProvider {
    base_url:      String,
    api_key:       String,
    default_model: String,
    http:          reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(base_url: &str, api_key: &str, default_model: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| err_msg(format!("anthropic: reqwest client build: {e}")))?;
        Ok(Self {
            base_url:      base_url.trim_end_matches('/').to_owned(),
            api_key:       api_key.to_owned(),
            default_model: default_model.to_owned(),
            http,
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str { "anthropic" }
    fn default_model(&self) -> &str { &self.default_model }
    fn capabilities(&self) -> Capabilities {
        Capabilities { chat: true, embed: false }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        // Anthropic separates the system prompt onto its own top-level field;
        // every other message must be user/assistant in alternating order.
        let mut system: Option<String> = None;
        let mut messages: Vec<JsonValue> = Vec::with_capacity(req.messages.len());
        for m in &req.messages {
            match m.role {
                Role::System => {
                    system = Some(match system {
                        Some(prev) => format!("{prev}\n\n{}", m.content),
                        None       => m.content.clone(),
                    });
                }
                Role::User | Role::Assistant => {
                    messages.push(json!({"role": m.role.as_str(), "content": m.content}));
                }
                Role::Tool => {
                    return Err(err_msg("anthropic: tool messages not yet supported"));
                }
            }
        }
        if messages.is_empty() {
            return Err(err_msg("anthropic: completion request has no user/assistant messages"));
        }

        let mut body = serde_json::Map::new();
        body.insert("model".into(),     json!(req.model));
        body.insert("max_tokens".into(), json!(req.options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)));
        body.insert("messages".into(),  JsonValue::Array(messages));
        if let Some(sys) = system { body.insert("system".into(), json!(sys)); }
        if let Some(t) = req.options.temperature { body.insert("temperature".into(), json!(t)); }
        if let Some(p) = req.options.top_p       { body.insert("top_p".into(),       json!(p)); }
        if !req.options.stop.is_empty()          { body.insert("stop_sequences".into(), json!(req.options.stop)); }
        // Anthropic doesn't accept a `seed` field — silently dropped.

        let body_str = serde_json::to_string(&JsonValue::Object(body))
            .map_err(|e| err_msg(format!("anthropic: serialize body: {e}")))?;
        let resp = self.http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .body(body_str)
            .send()
            .await
            .map_err(|e| err_msg(format!("anthropic: HTTP send: {e}")))?;
        let status = resp.status();
        let text = resp.text().await
            .map_err(|e| err_msg(format!("anthropic: read body: {e}")))?;
        if !status.is_success() {
            return Err(err_msg(format!("anthropic: HTTP {status}: {text}")));
        }
        let json: JsonValue = serde_json::from_str(&text)
            .map_err(|e| err_msg(format!("anthropic: parse response: {e}: body={text}")))?;

        // content is an array of blocks; concatenate every type=="text".
        let blocks = json.get("content")
            .and_then(|v| v.as_array())
            .ok_or_else(|| err_msg(format!("anthropic: response missing content array: {text}")))?;
        let mut out = String::new();
        for b in blocks {
            if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                    if !out.is_empty() { out.push('\n'); }
                    out.push_str(t);
                }
            }
        }
        Ok(CompletionResponse {
            text:          out,
            model:         req.model,
            finish_reason: json.get("stop_reason").and_then(|v| v.as_str()).map(str::to_owned),
            tokens_in:     json.pointer("/usage/input_tokens").and_then(|v| v.as_u64()).map(|n| n as u32),
            tokens_out:    json.pointer("/usage/output_tokens").and_then(|v| v.as_u64()).map(|n| n as u32),
        })
    }
}
