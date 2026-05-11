//! v2/user.* — unauthenticated receivers for the cluster-replicated
//! user store.  Called by `replicate_to_all` (write fan-out from v3
//! coordinators), by hint replay, and by the anti-entropy loop.
//!
//! All methods are idempotent under retry — the same call body MUST
//! be safe to re-fire as many times as the hint replay or AE pull
//! path needs.  In particular `v2/user.add` short-circuits if the
//! UUID already exists, and `v2/user.modify` honours `if_newer` LWW
//! semantics by default (the v3 coordinator path overrides this with
//! `if_newer: false` for an authoritative local commit).

use super::params::rpc_err;
use bdslib::cluster::credential::AuthMethod;
use bdslib::cluster::user_store::{UserPatch, UserSummary};
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub fn register(module: &mut RpcModule<()>) {
    register_add(module);
    register_modify(module);
    register_delete(module);
    register_get_by_username(module);
    register_get_by_id(module);
    register_list_ids(module);
}

// ── v2/user.add ──────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct AddParams {
    /// Caller-supplied UUIDv7.  Required for replication so every
    /// replica writes under the same identity.
    id:           String,
    username:     String,
    /// For `auth_method == "password"` this is the raw password to
    /// be hashed by the local PasswordVerifier.  For OAuth/LDAP rows
    /// it's whatever the verifier wants stored verbatim.
    credential:   String,
    #[serde(default = "default_method")]
    auth_method:  String,
    #[serde(default)]
    metadata:     JsonValue,
    /// Optional creation timestamp (Unix s).  Replication passes this
    /// so all replicas agree; standalone callers can omit.
    #[serde(default)]
    now_secs:     Option<u64>,
}
fn default_method() -> String { "password".into() }

