//! v3/user.{add,modify,delete,authenticate,list} — cluster-aware
//! coordinators for the user store.
//!
//! All admin methods (add / modify / delete / list) are HMAC-protected
//! the same way `v3/cluster.*` methods are — the cluster shared secret
//! gates them.  `v3/user.add` makes one exception: the **first** add on
//! an empty user store goes through unauthenticated so a fresh
//! deployment can be bootstrapped without first wiring HMAC into a CLI
//! tool.  Once `users.is_empty()` returns false the bypass closes.
//!
//! `v3/user.authenticate` is the public login path and is NEVER HMAC-
//! protected — but Phase 4 will mount it behind the
//! `tower_governor` rate limiter so brute force costs more than free
//! attempts allow.

use super::cluster::authenticate_admin;
use super::params::{rpc_err, v3_cluster_meta};
use super::v3_replicated::replicate_to_all;
use bdslib::cluster::credential::AuthMethod;
use bdslib::cluster::fanout::{self, FanOutResults};
use bdslib::cluster::session::issue_session_token;
use bdslib::cluster::user_store::UserPatch;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub fn register(module: &mut RpcModule<()>) {
    register_add(module);
    register_modify(module);
    register_delete(module);
    register_authenticate(module);
    register_list(module);
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// HMAC gate that allows one bypass: when the user store is empty AND
/// the admin is calling `v3/user.add`, let the call through so the
/// first deployment can mint its first admin without ambient secret
/// distribution.  Returns the params object on success or maps to the
/// usual auth error on failure.
async fn admin_or_first_user_bootstrap(
    params: jsonrpsee::types::Params<'static>,
) -> Result<serde_json::Map<String, JsonValue>, ErrorObject<'static>> {
    // Bootstrap shortcut: if the user store is empty, accept the call
    // unconditionally.  We still parse the params object out so the
    // call site downstream gets the same shape as the post-bootstrap
    // path.
    let raw = params.parse::<JsonValue>()
        .map_err(|e| rpc_err(-32602, format!("invalid params: {e}")))?;
    let obj = raw.as_object()
        .ok_or_else(|| rpc_err(-32602, "params must be an object"))?
        .clone();

    let bootstrap = tokio::task::spawn_blocking(|| {
        let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
        let cluster = db.cluster()
            .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
        cluster.users.is_empty().map_err(|e| rpc_err(-32011, e))
    }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

    if bootstrap {
        log::info!("[v3/user.add] BOOTSTRAP: user store is empty — admitting first call without HMAC");
        return Ok(obj);
    }

    // Normal path: same HMAC gate the v3/cluster.* methods use.
    let raw = JsonValue::Object(obj);
    authenticate_admin(raw).await
}

fn now_secs_u64() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ── v3/user.add ──────────────────────────────────────────────────────────────

fn register_add(module: &mut RpcModule<()>) {
    module.register_async_method("v3/user.add", |params, _ctx, _| async move {
        let obj = admin_or_first_user_bootstrap(params).await?;

        let username = obj.get("username").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'username'"))?
            .to_owned();
        let password = obj.get("password").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'password'"))?
            .to_owned();
        let auth_method = obj.get("auth_method").and_then(|v| v.as_str())
            .unwrap_or("password").to_owned();
        let metadata = obj.get("metadata").cloned().unwrap_or(JsonValue::Null);

        // Caller-supplied UUID lets the same v3/user.add call fan out
        // to peers under a shared identity.  When absent we mint a
        // UUIDv7 here so every replica writes the same row.
        let id = match obj.get("id").and_then(|v| v.as_str()) {
            Some(s) => Uuid::parse_str(s).map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?,
            None    => Uuid::now_v7(),
        };
        let now = now_secs_u64();

        // Local commit first.
        let id_local = id;
        let username_local = username.clone();
        let password_local = password.clone();
        let method_local = AuthMethod::from_wire(&auth_method);
        let metadata_local = metadata.clone();
        tokio::task::spawn_blocking(move || -> Result<(), ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster().ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            cluster.users.add(id_local, &username_local, &password_local, method_local,
                              metadata_local, now)
                .map_err(|e| rpc_err(-32011, e))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        // Fan out to all Alive peers.  Re-use the password verbatim;
        // each peer hashes locally.  This is intentional — we do NOT
        // ship the local hash because peers' argon2 params might
        // differ in the future (e.g. one node on a faster CPU).
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let outcome = match &cluster {
            Some(c) => {
                let v2_params = serde_json::json!({
                    "id":          id.to_string(),
                    "username":    username,
                    "credential":  password,
                    "auth_method": auth_method,
                    "metadata":    metadata,
                    "now_secs":    now,
                });
                Some(replicate_to_all(c.clone(), "v2/user.add", v2_params).await)
            }
            None => None,
        };
        let outcome_json = outcome.as_ref().map(|o| o.to_json())
            .unwrap_or_else(|| serde_json::json!({"peers_attempted":0,"peers_succeeded":0,"hints_queued":0}));

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "id":           id.to_string(),
            "outcome":      outcome_json,
            "cluster_meta": v3_cluster_meta(None::<FanOutResults>),
        }))
    }).unwrap();
}

