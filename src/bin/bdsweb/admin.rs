//! HMAC-signed admin RPC helpers for bdsweb.
//!
//! `signed_rpc` is the analogue of `client::rpc` for v3/* admin
//! methods that require an HMAC signature in `_hmac`.  Used by:
//!
//! - the auth middleware (to probe `v3/user.list` and detect the
//!   first-user bootstrap window), and
//! - `routes::admin_users` (to mutate the user store from the
//!   Administration → User management UI).
//!
//! The signed-call recipe mirrors `bdscmd::cmd::cluster::signed_call`:
//! canonicalise the params object, compute HMAC-SHA256 over the bytes,
//! insert `_hmac` as a hex string.  serde_json's default object
//! serialisation sorts keys alphabetically (BTreeMap backing) so the
//! canonical form matches the server's recomputation.

use bdslib::cluster::hmac_auth;
use serde_json::{json, Map, Value};

use crate::{client::rpc_with_timeout, error::AppError, state::AppState};

/// Sign + send a v3/* admin RPC.  `params` is the inner params object
/// (without `_hmac`).  Returns the parsed `result` field of the JSON-RPC
/// envelope on success.
///
/// When `state.shared_secret` is empty (open-access mode), the call is
/// sent unsigned — the server will admit it only via the first-user
/// bootstrap path (where applicable).  Callers should treat the
/// returned error from a closed bootstrap window as "auth required".
pub async fn signed_rpc(state: &AppState, method: &str, params: Value) -> Result<Value, AppError> {
    signed_rpc_with_timeout(state, method, params, None).await
}

/// Like [`signed_rpc`] but with an explicit per-request timeout.  Pass
/// `Some(duration)` for v4/llm.* calls that legitimately exceed the
/// 120s default — e.g. analyzing 50 rows on a CPU-bound local Ollama.
pub async fn signed_rpc_with_timeout(
    state:   &AppState,
    method:  &str,
    params:  Value,
    timeout: Option<std::time::Duration>,
) -> Result<Value, AppError> {
    let obj = match params {
        Value::Object(m) => m,
        Value::Null      => Map::new(),
        other            => return Err(AppError::Rpc(format!(
            "signed_rpc: params must be an object (got {})",
            short_type(&other)
        ))),
    };

    let signed = if state.shared_secret.is_empty() {
        Value::Object(obj)
    } else {
        let mut obj = obj;
        let canonical = serde_json::to_vec(&Value::Object(obj.clone()))
            .map_err(|e| AppError::Rpc(format!("signed_rpc: canonical: {e}")))?;
        let sig = hmac_auth::sign(&state.shared_secret, &canonical);
        obj.insert("_hmac".into(), Value::String(sig));
        Value::Object(obj)
    };

    rpc_with_timeout(state, method, signed, timeout).await
}

fn short_type(v: &Value) -> &'static str {
    match v {
        Value::Null      => "null",
        Value::Bool(_)   => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_)  => "array",
        Value::Object(_) => "object",
    }
}

/// Convenience wrapper: HMAC-sign with no params (for `v3/user.list`).
#[allow(dead_code)]
pub async fn signed_call_empty(state: &AppState, method: &str) -> Result<Value, AppError> {
    signed_rpc(state, method, json!({})).await
}
