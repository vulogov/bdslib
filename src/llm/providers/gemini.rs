//! Google Gemini provider.
//!
//! Endpoint: `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent`
//! Auth:     `x-goog-api-key: <key>` header (preferred — keeps the
//!           secret out of URL access logs and proxy buffers).
//! Docs:     https://ai.google.dev/api/generate-content
//!
//! Differences from the other v1beta/HTTP providers we ship:
//!
//! - The model name appears in the URL path (`/models/{model}:generateContent`),
//!   not in the JSON body.
//! - System prompts go in their own top-level `systemInstruction` field
//!   (same shape as Anthropic, different field name).
//! - The chat turn role for the assistant is `"model"`, not `"assistant"`.
//! - Generation knobs sit under a `generationConfig` sub-object
//!   (camelCase: `maxOutputTokens`, `topP`, `stopSequences`).
//! - Response carries `candidates[].content.parts[].text` (similar
//!   block-list shape to Anthropic).
//! - Usage stats are on `usageMetadata.{promptTokenCount,
//!   candidatesTokenCount}`.
//!
//! Embeddings (`text-embedding-004`, `:embedContent`) are NOT wired
//! here yet — the trait default returns "not supported", same as
//! Anthropic.  Adding embedding support is a follow-up on its own.

use crate::common::error::{err_msg, Result};
use crate::llm::providers::Provider;
use crate::llm::types::{
    Capabilities, CompletionRequest, CompletionResponse, Role,
};
use async_trait::async_trait;
use serde_json::{json, Value as JsonValue};
use std::time::Duration;

const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct GeminiProvider {
    base_url:      String,
    api_key:       String,
    default_model: String,
    http:          reqwest::Client,
}