// ── v3/user.modify ───────────────────────────────────────────────────────────

fn register_modify(module: &mut RpcModule<()>) {
    module.register_async_method("v3/user.modify", |params, _ctx, _| async move {
        let raw = params.parse::<JsonValue>()
            .map_err(|e| rpc_err(-32602, format!("invalid params: {e}")))?;
        let obj = authenticate_admin(raw).await?;

        let id_str = obj.get("id").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'id'"))?.to_owned();
        let id = Uuid::parse_str(&id_str)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let new_password = obj.get("password").and_then(|v| v.as_str()).map(str::to_owned);
        let new_method   = obj.get("new_auth_method").and_then(|v| v.as_str()).map(str::to_owned);
        let new_meta     = obj.get("metadata").cloned();
        let disabled     = obj.get("disabled").and_then(|v| v.as_bool());
        let now = now_secs_u64();

        // Local authoritative commit — `if_newer = false` because the
        // operator's intent IS the latest state.  Replicas use the
        // default `if_newer = true` so AE-driven re-fires don't
        // clobber a concurrent edit.
        let new_password_local = new_password.clone();
        let new_method_local = new_method.clone();
        let new_meta_local = new_meta.clone();
        tokio::task::spawn_blocking(move || -> Result<(), ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster().ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            let patch = UserPatch {
                credential:      new_password_local,
                new_auth_method: new_method_local.as_deref().map(AuthMethod::from_wire),
                metadata:        new_meta_local,
                disabled,
            };
            cluster.users.modify(id, &patch, /* if_newer = */ false, now)
                .map_err(|e| rpc_err(-32011, e))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        // Fan-out.  Replicas honour if_newer = true (default), so a
        // stale replay can't clobber a concurrent local edit.
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let outcome = match &cluster {
            Some(c) => {
                let mut v2_params = serde_json::json!({
                    "id": id.to_string(), "now_secs": now,
                });
                if let (Some(obj), Some(p)) = (v2_params.as_object_mut(), new_password.as_ref()) {
                    obj.insert("credential".into(), JsonValue::String(p.clone()));
                }
                if let (Some(obj), Some(m)) = (v2_params.as_object_mut(), new_method.as_ref()) {
                    obj.insert("new_auth_method".into(), JsonValue::String(m.clone()));
                }
                if let (Some(obj), Some(m)) = (v2_params.as_object_mut(), new_meta.as_ref()) {
                    obj.insert("metadata".into(), m.clone());
                }
                if let (Some(obj), Some(d)) = (v2_params.as_object_mut(), disabled) {
                    obj.insert("disabled".into(), JsonValue::Bool(d));
                }
                Some(replicate_to_all(c.clone(), "v2/user.modify", v2_params).await)
            }
            None => None,
        };
        let outcome_json = outcome.as_ref().map(|o| o.to_json())
            .unwrap_or_else(|| serde_json::json!({"peers_attempted":0,"peers_succeeded":0,"hints_queued":0}));

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "id": id.to_string(), "outcome": outcome_json,
            "cluster_meta": v3_cluster_meta(None::<FanOutResults>),
        }))
    }).unwrap();
}

// ── v3/user.delete ───────────────────────────────────────────────────────────

fn register_delete(module: &mut RpcModule<()>) {
    module.register_async_method("v3/user.delete", |params, _ctx, _| async move {
        let raw = params.parse::<JsonValue>()
            .map_err(|e| rpc_err(-32602, format!("invalid params: {e}")))?;
        let obj = authenticate_admin(raw).await?;
        let id_str = obj.get("id").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'id'"))?.to_owned();
        let id = Uuid::parse_str(&id_str)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let deleted_at = now_secs_u64() as i64;

        // Local delete + tombstone.
        tokio::task::spawn_blocking(move || -> Result<(), ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster().ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            cluster.users.delete(id).map_err(|e| rpc_err(-32011, e))?;
            if let Err(e) = cluster.tombstones.mark_deleted("users", id, deleted_at) {
                log::warn!("v3/user.delete: tombstone {id}: {e}");
            }
            Ok(())
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let outcome = match &cluster {
            Some(c) => {
                let v2_params = serde_json::json!({
                    "id": id.to_string(), "deleted_at": deleted_at,
                });
                Some(replicate_to_all(c.clone(), "v2/user.delete", v2_params).await)
            }
            None => None,
        };
        let outcome_json = outcome.as_ref().map(|o| o.to_json())
            .unwrap_or_else(|| serde_json::json!({"peers_attempted":0,"peers_succeeded":0,"hints_queued":0}));

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "id": id.to_string(), "deleted": true, "deleted_at": deleted_at,
            "outcome": outcome_json,
            "cluster_meta": v3_cluster_meta(None::<FanOutResults>),
        }))
    }).unwrap();
}

// ── v3/user.authenticate ─────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct AuthParams {
    username: String,
    password: String,
}

