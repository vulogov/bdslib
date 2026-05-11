//! `v4/llm.*` — Phase 1 of the cluster-aware LLM surface.
//!
//! All methods are HMAC-signed under the same shared-secret scheme as
//! `v3/*`.  The handlers convert verified params into a
//! `rust_dynamic::value::Value`, hand off to `vm::api::llm`, then map
//! the helper's result back to JSON for the response envelope.
//!
//! Methods registered here:
//!
//! | Method                  | Purpose                                       |
//! |-------------------------|-----------------------------------------------|
//! | `v4/llm.complete`       | Single-shot text completion                   |
//! | `v4/llm.embed`          | Vector embeddings for a list of texts         |
//! | `v4/llm.providers.list` | List registered providers + the default       |
//!
//! `v4/llm.chat` and `v4/llm.analyze` land in subsequent commits within
//! Phase 1 (chat) and Phase 2 (analyze).

use super::cluster::authenticate_admin_obj;
use super::params::rpc_err;
use bdslib::vm::api::llm as api;
use bdslib::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

pub fn register(module: &mut RpcModule<()>) {
    register_complete(module);
    register_chat(module);
    register_analyze(module);
    register_embed(module);
    register_providers_list(module);
    register_cache_stats(module);
    register_cache_purge(module);
}

/// Take an unparsed `jsonrpsee::Params`, ensure it's a JSON object,
/// strip + verify the `_hmac` field on a blocking thread, and return
/// the verified inner object.
async fn authenticate(
    params: jsonrpsee::types::Params<'static>,
) -> Result<serde_json::Map<String, JsonValue>, ErrorObject<'static>> {
    let raw: JsonValue = params.parse()
        .map_err(|e| rpc_err(-32602, format!("invalid params: {e}")))?;
    let obj = match raw {
        JsonValue::Object(m) => m,
        _ => return Err(rpc_err(-32602, "params must be a JSON object")),
    };
    tokio::task::spawn_blocking(move || authenticate_admin_obj(obj))
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?
}

/// Run a sync `vm::api::llm` helper on a blocking thread, converting
/// the verified params object to a Value and the helper's return back
/// to JSON for the response.
async fn run_helper<F>(
    verified: serde_json::Map<String, JsonValue>,
    helper:   F,
) -> Result<JsonValue, ErrorObject<'static>>
where
    F: FnOnce(rust_dynamic::value::Value)
        -> Result<rust_dynamic::value::Value, easy_error::Error>
        + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let arg = json_to_dynamic(JsonValue::Object(verified));
        helper(arg)
            .map(dynamic_to_json)
            .map_err(|e| rpc_err(-32004, e))
    })
    .await
    .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?
}

// ── v4/llm.complete ──────────────────────────────────────────────────

fn register_complete(module: &mut RpcModule<()>) {
    module
        .register_async_method("v4/llm.complete", |params, _ctx, _| async move {
            log::debug!("v4/llm.complete: start");
            let verified = authenticate(params).await?;
            let resp = run_helper(verified, api::complete).await?;
            log::debug!("v4/llm.complete: done");
            Ok::<JsonValue, ErrorObject>(resp)
        })
        .unwrap();
}

// ── v4/llm.chat ──────────────────────────────────────────────────────

fn register_chat(module: &mut RpcModule<()>) {
    module
        .register_async_method("v4/llm.chat", |params, _ctx, _| async move {
            log::debug!("v4/llm.chat: start");
            let verified = authenticate(params).await?;
            let resp = run_helper(verified, api::chat).await?;
            log::debug!("v4/llm.chat: done");
            Ok::<JsonValue, ErrorObject>(resp)
        })
        .unwrap();
}

// ── v4/llm.analyze ───────────────────────────────────────────────────

fn register_analyze(module: &mut RpcModule<()>) {
    module
        .register_async_method("v4/llm.analyze", |params, _ctx, _| async move {
            log::debug!("v4/llm.analyze: start");
            let verified = authenticate(params).await?;
            let resp = run_helper(verified, api::analyze).await?;
            log::debug!("v4/llm.analyze: done");
            Ok::<JsonValue, ErrorObject>(resp)
        })
        .unwrap();
}

