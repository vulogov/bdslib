//! Helpers for v3/add fan-out replication.
//!
//! The actual coordinator lives in `bdsnode/jsonrpc/v3_add.rs`; this module
//! provides the picking and the per-peer call wrapper so the helper can be
//! unit-tested without spinning a JSON-RPC handler.

use crate::cluster::peer_table::Peer;
use crate::cluster::Cluster;
use crate::common::error::{err_msg, Result};
use rand::seq::SliceRandom;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;

/// Pick `k` random Alive peers (excluding self).  `k` is clamped to the
/// number of Alive peers — when `k > alive_count` we return everyone.
pub fn pick_random_alive(cluster: &Arc<Cluster>, k: usize) -> Vec<Peer> {
    let mut alive = cluster.peers.read().alive();
    if alive.is_empty() || k == 0 { return Vec::new(); }
    let mut rng = rand::thread_rng();
    alive.shuffle(&mut rng);
    alive.truncate(k);
    alive
}

/// Issue an unauthenticated v2/* JSON-RPC call against `peer`.  Used by the
/// replication coordinator and by the hint replay task.
pub async fn call_peer_v2(
    cluster: &Arc<Cluster>,
    peer_url: &str,
    method:   &str,
    params:   &JsonValue,
) -> Result<JsonValue> {
    let timeout = Duration::from_secs(cluster.config.peer_rpc_timeout_secs);
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method":  method,
        "params":  params,
        "id":      1,
    });
    let body = serde_json::to_string(&payload)
        .map_err(|e| err_msg(format!("serialize payload: {e}")))?;

    let resp = cluster.http.post(peer_url)
        .timeout(timeout)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| err_msg(format!("{method} -> {peer_url}: {e}")))?;

    let text = resp.text().await
        .map_err(|e| err_msg(format!("{method} <- {peer_url}: read body: {e}")))?;
    let envelope: JsonValue = serde_json::from_str(&text)
        .map_err(|e| err_msg(format!("{method} <- {peer_url}: invalid JSON: {e}")))?;

    if let Some(err) = envelope.get("error") {
        let msg = err.get("message").and_then(|v| v.as_str()).unwrap_or("unknown");
        return Err(err_msg(format!("{method} rpc error: {msg}")));
    }
    Ok(envelope.get("result").cloned().unwrap_or_default())
}
