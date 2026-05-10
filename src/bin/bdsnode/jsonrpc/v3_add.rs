//! `v3/add` — replicated single-document write.
//!
//! Coordinator algorithm (fire-and-forget + hinted handoff):
//!
//! 1. Generate a UUIDv7 (or accept the caller's `id` for retries — same
//!    UUID = idempotent replication).
//! 2. Inject the UUID into the doc as `id` so every replica writes it
//!    under the same identity.
//! 3. Write locally via `ShardsManager::add` (sync, on a blocking thread).
//! 4. Pick `replication_factor - 1` random Alive peers.
//! 5. Spawn a **detached** tokio task: try `v2/add` against each replica
//!    with `cluster.peer_rpc_timeout`; on failure (transport, timeout,
//!    server error) enqueue a hint for that peer.  The replay task in
//!    `bdsnode/server/cluster.rs` retries hints whenever the peer's state
//!    transitions Alive again.
//! 6. Return immediately with `{id, replicas_dispatched, mode, …}`.
//!
//! Local-write failure is the only thing that fails the call — that's the
//! durability guarantee the client gets.  Replica failures degrade
//! availability silently (and surface in the `cluster.hint_backlog`
//! telemetry once we ship phase-3 dashboards).

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::{fanout::FanOutResults, replication};
use bdslib::ClusterMode;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct Params {
    /// Telemetry document.  Must contain `timestamp`, `key`, `data`; any
    /// supplied `id` is preserved (used by retries to keep replication
    /// idempotent).
    doc: JsonValue,
    /// Override the cluster's configured `replication_factor`.  Clamped
    /// at runtime to `min(rf, alive_peers + 1)`.
    #[serde(default)]
    replication_factor: Option<usize>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v3/add", |params, _ctx, _| async move {
            log::debug!("v3/add: start");
            let p: Params = params.parse()?;

            // Resolve the UUID that every replica will share.
            let id = match p.doc.get("id").and_then(|v| v.as_str()) {
                Some(s) => Uuid::parse_str(s)
                    .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?,
                None    => Uuid::now_v7(),
            };
            let mut doc = p.doc;
            // Always pin the UUID — even when caller supplied one — so
            // downstream code can rely on `doc["id"]` being present.
            doc["id"] = JsonValue::String(id.to_string());

            // Local write on a blocking thread.
            let local_doc = doc.clone();
            let local_id = tokio::task::spawn_blocking(move || {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                db.add(local_doc).map_err(|e| rpc_err(-32004, e))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

            // The local v2-style dedup might collapse a duplicate (key+data)
            // back onto an existing UUID; in that case we replicate the
            // *existing* UUID and surface it to the caller.
            let id_to_replicate = local_id;
            doc["id"] = JsonValue::String(id_to_replicate.to_string());

            // Fan-out planning.
            let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
            let (mode, n_dispatched, n_alive, rf_effective) = match &cluster {
                Some(c) => {
                    let rf = p.replication_factor.unwrap_or(c.config.replication_factor).max(1);
                    let need = rf.saturating_sub(1);  // -1 because we already wrote locally
                    let replicas = replication::pick_random_alive(c, need);
                    let n_alive  = c.peers.read().alive_count();
                    let n_disp   = replicas.len();
                    let mode     = c.mode();

                    if !replicas.is_empty() {
                        // Detached: client doesn't wait.
                        let cluster_t = c.clone();
                        let doc_t = doc.clone();
                        tokio::spawn(async move {
                            replicate_to_peers(cluster_t, replicas, doc_t).await;
                        });
                    }
                    (mode, n_disp, n_alive, rf)
                }
                None => (ClusterMode::Standalone, 0, 0, 1),
            };

            let under_replicated = n_dispatched + 1 < rf_effective;

            log::debug!("v3/add: done id={id_to_replicate} dispatched={n_dispatched}");
            Ok::<JsonValue, ErrorObject>(serde_json::json!({
                "id":                  id_to_replicate.to_string(),
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

/// Try to replicate `doc` to each peer in `replicas`.  Failures are
/// enqueued as hints so the replay task picks them up later.  This is
/// fire-and-forget from the client's perspective.
async fn replicate_to_peers(
    cluster: std::sync::Arc<bdslib::Cluster>,
    replicas: Vec<bdslib::cluster::Peer>,
    doc: JsonValue,
) {
    let v2_params = serde_json::json!({ "doc": doc, "sync": true });
    let canonical = match serde_json::to_vec(&v2_params) {
        Ok(b)  => b,
        Err(e) => { log::warn!("[v3/add] serialize hint payload: {e}"); return; }
    };

    let mut joins = Vec::with_capacity(replicas.len());
    for peer in replicas {
        let cluster = cluster.clone();
        let params  = v2_params.clone();
        let bytes   = canonical.clone();
        joins.push(tokio::spawn(async move {
            match replication::call_peer_v2(&cluster, &peer.url, "v2/add", &params).await {
                Ok(_) => {
                    log::debug!("[v3/add] replicated to {} ok", peer.url);
                }
                Err(e) => {
                    log::warn!("[v3/add] replication to {} failed: {e}; hinting", peer.url);
                    if let Err(err) = cluster.hints.enqueue(peer.node_id, "v2/add", &bytes) {
                        log::error!("[v3/add] enqueue hint for {}: {err}", peer.url);
                    }
                }
            }
        }));
    }
    for j in joins { let _ = j.await; }
}