fn register_add(module: &mut RpcModule<()>) {
    module.register_async_method("v2/user.add", |params, _ctx, _| async move {
        let p: AddParams = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let now = p.now_secs.unwrap_or_else(now_secs);
        let result = tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster()
                .ok_or_else(|| rpc_err(-32097, "cluster mode disabled — user store unavailable"))?;
            cluster.users.add(
                id, &p.username, &p.credential,
                AuthMethod::from_wire(&p.auth_method),
                p.metadata, now,
            ).map_err(|e| rpc_err(-32011, e))?;
            Ok::<JsonValue, jsonrpsee::types::ErrorObject>(serde_json::json!({
                "id": id.to_string(),
            }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
        result
    }).unwrap();
}

// ── v2/user.modify ───────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct ModifyParams {
    id: String,
    /// New raw credential.  When present, re-hashed by the verifier
    /// for the row's (post-update) auth_method.
    #[serde(default)]
    credential:      Option<String>,
    /// Switch the auth method.  Combined with `credential` to migrate
    /// a row from password → OAuth, etc.
    #[serde(default)]
    new_auth_method: Option<String>,
    #[serde(default)]
    metadata:        Option<JsonValue>,
    #[serde(default)]
    disabled:        Option<bool>,
    /// LWW guard.  Default `true` — receivers should reject stale
    /// updates so AE pull-newer doesn't clobber a concurrent local
    /// edit.  The v3 coordinator overrides to `false` for the local
    /// authoritative commit path.
    #[serde(default = "default_if_newer")]
    if_newer:        bool,
    #[serde(default)]
    now_secs:        Option<u64>,
}
fn default_if_newer() -> bool { true }

fn register_modify(module: &mut RpcModule<()>) {
    module.register_async_method("v2/user.modify", |params, _ctx, _| async move {
        let p: ModifyParams = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let now = p.now_secs.unwrap_or_else(now_secs);
        let result = tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster()
                .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            let patch = UserPatch {
                credential:      p.credential,
                new_auth_method: p.new_auth_method.as_deref().map(AuthMethod::from_wire),
                metadata:        p.metadata,
                disabled:        p.disabled,
            };
            cluster.users.modify(id, &patch, p.if_newer, now)
                .map_err(|e| rpc_err(-32011, e))?;
            Ok::<JsonValue, jsonrpsee::types::ErrorObject>(serde_json::json!({
                "id": id.to_string(), "applied": true,
            }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
        result
    }).unwrap();
}

// ── v2/user.delete ───────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct DeleteParams {
    id: String,
    /// Tombstone timestamp from the coordinator.  Replicas record the
    /// same value so AE doesn't see a tombstone-disagreement.  When
    /// absent, this node uses its own wall clock — fine for the v3
    /// coordinator path (it's the authoritative one), risky for hint
    /// replay (tombstones diverge by replay-delay seconds).
    #[serde(default)]
    deleted_at: Option<i64>,
}

fn register_delete(module: &mut RpcModule<()>) {
    module.register_async_method("v2/user.delete", |params, _ctx, _| async move {
        let p: DeleteParams = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let deleted_at = p.deleted_at.unwrap_or_else(|| now_secs() as i64);
        let result = tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster()
                .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            cluster.users.delete(id).map_err(|e| rpc_err(-32011, e))?;
            // Tombstone so AE doesn't resurrect from a peer that
            // hasn't seen the delete yet.  Same store-name convention
            // as docs/scripts.
            if let Err(e) = cluster.tombstones.mark_deleted("users", id, deleted_at) {
                log::warn!("v2/user.delete: tombstone {id}: {e}");
            }
            Ok::<JsonValue, jsonrpsee::types::ErrorObject>(serde_json::json!({
                "id": id.to_string(), "deleted_at": deleted_at,
            }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
        result
    }).unwrap();
}

// ── v2/user.get_by_username ──────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct GetByUsernameParams {
    username: String,
    /// When `true`, the response includes the credential_hash so the
    /// caller can verify against it without a second RPC.  Default
    /// `false` — the AE pull path needs it; the bdsweb operator
    /// listing must NOT.
    #[serde(default)]
    include_hash: bool,
}

fn register_get_by_username(module: &mut RpcModule<()>) {
    module.register_async_method("v2/user.get_by_username", |params, _ctx, _| async move {
        let p: GetByUsernameParams = params.parse()?;
        let result = tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster()
                .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            let user = cluster.users.get_by_username(&p.username)
                .map_err(|e| rpc_err(-32011, e))?;
            let body = match user {
                None    => serde_json::json!({ "user": null }),
                Some(u) => {
                    let mut obj = serde_json::json!({
                        "id":          u.id.to_string(),
                        "username":    u.username,
                        "auth_method": u.auth_method.to_wire(),
                        "metadata":    u.metadata,
                        "created_at":  u.created_at,
                        "updated_at":  u.updated_at,
                        "disabled":    u.disabled,
                    });
                    if p.include_hash {
                        obj.as_object_mut().unwrap()
                           .insert("credential_hash".into(), JsonValue::String(u.credential_hash));
                    }
                    serde_json::json!({ "user": obj })
                }
            };
            Ok::<JsonValue, jsonrpsee::types::ErrorObject>(body)
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
        result
    }).unwrap();
}

// ── v2/user.get_by_id ────────────────────────────────────────────────────────
//
// Used by the anti-entropy pull path to backfill a missing user row by id.
// Returns the same shape as `v2/user.get_by_username` (always with the
// credential_hash so the receiver can write the row verbatim under the
// same identity).

#[derive(serde::Deserialize)]
struct GetByIdParams {
    id: String,
}

fn register_get_by_id(module: &mut RpcModule<()>) {
    module.register_async_method("v2/user.get_by_id", |params, _ctx, _| async move {
        let p: GetByIdParams = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let result = tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster()
                .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            let user = cluster.users.get(id).map_err(|e| rpc_err(-32011, e))?;
            let body = match user {
                None    => serde_json::json!({ "user": null }),
                Some(u) => serde_json::json!({
                    "user": {
                        "id":              u.id.to_string(),
                        "username":        u.username,
                        "credential_hash": u.credential_hash,
                        "auth_method":     u.auth_method.to_wire(),
                        "metadata":        u.metadata,
                        "created_at":      u.created_at,
                        "updated_at":      u.updated_at,
                        "disabled":        u.disabled,
                    }
                }),
            };
            Ok::<JsonValue, jsonrpsee::types::ErrorObject>(body)
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
        result
    }).unwrap();
}

// ── v2/user.list_ids ─────────────────────────────────────────────────────────

fn register_list_ids(module: &mut RpcModule<()>) {
    module.register_async_method("v2/user.list_ids", |_params, _ctx, _| async move {
        // Match the shape that docs/signals/scripts list_ids returns —
        // {live: [{id, updated_at}], tombstones: [{id, deleted_at}]} —
        // so the AE loop can reuse the existing sync_store recipe.
        let result = tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster()
                .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            let summaries: Vec<UserSummary> = cluster.users.list_summaries()
                .map_err(|e| rpc_err(-32011, e))?;
            let live: Vec<JsonValue> = summaries.into_iter().map(|s| {
                serde_json::json!({ "id": s.id.to_string(), "updated_at": s.updated_at })
            }).collect();
            let tombstones: Vec<JsonValue> = cluster.tombstones.list_for_store("users")
                .map_err(|e| rpc_err(-32011, e))?
                .into_iter()
                .map(|t| serde_json::json!({ "id": t.id.to_string(), "deleted_at": t.deleted_at }))
                .collect();
            Ok::<JsonValue, jsonrpsee::types::ErrorObject>(serde_json::json!({
                "live":       live,
                "tombstones": tombstones,
            }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
        result
    }).unwrap();
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
