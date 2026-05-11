//! OpenAI Chat Completions + Embeddings provider.
//!
//! Endpoints:
//!   chat:  `https://api.openai.com/v1/chat/completions`
//!   embed: `https://api.openai.com/v1/embeddings`
//! Auth: `Authorization: Bearer <api-key>`
//! Docs: https://platform.openai.com/docs/api-reference

use crate::common::error::{err_msg, Result};
use crate::llm::providers::Provider;
use crate::llm::types::{
    Capabilities, CompletionRequest, CompletionResponse, EmbedRequest, EmbedResponse,
};
use async_trait::async_trait;
use serde_json::{json, Value as JsonValue};
use std::time::Duration;

pub struct OpenAIProvider {
    base_url:      String,
    api_key:       String,
    default_model: String,
    http:          reqwest::Client,
}

impl OpenAIProvider {
    pub fn new(base_url: &str, api_key: &str, default_model: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| err_msg(format!("openai: reqwest client build: {e}")))?;
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
impl Provider for OpenAIProvider {
    fn id(&self) -> &str { "openai" }
    fn default_model(&self) -> &str { &self.default_model }
    fn capabilities(&self) -> Capabilities {
        Capabilities { chat: true, embed: true }
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
            .map_err(|e| err_msg(format!("openai: serialize chat body: {e}")))?;
        let resp = self.http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await
            .map_err(|e| err_msg(format!("openai: HTTP send: {e}")))?;
        let status = resp.status();
        let text = resp.text().await
            .map_err(|e| err_msg(format!("openai: read body: {e}")))?;
        if !status.is_success() {
            return Err(err_msg(format!("openai: HTTP {status}: {text}")));
        }
        let json: JsonValue = serde_json::from_str(&text)
            .map_err(|e| err_msg(format!("openai: parse response: {e}: body={text}")))?;

        let choice = json.pointer("/choices/0")
            .ok_or_else(|| err_msg(format!("openai: response missing choices[0]: {text}")))?;
        let content = choice.pointer("/message/content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| err_msg(format!("openai: response missing choices[0].message.content: {text}")))?
            .to_owned();
        Ok(CompletionResponse {
            text:          content,
            model:         json.get("model").and_then(|v| v.as_str()).map(str::to_owned).unwrap_or(req.model),
            finish_reason: choice.get("finish_reason").and_then(|v| v.as_str()).map(str::to_owned),
            tokens_in:     json.pointer("/usage/prompt_tokens").and_then(|v| v.as_u64()).map(|n| n as u32),
            tokens_out:    json.pointer("/usage/completion_tokens").and_then(|v| v.as_u64()).map(|n| n as u32),
        })
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse> {
        if req.texts.is_empty() {
            return Err(err_msg("openai: embed requires at least one input text"));
        }
        let body_str = serde_json::to_string(&json!({"model": req.model, "input": req.texts}))
            .map_err(|e| err_msg(format!("openai: serialize embed body: {e}")))?;
        let resp = self.http
            .post(format!("{}/v1/embeddings", self.base_url))
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await
            .map_err(|e| err_msg(format!("openai: embed HTTP send: {e}")))?;
        let status = resp.status();
        let text = resp.text().await
            .map_err(|e| err_msg(format!("openai: embed read body: {e}")))?;
        if !status.is_success() {
            return Err(err_msg(format!("openai: embed HTTP {status}: {text}")));
        }
        let json: JsonValue = serde_json::from_str(&text)
            .map_err(|e| err_msg(format!("openai: embed parse: {e}: body={text}")))?;

        let arr = json.get("data")
            .and_then(|v| v.as_array())
            .ok_or_else(|| err_msg(format!("openai: embed response missing data: {text}")))?;
        let vectors: Vec<Vec<f32>> = arr.iter().map(|item| {
            item.get("embedding").and_then(|v| v.as_array()).map(|inner| {
                inner.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect::<Vec<f32>>()
            }).unwrap_or_default()
        }).collect();
        let dim = vectors.first().map(|v| v.len()).unwrap_or(0);
        Ok(EmbedResponse {
            model: json.get("model").and_then(|v| v.as_str()).map(str::to_owned).unwrap_or(req.model),
            vectors,
            dim,
        })
    }
}
