//! `v2/to.bund` — LLM-based English → Bund translator.
//!
//! Single-node JSON-RPC endpoint that hands a natural-language
//! request to the default LLM provider, validates the returned
//! script through `bund_language_parser::bund_parse`, and returns
//! the resulting Bund source.  Parse failures trigger up to
//! `llm.to_bund.max_retries` follow-up turns where the parse error
//! is fed back to the model.
//!
//! Trust model: same as the other unauthenticated v2 RPCs — the
//! caller authentication is the firewall in front of the bdsnode
//! RPC port, not an `_hmac` field per call.  The endpoint does not
//! execute the generated script; consumers decide whether to run
//! it.

use super::params::rpc_err;
use bdslib::bund_policy;
use bdslib::llm::to_bund::{self, translate, translation_to_json};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

pub fn register(module: &mut RpcModule<()>) {
    register_translate(module);
    register_settings(module);
}

#[derive(Deserialize)]
struct ToBundParams {
    /// The English request to translate.  Trimmed before dispatch.
    message: String,
    /// Optional provider override (`""` or omitted → use the
    /// manager's default).
    #[serde(default)]
    provider: Option<String>,
    /// Optional model override (`""` or omitted → use the
    /// provider's default).
    #[serde(default)]
    model: Option<String>,
    /// Optional per-call retry ceiling (capped to 5).
    #[serde(default)]
    max_retries: Option<u64>,
    /// Optional [`CompletionOpts`] passthrough block (temperature,
    /// max_tokens, top_p, seed, num_ctx, stop[]).
    #[serde(default)]
    options: Option<JsonValue>,
}

fn register_translate(module: &mut RpcModule<()>) {
    module.register_async_method("v2/to.bund", |params, _ctx, _| async move {
        let p: ToBundParams = params.parse()?;

        // Build the req_extra JSON that translate() consumes.
        // Mirrors the wire fields 1:1 so we don't lose info.
        let mut extra = serde_json::Map::new();
        if let Some(s) = p.provider {
            if !s.is_empty() { extra.insert("provider".into(), json!(s)); }
        }
        if let Some(s) = p.model {
            if !s.is_empty() { extra.insert("model".into(),    json!(s)); }
        }
        if let Some(n) = p.max_retries {
            extra.insert("max_retries".into(), json!(n));
        }
        if let Some(o) = p.options {
            extra.insert("options".into(), o);
        }
        let req_extra = JsonValue::Object(extra);
        let message = p.message;

        // Provider .complete() blocks on the network; run it on a
        // pool thread so we don't stall the JSON-RPC reactor.
        let resp = tokio::task::spawn_blocking(move || {
            translate(&message, &req_extra)
        })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?
            .map_err(|e| rpc_err(-32004, e))?;

        Ok::<JsonValue, ErrorObject>(translation_to_json(&resp))
    }).unwrap();
}

fn register_settings(module: &mut RpcModule<()>) {
    module.register_async_method("v2/to.bund.settings", |_params, _ctx, _| async move {
        let s = to_bund::settings();
        // Echo the active sandbox policy so operators / consumers can
        // see what words the translator is being told to avoid.  Empty
        // when no policy is active (the common dev default).
        let disabled_groups: Vec<JsonValue> = bund_policy::effective_disabled_by_category()
            .into_iter()
            .map(|(cat, words)| json!({"category": cat, "words": words}))
            .collect();
        Ok::<JsonValue, ErrorObject>(json!({
            "enabled":                 s.enabled,
            "timeout_secs":            s.timeout_secs,
            "max_retries":             s.max_retries,
            "provider":                s.provider,
            "model":                   s.model,
            "extra_system_prompt_len": s.extra_system_prompt.len(),
            "disabled_groups":         disabled_groups,
        }))
    }).unwrap();
}
