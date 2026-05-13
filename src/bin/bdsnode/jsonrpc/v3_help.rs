//! `v3/help` — docstore-backed LLM Q&A.
//!
//! Takes an English `message`, retrieves the top-`limit` matching
//! documents from the cluster docstore (fully replicated, so a local
//! search covers the whole cluster), optionally filtered to internal-
//! only documents, then asks the default LLM to answer using those
//! documents as RAG context.
//!
//! Unauthenticated v3/* read surface — matches `v3/search` /
//! `v3/aggregationsearch` conventions.  The endpoint does trigger an
//! LLM call (cost), but the trust boundary is the bdsnode RPC port,
//! same as every other v3/* read.
//!
//! Library entry point: [`bdslib::llm::help::help`].

use super::params::rpc_err;
use bdslib::llm::help::{self, HelpRequest, response_to_json};
use bdslib::llm::types::CompletionOpts;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

pub fn register(module: &mut RpcModule<()>) {
    register_help(module);
    register_settings(module);
}

#[derive(Deserialize)]
struct HelpParams {
    /// The English question.  Required and non-empty.
    message: String,

    /// When true, restrict RAG to documents whose metadata carries
    /// `internal_doc: true` (the tag emitted by
    /// `scripts/load_internal_documentation.sh`).  Default: false.
    #[serde(default)]
    internal_only: bool,

    /// Number of documents to include in the prompt.  Clamped
    /// server-side to `[1, llm::help::MAX_LIMIT]`.  Default:
    /// `llm::help::DEFAULT_LIMIT`.
    #[serde(default)]
    limit: Option<usize>,

    /// Optional provider override (`""` / omitted → `llm.default`).
    #[serde(default)]
    provider: Option<String>,

    /// Optional model override (`""` / omitted → provider's
    /// `default_model`).
    #[serde(default)]
    model: Option<String>,

    /// Optional [`CompletionOpts`] passthrough (`temperature`,
    /// `max_tokens`, `top_p`, `seed`, `num_ctx`, `stop[]`).
    #[serde(default)]
    options: Option<JsonValue>,
}

fn register_help(module: &mut RpcModule<()>) {
    module.register_async_method("v3/help", |params, _ctx, _| async move {
        let p: HelpParams = params.parse()?;

        let req = HelpRequest {
            message:       p.message,
            internal_only: p.internal_only,
            limit:         p.limit,
            provider:      p.provider.filter(|s| !s.is_empty()),
            model:         p.model.filter(|s| !s.is_empty()),
            options:       parse_options(p.options.as_ref()),
        };

        // Provider .complete() blocks on the network and docstore
        // search hits DuckDB — both run on a pool thread so we don't
        // stall the JSON-RPC reactor.
        let resp = tokio::task::spawn_blocking(move || help::help(req))
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?
            .map_err(|e| rpc_err(-32004, e))?;

        Ok::<JsonValue, ErrorObject>(response_to_json(&resp))
    }).unwrap();
}

/// `v3/help.settings` — echo the active defaults so operators can
/// confirm the limit ceiling / default and the underlying LLM
/// provider manager state without making a real Q&A call.
fn register_settings(module: &mut RpcModule<()>) {
    module.register_async_method("v3/help.settings", |_params, _ctx, _| async move {
        let mgr = bdslib::llm::manager::manager();
        let default_provider = mgr.and_then(|m| m.default_id().map(str::to_owned))
            .unwrap_or_default();
        let registered: Vec<String> = mgr.map(|m| m.registered()).unwrap_or_default();
        Ok::<JsonValue, ErrorObject>(json!({
            "default_limit":    help::DEFAULT_LIMIT,
            "max_limit":        help::MAX_LIMIT,
            "default_provider": default_provider,
            "providers":        registered,
        }))
    }).unwrap();
}

/// Pull standard sampling knobs out of `options.*` so the JSON
/// passthrough matches the [`CompletionOpts`] field shape.
fn parse_options(opts: Option<&JsonValue>) -> CompletionOpts {
    let mut out = CompletionOpts::default();
    let Some(JsonValue::Object(map)) = opts else { return out; };
    out.temperature = map.get("temperature").and_then(|v| v.as_f64()).map(|f| f as f32);
    out.max_tokens  = map.get("max_tokens").and_then(|v| v.as_u64()).map(|n| n as u32);
    out.top_p       = map.get("top_p").and_then(|v| v.as_f64()).map(|f| f as f32);
    out.seed        = map.get("seed").and_then(|v| v.as_u64());
    out.num_ctx     = map.get("num_ctx").and_then(|v| v.as_u64()).map(|n| n as u32);
    if let Some(arr) = map.get("stop").and_then(|v| v.as_array()) {
        out.stop = arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    }
    out
}
