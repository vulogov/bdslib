//! Helpers for v3/* fan-out replication.
//!
//! Two patterns are supported:
//!
//! - **Sharded** (`pick_random_alive` + per-peer `call_peer_v2`): pick
//!   `k = replication_factor - 1` random Alive peers and replicate
//!   there.  Used by the standard `v3/add` write path coordinated in
//!   `bdsnode/jsonrpc/v3_add.rs`.
//!
//! - **Fully-replicated** (`replicate_to_all`): replicate to **every**
//!   Alive peer.  Failures enqueue hints in
//!   `<dbpath>/network/hints.duckdb` for replay by the cluster
//!   background task.  Used by every fully-replicated store (docs,
//!   signals, scripts, templates) and by the `vm::api::*` write
//!   helpers when a Bund script makes a write under cluster mode.

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

// ─────────────────────────────────────────────────────────────────────────────
// Fully-replicated fan-out (docs / signals / scripts / templates / vm::api)
// ─────────────────────────────────────────────────────────────────────────────

/// Outcome of a `replicate_to_all` call — surfaced both through bdsnode
/// v3/* responses and through `vm::api::meta::LAST_META` so Bund scripts
/// can introspect partial replication.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub peers_attempted: usize,
    pub peers_succeeded: usize,
    pub hints_queued:    usize,
}

impl Outcome {
    fn default_with_attempted(n: usize) -> Self {
        Self { peers_attempted: n, peers_succeeded: 0, hints_queued: n }
    }

    /// Wire-format JSON used by the bdsnode v3/* write handlers.
    pub fn to_json(&self) -> JsonValue {
        serde_json::json!({
            "peers_attempted": self.peers_attempted,
            "peers_succeeded": self.peers_succeeded,
            "hints_queued":    self.hints_queued,
        })
    }
}

/// Fan out the same v2/* call to **every** Alive peer; failures enqueue
/// a hint into the cluster's `HintStorage`.  Returns the per-peer
/// outcome counters; the caller decides how to surface them (response
/// JSON for v3/* handlers, `meta::LAST_META` for `vm::api::*`).
///
/// `params` should already have any required `id` field injected so
/// every replica writes under the same identity.  The serialised
/// payload is computed exactly once for hint enqueue.
pub async fn replicate_to_all(
    cluster: Arc<Cluster>,
    method:  &'static str,
    params:  JsonValue,
) -> Outcome {
    let peers = cluster.peers.read().alive();
    let n_peers = peers.len();
    if peers.is_empty() {
        return Outcome { peers_attempted: 0, peers_succeeded: 0, hints_queued: 0 };
    }

    let canonical = match serde_json::to_vec(&params) {
        Ok(b)  => b,
        Err(e) => {
            log::warn!("[cluster::replication] serialize hint payload: {e}");
            return Outcome::default_with_attempted(n_peers);
        }
    };

    // Wrap once.  Per-peer task gets a cheap Arc::clone (atomic
    // refcount bump) instead of deep-cloning the full doc tree.
    // Significant under write bursts where `params` is a multi-KB
    // document or a batch.
    let params_arc    = Arc::new(params);
    let canonical_arc = Arc::new(canonical);

    let mut joins = Vec::with_capacity(peers.len());
    for peer in peers {
        let cluster = cluster.clone();
        let params  = Arc::clone(&params_arc);
        let bytes   = Arc::clone(&canonical_arc);
        joins.push(tokio::spawn(async move {
            replicate_one(cluster, peer, method, params, bytes).await
        }));
    }

    let mut succeeded = 0;
    let mut hinted    = 0;
    for j in joins {
        match j.await {
            Ok(true)  => succeeded += 1,
            Ok(false) => hinted    += 1,
            Err(e)    => log::error!("[cluster::replication] task panicked: {e:?}"),
        }
    }
    Outcome {
        peers_attempted: n_peers,
        peers_succeeded: succeeded,
        hints_queued:    hinted,
    }
}

async fn replicate_one(
    cluster: Arc<Cluster>,
    peer:    Peer,
    method:  &'static str,
    params:  Arc<JsonValue>,
    bytes:   Arc<Vec<u8>>,
) -> bool {
    // Time the write replication RPC for v2/perf.  Distinct prefix from
    // `fanout.*` (read fan-out) so operators can isolate write load.
    let node_label = peer.node_id.to_string();
    let started    = std::time::Instant::now();
    let res        = call_peer_v2(&cluster, &peer.url, method, &params).await;
    let elapsed_us = started.elapsed().as_micros() as u64;
    crate::perf::record_us(&format!("replicate.peer.{node_label}"), elapsed_us);
    crate::perf::record_us(&format!("replicate.method.{method}"),    elapsed_us);
    match res {
        Ok(_)  => { log::debug!("[cluster::replication {method}] -> {} ok", peer.url); true }
        Err(e) => {
            log::warn!("[cluster::replication {method}] -> {} failed: {e}; hinting", peer.url);
            if let Err(err) = cluster.hints.enqueue(peer.node_id, method, bytes.as_slice()) {
                log::error!("[cluster::replication {method}] enqueue hint for {}: {err}", peer.url);
            }
            false
        }
    }
}
