//! Gossip orchestration: a single tick and a one-shot bootstrap pass.
//!
//! The actual periodic timer lives in `bdsnode/server/cluster.rs`; this module
//! exposes the per-tick async helpers so they can be unit-tested without
//! spinning a tokio interval.

use crate::cluster::peer_table::{now_secs, Peer, PeerState, PeerTable, SharedPeerTable};
use crate::cluster::rpc_client::{self, NodeInfo};
use crate::cluster::Cluster;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Convert a remote `Peer` (as returned by `cluster.peers`) into something
/// suitable for `PeerTable::upsert`.  The receiving end re-stamps `last_seen`
/// based on what *they* observed, so we keep that value rather than overwriting
/// it with our local clock.
fn merge_peer_into(peer: Peer, self_id: Uuid) -> Option<Peer> {
    if peer.node_id == self_id {
        return None;
    }
    Some(peer)
}

fn build_node_info(cluster: &Cluster) -> NodeInfo {
    NodeInfo {
        node_id:         cluster.node_id.to_string(),
        bind_url:        cluster.config.bind_url.clone(),
        version:         env!("CARGO_PKG_VERSION").to_string(),
        embedding_model: cluster.embedding_model.read().clone(),
        started_at:      cluster.started_at_unix(),
    }
}

/// Run the configured liveness sweep.  Caller invokes this on every tick.
pub fn sweep(table: &SharedPeerTable, suspect_after: Duration, dead_after: Duration) -> usize {
    table.write().sweep(suspect_after, dead_after)
}

/// One gossip tick: pick a random Alive peer, ping it, and (every 3rd tick)
/// pull its peer view to converge the membership table.  Tick number is
/// passed in by the caller so this function stays stateless.
pub async fn tick(
    cluster: &Arc<Cluster>,
    http:    &reqwest::Client,
    tick_no: u64,
) -> GossipTickResult {
    let cfg = &cluster.config;
    let target = cluster.peers.read().pick_random_alive();
    let target = match target {
        Some(p) => p,
        None    => return GossipTickResult::NoAlivePeer,
    };

    let timeout = Duration::from_secs(cfg.peer_rpc_timeout_secs);

    let ping_outcome = rpc_client::cluster_ping(http, &target.url, &cfg.shared_secret, timeout).await;
    if let Err(e) = ping_outcome {
        cluster.peers.write().record_miss(target.node_id);
        log::debug!("[cluster] ping {} failed: {e}", target.url);
        return GossipTickResult::PingFailed { peer: target.url, reason: e.to_string() };
    }
    cluster.peers.write().record_alive(target.node_id);

    // Every Nth tick, also exchange peer views so the table converges
    // even when no nodes are joining/leaving.
    let do_pull = tick_no % 3 == 0;
    if !do_pull {
        return GossipTickResult::Pinged { peer: target.url };
    }

    match rpc_client::cluster_peers(http, &target.url, &cfg.shared_secret, timeout).await {
        Ok(remote) => {
            let mut new_count = 0;
            {
                let mut t = cluster.peers.write();
                let self_id = t.self_id();
                for p in remote {
                    if let Some(p) = merge_peer_into(p, self_id) {
                        if t.upsert(p) { new_count += 1; }
                    }
                }
            }
            cluster.persist_peers_best_effort();
            GossipTickResult::Merged { peer: target.url, new_peers: new_count }
        }
        Err(e) => {
            log::debug!("[cluster] peers from {} failed: {e}", target.url);
            GossipTickResult::PeersFailed { peer: target.url, reason: e.to_string() }
        }
    }
}

