//! `v3/add.batch` — replicated batch write.
//!
//! Same fire-and-forget + hinted-handoff recipe as [`v3/add`](v3_add.rs),
//! but the local write goes through `ShardsManager::add_batch` and the
//! fan-out preserves every UUID.  All replicas see the **same** batch
//! call (single round-trip per peer) — much cheaper than N independent
//! `v3/add` invocations for high-volume ingest.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::{fanout::FanOutResults, replication};
use bdslib::ClusterMode;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct Params {
    docs: Vec<JsonValue>,
    #[serde(default)]
    replication_factor: Option<usize>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v3/add.batch", |params, _ctx, _| async move {
            log::debug!("v3/add.batch: start");
            let p: Params = params.parse()?;
            if p.docs.is_empty() {
                return Ok::<JsonValue, ErrorObject>(serde_json::json!({
                    "ids": [],
                    "replicas_dispatched": 0,
                    "mode": "standalone",
                }));
            }

            // Pre-assign UUIDs (or accept caller-supplied ids) so every
            // replica writes under the same identity.
            let mut docs = p.docs;
            for d in docs.iter_mut() {
                let id = match d.get("id").and_then(|v| v.as_str()) {
                    Some(s) => Uuid::parse_str(s)
                        .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?,
                    None => Uuid::now_v7(),
                };
                d["id"] = JsonValue::String(id.to_string());
            }

            // Local batch write.
            let local_docs = docs.clone();
            let local_ids = tokio::task::spawn_blocking(move || {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                db.add_batch(local_docs).map_err(|e| rpc_err(-32004, e))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

            // The local add_batch may have collapsed some duplicates onto
            // existing UUIDs.  Rewrite the docs we replicate so peers
            // receive the same final identities.
            for (d, id) in docs.iter_mut().zip(local_ids.iter()) {
                d["id"] = JsonValue::String(id.to_string());
            }

            let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
            let (mode, n_dispatched, n_alive, rf_effective) = match &cluster {
                Some(c) => {
                    let rf = p.replication_factor.unwrap_or(c.config.replication_factor).max(1);
                    let need = rf.saturating_sub(1);
                    let replicas = replication::pick_random_alive(c, need);
                    let n_alive  = c.peers.read().alive_count();
                    let n_disp   = replicas.len();
                    let mode     = c.mode();
                    if !replicas.is_empty() {
                        let cluster_t = c.clone();
                        let docs_t = docs.clone();
                        tokio::spawn(async move {
                            replicate_batch_to_peers(cluster_t, replicas, docs_t).await;
                        });
                    }
                    (mode, n_disp, n_alive, rf)
                }
                None => (ClusterMode::Standalone, 0, 0, 1),
            };

            let under_replicated = n_dispatched + 1 < rf_effective;
            let ids_str: Vec<JsonValue> = local_ids.iter()
                .map(|u| JsonValue::String(u.to_string()))
                .collect();

            log::debug!("v3/add.batch: done n={} dispatched={n_dispatched}", ids_str.len());
            Ok::<JsonValue, ErrorObject>(serde_json::json!({
                "ids":                 ids_str,
                "n":                   ids_str.len(),
                "replication_factor":  rf_effective,
                "replicas_dispatched": n_dispatched,
                "alive_peers":         n_alive,
                "under_replicated":    under_replicated,
                "mode":                mode.as_str(),
                "cluster_meta":        v3_cluster_meta(None::<FanOutResults>),
            }))
        })
        .unwrap();
}

async fn replicate_batch_to_peers(
    cluster: std::sync::Arc<bdslib::Cluster>,
    replicas: Vec<bdslib::cluster::Peer>,
    docs: Vec<JsonValue>,
) {
    let v2_params = serde_json::json!({ "docs": docs });
    let canonical = match serde_json::to_vec(&v2_params) {
        Ok(b)  => b,
        Err(e) => { log::warn!("[v3/add.batch] serialize hint payload: {e}"); return; }
    };

    let mut joins = Vec::with_capacity(replicas.len());
    for peer in replicas {
        let cluster = cluster.clone();
        let params  = v2_params.clone();
        let bytes   = canonical.clone();
        joins.push(tokio::spawn(async move {
            match replication::call_peer_v2(&cluster, &peer.url, "v2/add.batch", &params).await {
                Ok(_) => log::debug!("[v3/add.batch] replicated to {} ok", peer.url),
                Err(e) => {
                    log::warn!("[v3/add.batch] replication to {} failed: {e}; hinting", peer.url);
                    if let Err(err) = cluster.hints.enqueue(peer.node_id, "v2/add.batch", &bytes) {
                        log::error!("[v3/add.batch] enqueue hint for {}: {err}", peer.url);
                    }
                }
            }
        }));
    }
    for j in joins { let _ = j.await; }
}