impl GeminiProvider {
    pub fn new(base_url: &str, api_key: &str, default_model: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| err_msg(format!("gemini: reqwest client build: {e}")))?;
        Ok(Self {
            base_url:      base_url.trim_end_matches('/').to_owned(),
            api_key:       api_key.to_owned(),
            default_model: default_model.to_owned(),
            http,
        })
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> &str { "gemini" }
    fn default_model(&self) -> &str { &self.default_model }
    fn capabilities(&self) -> Capabilities {
        // Embeddings ARE supported by Gemini via :embedContent + the
        // text-embedding-004 model, but we don't wire that path here
        // yet — mirror Anthropic's chat-only surface.
        Capabilities { chat: true, embed: false }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse> {
        // Gemini, like Anthropic, separates the system prompt onto its
        // own top-level field; the conversation `contents` array must
        // alternate user / model turns.  Concatenate consecutive
        // system messages with `\n\n`, same convention as Anthropic.
        let mut system: Option<String> = None;
        let mut contents: Vec<JsonValue> = Vec::with_capacity(req.messages.len());
        for m in &req.messages {
            match m.role {
                Role::System => {
                    system = Some(match system {
                        Some(prev) => format!("{prev}\n\n{}", m.content),
                        None       => m.content.clone(),
                    });
                }
                Role::User => {
                    contents.push(json!({
                        "role":  "user",
                        "parts": [{ "text": m.content }],
                    }));
                }
                Role::Assistant => {
                    // The role we send back to Gemini for the
                    // assistant's prior turn is "model" — the Google
                    // API rejects "assistant".
                    contents.push(json!({
                        "role":  "model",
                        "parts": [{ "text": m.content }],
                    }));
                }
                Role::Tool => {
                    return Err(err_msg("gemini: tool messages not yet supported"));
                }
            }
        }
        if contents.is_empty() {
            return Err(err_msg("gemini: completion request has no user/assistant messages"));
        }

        // Build generationConfig from the standard CompletionRequest
        // knobs.  Gemini uses camelCase for these.
        let mut gen_cfg = serde_json::Map::new();
        gen_cfg.insert(
            "maxOutputTokens".into(),
            json!(req.options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        );
        if let Some(t) = req.options.temperature { gen_cfg.insert("temperature".into(), json!(t)); }
        if let Some(p) = req.options.top_p       { gen_cfg.insert("topP".into(),        json!(p)); }
        if !req.options.stop.is_empty()          { gen_cfg.insert("stopSequences".into(), json!(req.options.stop)); }
        // Gemini accepts `seed` under generationConfig as of v1beta
        // (June 2024+); pass through when the caller set it.
        if let Some(s) = req.options.seed        { gen_cfg.insert("seed".into(), json!(s)); }

        let mut body = serde_json::Map::new();
        body.insert("contents".into(), JsonValue::Array(contents));
        body.insert("generationConfig".into(), JsonValue::Object(gen_cfg));
        if let Some(sys) = system {
            body.insert("systemInstruction".into(), json!({ "parts": [{ "text": sys }] }));
        }

        let body_str = serde_json::to_string(&JsonValue::Object(body))
            .map_err(|e| err_msg(format!("gemini: serialize body: {e}")))?;

        // Gemini takes the model name in the URL path, not the body.
        // We use the `x-goog-api-key` header rather than the `?key=`
        // query param so the key never lands in HTTP access logs or
        // proxy buffers.
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, req.model,
        );
        let resp = self.http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .body(body_str)
            .send()
            .await
            .map_err(|e| err_msg(format!("gemini: HTTP send: {e}")))?;
        let status = resp.status();
        let text = resp.text().await
            .map_err(|e| err_msg(format!("gemini: read body: {e}")))?;
        if !status.is_success() {
            return Err(err_msg(format!("gemini: HTTP {status}: {text}")));
        }
        let v: JsonValue = serde_json::from_str(&text)
            .map_err(|e| err_msg(format!("gemini: parse response: {e}: body={text}")))?;

        // Response shape:
        //   candidates: [{ content: { parts: [{ text: "..." }], role: "model" },
        //                  finishReason: "STOP", ... }, ...]
        //   usageMetadata: { promptTokenCount: 50, candidatesTokenCount: 100, ... }
        //
        // We concatenate the text parts from the FIRST candidate.  In
        // practice Gemini returns one candidate unless `candidateCount`
        // is overridden; we don't expose that knob.
        let candidate = v.pointer("/candidates/0")
            .ok_or_else(|| err_msg(format!("gemini: response missing candidates[0]: {text}")))?;

        // Surface safety blocks loudly so operators can tell why the
        // response is empty — silent empties have wasted hours of
        // debugging in the past.
        let finish_reason = candidate.get("finishReason")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if let Some(reason) = &finish_reason {
            if reason == "SAFETY" || reason == "RECITATION" || reason == "BLOCKLIST" {
                return Err(err_msg(format!(
                    "gemini: response blocked by {reason} filter (no text returned). \
                     Full body: {text}"
                )));
            }
        }

        let parts = candidate.pointer("/content/parts")
            .and_then(|v| v.as_array())
            .ok_or_else(|| err_msg(format!(
                "gemini: candidate missing content.parts: {text}"
            )))?;
        let mut out = String::new();
        for p in parts {
            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                if !out.is_empty() { out.push('\n'); }
                out.push_str(t);
            }
        }
        Ok(CompletionResponse {
            text:          out,
            model:         req.model,
            finish_reason,
            tokens_in:     v.pointer("/usageMetadata/promptTokenCount")
                            .and_then(|v| v.as_u64()).map(|n| n as u32),
            tokens_out:    v.pointer("/usageMetadata/candidatesTokenCount")
                            .and_then(|v| v.as_u64()).map(|n| n as u32),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — request-shape pinning.  Network-driven end-to-end coverage
// belongs in an integration test against a fake HTTP server; these
// confine themselves to the serialization invariants.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_trims_trailing_slash_on_base_url() {
        let p = GeminiProvider::new(
            "https://generativelanguage.googleapis.com/",
            "fake-key",
            "gemini-2.5-flash",
        ).unwrap();
        assert_eq!(p.base_url, "https://generativelanguage.googleapis.com");
    }

    #[test]
    fn capabilities_are_chat_only() {
        let p = GeminiProvider::new(
            "https://generativelanguage.googleapis.com",
            "k",
            "gemini-2.5-flash",
        ).unwrap();
        let caps = p.capabilities();
        assert!(caps.chat);
        assert!(!caps.embed);
    }

    #[test]
    fn id_is_gemini() {
        let p = GeminiProvider::new("https://example", "k", "m").unwrap();
        assert_eq!(p.id(), "gemini");
    }

    #[test]
    fn default_model_round_trips() {
        let p = GeminiProvider::new("https://example", "k", "gemini-2.5-flash").unwrap();
        assert_eq!(p.default_model(), "gemini-2.5-flash");
    }
}