/// Public login path.  Local-first verify, then a v2/user.get_by_username
/// fan-out fallback if the user isn't here yet (covers the AE window
/// after a fresh user.add on a peer).  On success returns a session
/// token signed with the cluster shared secret.
///
/// NOTE: NOT HMAC-protected by design — this is the path users hit.
/// Phase 4 mounts the rate limiter in front of it.
fn register_authenticate(module: &mut RpcModule<()>) {
    module.register_async_method("v3/user.authenticate", |params, _ctx, _| async move {
        let p: AuthParams = params.parse()?;
        let username = p.username.clone();
        let password = p.password.clone();

        // 1. Try local verify.
        let local = tokio::task::spawn_blocking(move || -> Result<Option<String>, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster().ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            match cluster.users.verify(&username, &password).map_err(|e| rpc_err(-32011, e))? {
                Some(u) => Ok(Some(u.id.to_string())),
                None    => Ok(None),
            }
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let user_id_str = match local {
            Some(id) => Some(id),
            None => {
                // 2. Local miss — fan out v2/user.get_by_username with
                // include_hash=true so we can verify locally without a
                // second RPC.  Used to bridge the AE window between
                // user creation on a peer and replication landing here.
                let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
                let mut found_id: Option<String> = None;
                if let Some(c) = &cluster {
                    let fan_params = serde_json::json!({
                        "username": p.username, "include_hash": true,
                    });
                    let fan = fanout::fan_out_v2(c, "v2/user.get_by_username", fan_params).await;
                    // Collect into owned Vec so the iterator doesn't
                    // hold a borrow across the spawn_blocking await
                    // (the Future would otherwise be !Send).
                    let bodies: Vec<JsonValue> = fan.ok_results().cloned().collect();
                    for resp in bodies {
                        let Some(user) = resp.get("user").filter(|v| !v.is_null()) else { continue };
                        let Some(stored_hash) = user.get("credential_hash").and_then(|v| v.as_str()) else { continue };
                        let Some(method_s) = user.get("auth_method").and_then(|v| v.as_str()) else { continue };
                        let Some(id_s) = user.get("id").and_then(|v| v.as_str()) else { continue };
                        let disabled = user.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);
                        if disabled { continue; }
                        let method = AuthMethod::from_wire(method_s);
                        // Compare on this node — guarantees the local
                        // verifier setup is what's used (keeps OAuth
                        // bearer-token introspection consistent).
                        let verifier = c.verifiers.for_method(&method);
                        let stored_hash = stored_hash.to_owned();
                        let presented   = p.password.clone();
                        let id_owned    = id_s.to_owned();
                        let ok = match verifier {
                            Some(v) => tokio::task::spawn_blocking(move || v.verify(&stored_hash, &presented))
                                .await
                                .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?
                                .map_err(|e| rpc_err(-32011, e))?,
                            None => false,
                        };
                        if ok {
                            found_id = Some(id_owned);
                            break;
                        }
                    }
                }
                found_id
            }
        };

        let user_id_str = match user_id_str {
            Some(s) => s,
            None    => {
                // Generic message — never disclose whether the failure
                // was unknown user vs wrong password.
                return Ok::<JsonValue, ErrorObject>(serde_json::json!({
                    "ok": false, "error": "invalid credentials",
                }));
            }
        };

        // Issue session token.
        let user_id = Uuid::parse_str(&user_id_str)
            .map_err(|e| rpc_err(-32004, format!("invalid resolved id: {e}")))?;
        let (token, ttl, expires_at) = tokio::task::spawn_blocking(move || -> Result<(String, u64, u64), ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster().ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            let ttl = cluster.config.session_ttl_secs;
            let token = issue_session_token(user_id, ttl, &cluster.config.shared_secret)
                .map_err(|e| rpc_err(-32004, e))?;
            let expires_at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
                .saturating_add(ttl);
            Ok((token, ttl, expires_at))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "ok":            true,
            "user_id":       user_id_str,
            "session_token": token,
            "ttl_secs":      ttl,
            "expires_at":    expires_at,
        }))
    }).unwrap();
}

// ── v3/user.list ─────────────────────────────────────────────────────────────

fn register_list(module: &mut RpcModule<()>) {
    module.register_async_method("v3/user.list", |params, _ctx, _| async move {
        let raw = params.parse::<JsonValue>().unwrap_or(JsonValue::Object(Default::default()));
        let _ = authenticate_admin(raw).await?;
        let result = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cluster = db.cluster().ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
            let summaries = cluster.users.list_summaries()
                .map_err(|e| rpc_err(-32011, e))?;
            let users: Vec<JsonValue> = summaries.into_iter().map(|s| serde_json::json!({
                "id":          s.id.to_string(),
                "username":    s.username,
                "auth_method": s.auth_method.to_wire(),
                "metadata":    s.metadata,
                "created_at":  s.created_at,
                "updated_at":  s.updated_at,
                "disabled":    s.disabled,
            })).collect();
            Ok(serde_json::json!({ "users": users }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;
        Ok::<JsonValue, ErrorObject>(result)
    }).unwrap();
}