/// One-shot bootstrap call: handshake with the configured bootstrap URL and
/// optionally with every persisted peer (in parallel).  Errors from individual
/// peers are logged but never propagated — bootstrap is best-effort.
pub async fn bootstrap(cluster: &Arc<Cluster>, http: &reqwest::Client) {
    let mut targets: Vec<String> = Vec::new();
    if let Some(b) = &cluster.config.bootstrap {
        targets.push(b.clone());
    }
    // Also try every persisted peer (gives us reconnect-after-restart even
    // when the configured bootstrap is down).
    for p in cluster.peers.read().snapshot() {
        if !targets.contains(&p.url) {
            targets.push(p.url);
        }
    }
    if targets.is_empty() {
        log::info!("[cluster] no bootstrap target — running standalone until peers are added");
        return;
    }

    let me   = build_node_info(cluster);
    let secret = cluster.config.shared_secret.clone();
    let timeout = Duration::from_secs(cluster.config.peer_rpc_timeout_secs);

    let mut joined = 0;
    for url in targets {
        match rpc_client::cluster_hello(http, &url, &secret, &me, timeout).await {
            Ok(resp) => {
                joined += 1;
                let remote_id = match Uuid::parse_str(&resp.node_id) {
                    Ok(u) => u,
                    Err(_) => { log::warn!("[cluster] hello from {url} returned invalid node_id"); continue; }
                };
                let mut t = cluster.peers.write();
                let self_id = t.self_id();
                let mut p = Peer::new(remote_id, resp.bind_url);
                p.state           = PeerState::Alive;
                p.last_seen       = now_secs();
                p.version         = resp.version;
                p.embedding_model = resp.embedding_model;
                p.started_at      = resp.started_at;
                t.upsert(p);
                for rp in resp.peers {
                    if let Some(rp) = merge_peer_into(rp, self_id) {
                        t.upsert(rp);
                    }
                }
            }
            Err(e) => {
                log::warn!("[cluster] hello {url}: {e}");
            }
        }
    }
    cluster.persist_peers_best_effort();
    log::info!("[cluster] bootstrap complete — joined {joined} peer(s); table now has {} entries",
               cluster.peers.read().len());
}

#[derive(Debug)]
pub enum GossipTickResult {
    /// No Alive peers — gossip skipped.
    NoAlivePeer,
    /// Ping succeeded but we did not pull peers this tick.
    Pinged       { peer: String },
    /// Ping + peer-list exchange completed; `new_peers` were added to the table.
    Merged       { peer: String, new_peers: usize },
    /// Ping failed (peer marked Suspect via miss bookkeeping).
    PingFailed   { peer: String, reason: String },
    /// Ping ok but the follow-up peers call failed.
    PeersFailed  { peer: String, reason: String },
}

/// Try to resurrect one non-Alive peer.  Pinged separately from the main
/// `tick` Alive-peer gossip so a node whose peers all went Dead has a way
/// out — without this, `pick_random_alive` returns None forever and
/// gossip never re-checks the Dead peers, leaving the node permanently
/// Standalone even after the others come back.
///
/// Returns `Some(peer_url)` when a peer transitioned back to Alive;
/// `None` when every peer is already Alive, no candidate exists, or the
/// probe failed.
pub async fn probe_recovery(
    cluster: &Arc<Cluster>,
    http:    &reqwest::Client,
) -> Option<String> {
    let target = cluster.peers.read().pick_random_non_alive()?;
    let cfg = &cluster.config;
    let timeout = Duration::from_secs(cfg.peer_rpc_timeout_secs);

    match rpc_client::cluster_ping(http, &target.url, &cfg.shared_secret, timeout).await {
        Ok(_) => {
            cluster.peers.write().record_alive(target.node_id);
            log::info!("[cluster] recovery probe: {} -> Alive (was {:?})", target.url, target.state);
            // Persist immediately so the next restart doesn't lose this transition.
            cluster.persist_peers_best_effort();
            Some(target.url)
        }
        Err(e) => {
            log::debug!("[cluster] recovery probe: {} still unreachable: {e}", target.url);
            None
        }
    }
}

/// Re-exported helper so `mod.rs` can call this from `Cluster::init`.
pub fn build_initial_table(self_id: Uuid, persisted: Vec<Peer>) -> SharedPeerTable {
    let mut t = PeerTable::new(self_id);
    for mut p in persisted {
        // Persisted peers come back in whatever state we last saw them in.
        // Reset the miss counter so we don't immediately demote them again.
        p.miss_count = 0;
        t.upsert(p);
    }
    Arc::new(RwLock::new(t))
}
