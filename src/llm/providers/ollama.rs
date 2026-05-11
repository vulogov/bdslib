//! Ollama provider — wraps `/api/chat` (non-streaming) and `/api/embed`.
//!
//! Docs:
//!   chat:  https://github.com/ollama/ollama/blob/main/docs/api.md#generate-a-chat-completion
//!   embed: https://github.com/ollama/ollama/blob/main/docs/api.md#generate-embeddings

use crate::common::error::{err_msg, Result};
use crate::llm::providers::Provider;
use crate::llm::types::{
    Capabilities, CompletionRequest, CompletionResponse, EmbedRequest, EmbedResponse,
};
use async_trait::async_trait;
use serde_json::{json, Value as JsonValue};
use std::time::Duration;

pub struct OllamaProvider {
    base_url:      String,
    default_model: String,
    http:          reqwest::Client,
}

impl OllamaProvider {
    pub fn new(base_url: &str, default_model: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| err_msg(format!("ollama: reqwest client build: {e}")))?;
        Ok(Self {
            base_url:      base_url.trim_end_matches('/').to_owned(),
            default_model: default_model.to_owned(),
            http,
        })
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn id(&self) -> &str { "ollama" }
    fn default_model(&self) -> &str { &self.default_model }
    fn capabilities(&self) -> Capabilities {
        Capabilities { chat: true, embed: true }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        let mut options = serde_json::Map::new();
        if let Some(t) = req.options.temperature { options.insert("temperature".into(), json!(t)); }
        if let Some(m) = req.options.max_tokens  { options.insert("num_predict".into(), json!(m)); }
        if let Some(p) = req.options.top_p       { options.insert("top_p".into(),       json!(p)); }
        if let Some(s) = req.options.seed        { options.insert("seed".into(),        json!(s)); }
        if !req.options.stop.is_empty()          { options.insert("stop".into(),        json!(req.options.stop)); }

        let messages: Vec<JsonValue> = req.messages.iter()
            .map(|m| json!({"role": m.role.as_str(), "content": m.content}))
            .collect();

        let mut body = serde_json::Map::new();
        body.insert("model".into(),    json!(req.model));
        body.insert("messages".into(), JsonValue::Array(messages));
        body.insert("stream".into(),   json!(false));
        if !options.is_empty() {
            body.insert("options".into(), JsonValue::Object(options));
        }

        let body_str = serde_json::to_string(&JsonValue::Object(body))
            .map_err(|e| err_msg(format!("ollama: serialize chat body: {e}")))?;
        let resp = self.http
            .post(format!("{}/api/chat", self.base_url))
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await
            .map_err(|e| err_msg(format!("ollama: HTTP send: {e}")))?;
        let status = resp.status();
        let text = resp.text().await
            .map_err(|e| err_msg(format!("ollama: read body: {e}")))?;
        if !status.is_success() {
            return Err(err_msg(format!("ollama: HTTP {status}: {text}")));
        }
        let json: JsonValue = serde_json::from_str(&text)
            .map_err(|e| err_msg(format!("ollama: parse response: {e}: body={text}")))?;

        let content = json.get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| err_msg(format!("ollama: response missing message.content: {text}")))?
            .to_owned();

        Ok(CompletionResponse {
            text:          content,
            model:         req.model,
            finish_reason: json.get("done_reason").and_then(|v| v.as_str()).map(str::to_owned),
            tokens_in:     json.get("prompt_eval_count").and_then(|v| v.as_u64()).map(|n| n as u32),
            tokens_out:    json.get("eval_count").and_then(|v| v.as_u64()).map(|n| n as u32),
        })
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse> {
        if req.texts.is_empty() {
            return Err(err_msg("ollama: embed requires at least one input text"));
        }
        let body_str = serde_json::to_string(&json!({"model": req.model, "input": req.texts}))
            .map_err(|e| err_msg(format!("ollama: serialize embed body: {e}")))?;
        let resp = self.http
            .post(format!("{}/api/embed", self.base_url))
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await
            .map_err(|e| err_msg(format!("ollama: embed HTTP send: {e}")))?;
        let status = resp.status();
        let text = resp.text().await
            .map_err(|e| err_msg(format!("ollama: embed read body: {e}")))?;
        if !status.is_success() {
            return Err(err_msg(format!("ollama: embed HTTP {status}: {text}")));
        }
        let json: JsonValue = serde_json::from_str(&text)
            .map_err(|e| err_msg(format!("ollama: embed parse: {e}: body={text}")))?;

        let arr = json.get("embeddings")
            .and_then(|v| v.as_array())
            .ok_or_else(|| err_msg(format!("ollama: embed response missing embeddings: {text}")))?;
        let vectors: Vec<Vec<f32>> = arr.iter().map(|v| {
            v.as_array().map(|inner| inner.iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect::<Vec<f32>>())
            .unwrap_or_default()
        }).collect();
        let dim = vectors.first().map(|v| v.len()).unwrap_or(0);
        Ok(EmbedResponse { model: req.model, vectors, dim })
    }
}
