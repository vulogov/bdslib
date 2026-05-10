//! Outbound JSON-RPC 2.0 client for v3/cluster.* gossip.
//!
//! Uses `reqwest` (already a workspace dependency) rather than introducing
//! `jsonrpsee::client`, so the build surface stays small.  Every request
//! body carries an `_hmac` field; the server-side handlers in
//! `bdsnode/jsonrpc/cluster.rs` reject any request whose HMAC does not match
//! `cluster.shared_secret`.

use crate::cluster::hmac_auth;
use crate::cluster::peer_table::Peer;
use crate::common::error::{err_msg, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Identity payload included in every outbound `cluster.hello` and
/// `cluster.ping`.  The remote echoes its own version of this struct in
/// the response so the caller can update its peer table immediately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node_id:         String,
    pub bind_url:        String,
    pub version:         String,
    pub embedding_model: Option<String>,
    pub started_at:      u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelloResponse {
    pub node_id:         String,
    pub bind_url:        String,
    pub version:         String,
    pub embedding_model: Option<String>,
    pub started_at:      u64,
    /// Peer's view of its own table.  May be empty for fresh nodes.
    #[serde(default)]
    pub peers:           Vec<Peer>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PingResponse {
    pub node_id: String,
    pub ts:      u64,
}

/// Issue a JSON-RPC 2.0 call to `url`, signing the payload with `secret`.
async fn rpc_call(
    http:    &reqwest::Client,
    url:     &str,
    method:  &str,
    params:  serde_json::Value,
    secret:  &str,
    timeout: Duration,
) -> Result<serde_json::Value> {
    // Insert `_hmac` into params so the receiver can verify against the
    // *signed* canonical bytes.  Sign over the raw params with `_hmac`
    // omitted, then re-insert.
    let canonical = serde_json::to_vec(&params)
        .map_err(|e| err_msg(format!("serialize params: {e}")))?;
    let sig = hmac_auth::sign(secret, &canonical);

    let mut params_obj = match params {
        serde_json::Value::Object(m) => m,
        _ => return Err(err_msg("rpc params must be a JSON object")),
    };
    params_obj.insert("_hmac".into(), serde_json::Value::String(sig));

    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method":  method,
        "params":  serde_json::Value::Object(params_obj),
        "id":      1,
    });

    let body = serde_json::to_string(&payload)
        .map_err(|e| err_msg(format!("serialize rpc envelope: {e}")))?;

    let resp = http.post(url)
        .timeout(timeout)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| err_msg(format!("{method} -> {url}: {e}")))?;

    let text = resp.text().await
        .map_err(|e| err_msg(format!("{method} <- {url}: read body: {e}")))?;
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| err_msg(format!("{method} <- {url}: invalid JSON: {e}")))?;

    if let Some(err) = envelope.get("error") {
        let msg = err.get("message").and_then(|v| v.as_str()).unwrap_or("unknown");
        return Err(err_msg(format!("{method} rpc error: {msg}")));
    }
    Ok(envelope.get("result").cloned().unwrap_or_default())
}

pub async fn cluster_hello(
    http:    &reqwest::Client,
    url:     &str,
    secret:  &str,
    me:      &NodeInfo,
    timeout: Duration,
) -> Result<HelloResponse> {
    let params = serde_json::to_value(me)
        .map_err(|e| err_msg(format!("serialize NodeInfo: {e}")))?;
    let result = rpc_call(http, url, "v3/cluster.hello", params, secret, timeout).await?;
    serde_json::from_value(result)
        .map_err(|e| err_msg(format!("parse HelloResponse: {e}")))
}

pub async fn cluster_peers(
    http:    &reqwest::Client,
    url:     &str,
    secret:  &str,
    timeout: Duration,
) -> Result<Vec<Peer>> {
    let result = rpc_call(http, url, "v3/cluster.peers", serde_json::json!({}), secret, timeout).await?;
    let arr = result.get("peers").cloned().unwrap_or_default();
    serde_json::from_value(arr)
        .map_err(|e| err_msg(format!("parse peers: {e}")))
}

pub async fn cluster_ping(
    http:    &reqwest::Client,
    url:     &str,
    secret:  &str,
    timeout: Duration,
) -> Result<PingResponse> {
    let result = rpc_call(http, url, "v3/cluster.ping", serde_json::json!({}), secret, timeout).await?;
    serde_json::from_value(result)
        .map_err(|e| err_msg(format!("parse PingResponse: {e}")))
}
