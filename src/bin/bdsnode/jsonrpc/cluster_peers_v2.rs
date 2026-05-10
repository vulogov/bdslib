//! `v2/cluster.peers` — unauthenticated read of the local peer table.
//!
//! Same data as `v3/cluster.peers` but without the HMAC requirement, so
//! local trusted clients (bdsweb, bdscmd `cluster status`-without-secret,
//! observability) can render the cluster page without leaking the shared
//! secret into the web tier.  Returns an empty list when cluster mode is
//! disabled on this node.

use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

use super::params::rpc_err;

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v2/cluster.peers", |_params, _ctx, _| async move {
        let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
        let value = match db.cluster() {
            Some(c) => {
                let snap = c.peers.read().snapshot();
                let (alive, suspect, dead) = c.peers.read().count_by_state();
                let hint_backlog = c.hints.len().unwrap_or(0);
                let hints_per_peer: std::collections::HashMap<String, u64> = c.hints
                    .count_per_peer()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(id, n)| (id.to_string(), n))
                    .collect();
                let tombstone_total = c.tombstones.len().unwrap_or(0);
                let stats = c.stats.read().clone();

                // Augment each peer entry with its hint count.
                let peers: Vec<serde_json::Value> = snap.iter().map(|p| {
                    let n = hints_per_peer.get(&p.node_id.to_string()).copied().unwrap_or(0);
                    serde_json::json!({
                        "node_id":         p.node_id.to_string(),
                        "url":             p.url,
                        "last_seen":       p.last_seen,
                        "state":           p.state.as_str(),
                        "version":         p.version,
                        "embedding_model": p.embedding_model,
                        "started_at":      p.started_at,
                        "miss_count":      p.miss_count,
                        "hints":           n,
                    })
                }).collect();

                serde_json::json!({
                    "enabled":  true,
                    "node_id":  c.node_id.to_string(),
                    "bind_url": c.config.bind_url,
                    "mode":     c.mode().as_str(),
                    "alive":    alive,
                    "suspect":  suspect,
                    "dead":     dead,
                    "full_mode_threshold": c.config.full_mode_threshold,
                    "replication_factor":  c.config.replication_factor,
                    "embedding_model":     c.embedding_model.read().clone(),
                    "uptime_secs":         c.uptime().as_secs(),
                    "hint_backlog":        hint_backlog,
                    "tombstone_total":     tombstone_total,
                    "stats": serde_json::json!({
                        "last_hint_tick":           stats.last_hint_tick,
                        "last_hint_tick_replayed":  stats.last_hint_tick_replayed,
                        "last_ae_tick":             stats.last_ae_tick,
                        "last_ae_tick_pulled":      stats.last_ae_tick_pulled,
                        "last_ae_tick_tombstones":  stats.last_ae_tick_tombstones,
                        "last_ae_tick_pruned":      stats.last_ae_tick_pruned,
                    }),
                    "peers":               peers,
                })
            }
            None => serde_json::json!({
                "enabled": false,
                "peers":   [],
            }),
        };
        Ok::<serde_json::Value, ErrorObject>(value)
    }).unwrap();
}
