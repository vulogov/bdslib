//! Fan-out helper for v3/* distributed reads.
//!
//! Calls a `v2/*` method on every Alive peer in parallel, returning per-peer
//! results plus a coarse partial-failure flag.  Coordinator-side handlers
//! (in `bdsnode/jsonrpc/v3_*.rs`) compose this with their own local result
//! to produce a deduplicated answer.
//!
//! Outbound calls do **not** carry HMAC: v3/* distributed reads are issued
//! by trusted local clients (the same trust boundary as v2/*), and the
//! receiver-side v2/* handlers are unauthenticated.

use crate::cluster::peer_table::Peer;
use crate::cluster::Cluster;
use crate::common::error::{err_msg, Result};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

/// Outcome of a single peer call.
pub struct PeerResult {
    pub peer:   Peer,
    pub result: Result<JsonValue>,
}

impl std::fmt::Debug for PeerResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerResult")
            .field("node_id", &self.peer.node_id)
            .field("url",     &self.peer.url)
            .field("ok",      &self.result.is_ok())
            .finish()
    }
}

/// Aggregate fan-out result.  `cluster_meta()` produces the JSON block
/// that v3/* responses embed so callers can detect partial answers.
pub struct FanOutResults {
    pub responses:      Vec<PeerResult>,
    pub peers_queried:  usize,
    pub peers_answered: usize,
}

impl FanOutResults {
    pub fn partial(&self) -> bool {
        self.peers_answered < self.peers_queried
    }

    /// Iterate over successful peer results only.
    pub fn ok_results(&self) -> impl Iterator<Item = &JsonValue> {
        self.responses.iter().filter_map(|r| r.result.as_ref().ok())
    }

    /// Standard `cluster_meta` block embedded in every v3/* read response.
    /// Callers add `"local"` themselves after calling the local handler.
    pub fn cluster_meta(&self) -> JsonValue {
        let failed: Vec<JsonValue> = self.responses.iter()
            .filter_map(|r| r.result.as_ref().err().map(|e| serde_json::json!({
                "node_id": r.peer.node_id.to_string(),
                "url":     r.peer.url,
                "error":   e.to_string(),
            })))
            .collect();
        serde_json::json!({
            "peers_queried":  self.peers_queried,
            "peers_answered": self.peers_answered,
            "partial":        self.partial(),
            "failed":         failed,
        })
    }
}

/// Issue `method` against every Alive peer with the supplied `params`.
/// Returns a `FanOutResults` with one entry per peer (success or error).
///
/// Per-peer timeout = `cluster.peer_rpc_timeout`.  The local node is **not**
/// queried — coordinator handlers do that in-process before/after this.
pub async fn fan_out_v2(
    cluster: &Arc<Cluster>,
    method:  &str,
    params:  JsonValue,
) -> FanOutResults {
    let alive = cluster.peers.read().alive();
    let timeout = Duration::from_secs(cluster.config.peer_rpc_timeout_secs);

    if alive.is_empty() {
        return FanOutResults { responses: vec![], peers_queried: 0, peers_answered: 0 };
    }

    let mut set: JoinSet<PeerResult> = JoinSet::new();
    for peer in alive.iter().cloned() {
        let http   = cluster.http.clone();
        let url    = peer.url.clone();
        let method = method.to_owned();
        let params = params.clone();
        set.spawn(async move {
            let result = call_v2(&http, &url, &method, params, timeout).await;
            PeerResult { peer, result }
        });
    }

    let mut responses = Vec::with_capacity(alive.len());
    let mut answered  = 0;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(r) => {
                if r.result.is_ok() { answered += 1; }
                responses.push(r);
            }
            Err(e) => log::warn!("[cluster] fan_out_v2 task panicked: {e:?}"),
        }
    }
    FanOutResults {
        peers_queried: alive.len(),
        peers_answered: answered,
        responses,
    }
}

async fn call_v2(
    http:    &reqwest::Client,
    url:     &str,
    method:  &str,
    params:  JsonValue,
    timeout: Duration,
) -> Result<JsonValue> {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method":  method,
        "params":  params,
        "id":      1,
    });
    let body = serde_json::to_string(&payload)
        .map_err(|e| err_msg(format!("serialize fanout body: {e}")))?;

    let resp = http.post(url)
        .timeout(timeout)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| err_msg(format!("{method} -> {url}: {e}")))?;

    let text = resp.text().await
        .map_err(|e| err_msg(format!("{method} <- {url}: read body: {e}")))?;
    let envelope: JsonValue = serde_json::from_str(&text)
        .map_err(|e| err_msg(format!("{method} <- {url}: invalid JSON: {e}")))?;

    if let Some(err) = envelope.get("error") {
        let msg = err.get("message").and_then(|v| v.as_str()).unwrap_or("unknown");
        return Err(err_msg(format!("{method} rpc error: {msg}")));
    }
    Ok(envelope.get("result").cloned().unwrap_or_default())
}