// ── v4/llm.embed ─────────────────────────────────────────────────────

fn register_embed(module: &mut RpcModule<()>) {
    module
        .register_async_method("v4/llm.embed", |params, _ctx, _| async move {
            log::debug!("v4/llm.embed: start");
            let verified = authenticate(params).await?;
            let resp = run_helper(verified, api::embed).await?;
            log::debug!("v4/llm.embed: done");
            Ok::<JsonValue, ErrorObject>(resp)
        })
        .unwrap();
}

// ── v4/llm.cache.stats ───────────────────────────────────────────────

fn register_cache_stats(module: &mut RpcModule<()>) {
    module
        .register_async_method("v4/llm.cache.stats", |params, _ctx, _| async move {
            log::debug!("v4/llm.cache.stats: start");
            let _verified = authenticate(params).await?;
            let resp = tokio::task::spawn_blocking(|| {
                let mgr = match bdslib::llm::cache::manager() {
                    Some(m) => m,
                    None    => return Ok::<serde_json::Value, ErrorObject<'static>>(
                        serde_json::json!({
                            "enabled":     false,
                            "rows":        0,
                            "total_hits":  0,
                            "bytes_rough": 0,
                        })
                    ),
                };
                let s = mgr.cache().stats().map_err(|e| rpc_err(-32004, e))?;
                Ok(serde_json::json!({
                    "enabled":     mgr.enabled(),
                    "ttl_secs":    mgr.ttl_secs(),
                    "rows":        s.rows,
                    "total_hits":  s.total_hits,
                    "bytes_rough": s.bytes_rough,
                }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;
            log::debug!("v4/llm.cache.stats: done");
            Ok::<JsonValue, ErrorObject>(resp)
        })
        .unwrap();
}

// ── v4/llm.cache.purge ───────────────────────────────────────────────

fn register_cache_purge(module: &mut RpcModule<()>) {
    module
        .register_async_method("v4/llm.cache.purge", |params, _ctx, _| async move {
            log::debug!("v4/llm.cache.purge: start");
            let verified = authenticate(params).await?;
            let resp = tokio::task::spawn_blocking(move || {
                let mgr = bdslib::llm::cache::manager()
                    .ok_or_else(|| rpc_err(-32004, "llm cache not initialised"))?;
                let filter = bdslib::llm::cache::PurgeFilter {
                    older_than_created: verified.get("older_than_created")
                        .and_then(|v| v.as_u64()),
                    provider: verified.get("provider")
                        .and_then(|v| v.as_str()).map(str::to_owned),
                    kind: verified.get("kind")
                        .and_then(|v| v.as_str()).map(str::to_owned),
                };
                let purged = mgr.cache().purge(filter)
                    .map_err(|e| rpc_err(-32004, e))?;
                Ok::<JsonValue, ErrorObject<'static>>(serde_json::json!({
                    "purged": purged,
                }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;
            log::debug!("v4/llm.cache.purge: done");
            Ok::<JsonValue, ErrorObject>(resp)
        })
        .unwrap();
}

// ── v4/llm.providers.list ────────────────────────────────────────────

fn register_providers_list(module: &mut RpcModule<()>) {
    module
        .register_async_method("v4/llm.providers.list", |params, _ctx, _| async move {
            log::debug!("v4/llm.providers.list: start");
            // The endpoint accepts no body params, but we still go
            // through the HMAC gate to keep the v4/* discipline uniform.
            let _verified = authenticate(params).await?;
            let resp = tokio::task::spawn_blocking(|| {
                api::providers_list()
                    .map(dynamic_to_json)
                    .map_err(|e| rpc_err(-32004, e))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;
            log::debug!("v4/llm.providers.list: done");
            Ok::<JsonValue, ErrorObject>(resp)
        })
        .unwrap();
}
