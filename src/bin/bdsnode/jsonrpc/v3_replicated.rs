//! Shared fan-out helper for v3 fully-replicated stores (docs, signals, scripts).
//!
//! Unlike `v3/add` (which picks `replication_factor - 1` random Alive peers),
//! these methods replicate to **every** Alive peer.  Failures enqueue hints
//! into the same `<dbpath>/network/hints.duckdb` store; the cluster
//! background task replays them when peers transition Alive again.
//!
//! Common shape:
//!
//! ```text
//! 1. parse caller params
//! 2. resolve UUID (caller-supplied or generate UUIDv7)
//! 3. local write on a blocking thread
//! 4. detached fan-out to ALL Alive peers via the matching v2/* method
//!    (with `id` injected so every replica writes under the same identity)
//! 5. on per-peer failure: enqueue a hint (peer_id, method, params)
//! ```

use bdslib::cluster::replication;
use bdslib::cluster::Peer;
use bdslib::Cluster;
use serde_json::Value as JsonValue;
use std::sync::Arc;

/// Fan out the same v2/* call to every Alive peer; failures hint.
/// `params` already has `id` injected so each peer writes under the
/// shared identity.
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
        Err(e) => { log::warn!("[v3 replicated] serialize hint payload: {e}"); return Outcome::default_with_attempted(n_peers); }
    };

    let mut joins = Vec::with_capacity(peers.len());
    for peer in peers {
        let cluster = cluster.clone();
        let params  = params.clone();
        let bytes   = canonical.clone();
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
            Err(e)    => log::error!("[v3 replicated] task panicked: {e:?}"),
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
    params:  JsonValue,
    bytes:   Vec<u8>,
) -> bool {
    match replication::call_peer_v2(&cluster, &peer.url, method, &params).await {
        Ok(_)  => { log::debug!("[v3 {method}] -> {} ok", peer.url); true }
        Err(e) => {
            log::warn!("[v3 {method}] -> {} failed: {e}; hinting", peer.url);
            if let Err(err) = cluster.hints.enqueue(peer.node_id, method, &bytes) {
                log::error!("[v3 {method}] enqueue hint for {}: {err}", peer.url);
            }
            false
        }
    }
}

#[derive(Default)]
pub struct Outcome {
    pub peers_attempted: usize,
    pub peers_succeeded: usize,
    pub hints_queued:    usize,
}

impl Outcome {
    fn default_with_attempted(n: usize) -> Self {
        Self { peers_attempted: n, peers_succeeded: 0, hints_queued: n }
    }

    pub fn to_json(&self) -> JsonValue {
        serde_json::json!({
            "peers_attempted": self.peers_attempted,
            "peers_succeeded": self.peers_succeeded,
            "hints_queued":    self.hints_queued,
        })
    }
}
