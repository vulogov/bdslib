//! `v2/llm.cache.*` — unauthenticated receivers for the inference cache.
//!
//! Called by `replicate_to_all` (write fan-out from `vm::api::llm::*`
//! cache_store), by the anti-entropy loop, and (in phase 3.d) by
//! `dispatch::read` for cluster-wide cache hits.  Same trust model as
//! the existing `v2/user.*` receivers — caller authentication is the
//! firewall in front of the bdsnode RPC port, not an `_hmac` field
//! per call.
//!
//! All methods are idempotent under retry — replication and hint
//! replay must be safe to re-fire repeatedly without duplicating rows
//! (the cache layer's `put` short-circuits on either an `id` or a
//! `cache_key` collision).

use super::params::rpc_err;
use bdslib::llm::cache::{self, CacheInsert};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::{json, Value as JsonValue};
use uuid::Uuid;

pub fn register(module: &mut RpcModule<()>) {
    register_get(module);
    register_get_by_id(module);
    register_put(module);
    register_list_ids(module);
    register_delete(module);
}

fn manager_or_err() -> Result<&'static cache::CacheManager, ErrorObject<'static>> {
    cache::manager().ok_or_else(|| rpc_err(-32004,
        "llm cache manager not initialised on this node"))
}

// ── v2/llm.cache.get ─────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct GetParams {
    cache_key: String,
}

fn register_get(module: &mut RpcModule<()>) {
    module.register_async_method("v2/llm.cache.get", |params, _ctx, _| async move {
        let p: GetParams = params.parse()?;
        let resp = tokio::task::spawn_blocking(move || {
            let mgr = manager_or_err()?;
            match mgr.cache().get_by_key(&p.cache_key) {
                Ok(Some(e)) => Ok::<JsonValue, ErrorObject<'static>>(json!({
                    "hit": true,
                    "id":            e.id.to_string(),
                    "cache_key":     e.cache_key,
                    "provider":      e.provider,
                    "model":         e.model,
                    "kind":          e.kind,
                    "request_json":  e.request_json,
                    "response_json": e.response_json,
                    "source_meta":   e.source_meta,
                    "created_at":    e.created_at,
                    "expires_at":    e.expires_at,
                    "updated_at":    e.updated_at,
                    "hits":          e.hits,
                })),
                Ok(None) => Ok(json!({"hit": false})),
                Err(e)   => Err(rpc_err(-32004, e)),
            }
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;
        Ok::<JsonValue, ErrorObject>(resp)
    }).unwrap();
}

// ── v2/llm.cache.get.by_id ───────────────────────────────────────────
// Used by the anti-entropy pull_one path.

#[derive(serde::Deserialize)]
struct GetByIdParams {
    id: String,
}

fn register_get_by_id(module: &mut RpcModule<()>) {
    module.register_async_method("v2/llm.cache.get.by_id", |params, _ctx, _| async move {
        let p: GetByIdParams = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let resp = tokio::task::spawn_blocking(move || {
            let mgr = manager_or_err()?;
            match mgr.cache().get_by_id(id) {
                Ok(Some(e)) => Ok::<JsonValue, ErrorObject<'static>>(json!({
                    "found": true,
                    "id":            e.id.to_string(),
                    "cache_key":     e.cache_key,
                    "provider":      e.provider,
                    "model":         e.model,
                    "kind":          e.kind,
                    "request_json":  e.request_json,
                    "response_json": e.response_json,
                    "source_meta":   e.source_meta,
                    "created_at":    e.created_at,
                    "expires_at":    e.expires_at,
                    "updated_at":    e.updated_at,
                    "hits":          e.hits,
                })),
                Ok(None) => Ok(json!({"found": false})),
                Err(e)   => Err(rpc_err(-32004, e)),
            }
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;
        Ok::<JsonValue, ErrorObject>(resp)
    }).unwrap();
}

// ── v2/llm.cache.put ─────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct PutParams {
    id:            String,
    cache_key:     String,
    provider:      String,
    model:         String,
    kind:          String,
    request_json:  JsonValue,
    response_json: JsonValue,
    #[serde(default)]
    source_meta:   Option<JsonValue>,
    created_at:    u64,
    expires_at:    u64,
}

fn register_put(module: &mut RpcModule<()>) {
    module.register_async_method("v2/llm.cache.put", |params, _ctx, _| async move {
        let p: PutParams = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let resp = tokio::task::spawn_blocking(move || {
            let mgr = manager_or_err()?;
            let insert = CacheInsert {
                id,
                cache_key:     p.cache_key,
                provider:      p.provider,
                model:         p.model,
                kind:          p.kind,
                request_json:  p.request_json,
                response_json: p.response_json,
                source_meta:   p.source_meta,
                created_at:    p.created_at,
                expires_at:    p.expires_at,
            };
            mgr.cache().put(insert).map_err(|e| rpc_err(-32004, e))?;
            Ok::<JsonValue, ErrorObject<'static>>(json!({"ok": true, "id": id.to_string()}))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;
        Ok::<JsonValue, ErrorObject>(resp)
    }).unwrap();
}

// ── v2/llm.cache.list_ids ────────────────────────────────────────────

fn register_list_ids(module: &mut RpcModule<()>) {
    module.register_async_method("v2/llm.cache.list_ids", |_params, _ctx, _| async move {
        let resp = tokio::task::spawn_blocking(move || {
            let mgr = manager_or_err()?;
            let ids = mgr.cache().list_ids().map_err(|e| rpc_err(-32004, e))?;
            // Shape matches the v2/<store>.list_ids contract the AE
            // machinery expects: `{live: [{id, updated_at}, …],
            // tombstones: []}`.  Tombstones for the cache aren't
            // wired through yet — landing soon when 3.d ships the
            // coordinator delete path.
            let live: Vec<JsonValue> = ids.into_iter().map(|(id, ts)|
                json!({"id": id.to_string(), "updated_at": ts})).collect();
            Ok::<JsonValue, ErrorObject<'static>>(json!({"live": live, "tombstones": []}))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;
        Ok::<JsonValue, ErrorObject>(resp)
    }).unwrap();
}

// ── v2/llm.cache.delete ──────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct DeleteParams {
    id: String,
}

fn register_delete(module: &mut RpcModule<()>) {
    module.register_async_method("v2/llm.cache.delete", |params, _ctx, _| async move {
        let p: DeleteParams = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let resp = tokio::task::spawn_blocking(move || {
            let mgr = manager_or_err()?;
            mgr.cache().delete(id).map_err(|e| rpc_err(-32004, e))?;
            Ok::<JsonValue, ErrorObject<'static>>(json!({"ok": true, "id": id.to_string()}))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;
        Ok::<JsonValue, ErrorObject>(resp)
    }).unwrap();
}
