//! `v2/graph.*` — inter-node receiver + anti-entropy surface for the
//! fully-replicated relationship graph.
//!
//! These are **not** the client API (that's `v5/graph.*`).  They are
//! what the `v5/graph.*` coordinator fans out to and what the
//! anti-entropy sweep (`server/cluster.rs::sync_graph`) pulls from:
//!
//! - `v2/graph.apply.batch` — apply a replicated write batch
//!   **locally only** (no re-fan-out) via the cache+FTS-coherent
//!   `apply_*_lww` primitives.
//! - `v2/graph.list_ids` — cheap node + edge enumeration (natural key
//!   + LWW `updated_at`) plus tombstones, the AE diff input.
//! - `v2/graph.node.get` / `v2/graph.edge.get` — full-entity getters
//!   for the AE pull path.
//! - `v2/graph.fingerprint` — the cheap whole-store digest the AE
//!   sweep compares before doing a full diff.

use super::params::rpc_err;
use super::v5_graph::{blocking, edge_to_json, json_to_edge, json_to_node, node_to_json};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use uuid::Uuid;

pub fn register(module: &mut RpcModule<()>) {
    register_apply_batch(module);
    register_list_ids(module);
    register_node_get(module);
    register_edge_get(module);
    register_fingerprint(module);
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── v2/graph.apply.batch — replication receiver ──────────────────────────────

#[derive(Deserialize)]
struct NodeKeyP {
    node_type: String,
    ref_id:    String,
}

#[derive(Deserialize)]
struct RemovedEdgeP {
    from:      NodeKeyP,
    to:        NodeKeyP,
    edge_type: String,
}

#[derive(Deserialize)]
struct EdgeTypeP {
    name:             String,
    #[serde(default = "one")]  default_weight:   f64,
    #[serde(default = "tru")]  default_directed: bool,
    #[serde(default)]          attrs:            Option<JsonValue>,
}
fn one() -> f64 { 1.0 }
fn tru() -> bool { true }

#[derive(Deserialize, Default)]
struct ApplyBatchP {
    #[serde(default)] nodes:         Vec<JsonValue>,
    #[serde(default)] edges:         Vec<JsonValue>,
    #[serde(default)] removed_nodes: Vec<NodeKeyP>,
    #[serde(default)] removed_edges: Vec<RemovedEdgeP>,
    #[serde(default)] edge_types:    Vec<EdgeTypeP>,
}

fn register_apply_batch(module: &mut RpcModule<()>) {
    module.register_async_method("v2/graph.apply.batch", |params, _ctx, _| async move {
        let p: ApplyBatchP = params.parse()?;
        let summary = blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            use bdslib::graphstorage::NodeRef;
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let now = now_secs();

            // 1. Nodes first — edges may reference them.
            let mut nodes_applied = 0u64;
            for nv in &p.nodes {
                let node = json_to_node(nv)?;
                if db.graph_apply_node_lww(&node).map_err(|e| rpc_err(-32011, e))? {
                    nodes_applied += 1;
                }
            }
            // 2. Edges (LWW upsert — cache-coherent).
            let mut edges_applied = 0u64;
            for ev in &p.edges {
                let edge = json_to_edge(ev)?;
                if db.graph_apply_edge_lww(&edge).map_err(|e| rpc_err(-32011, e))? {
                    edges_applied += 1;
                }
            }
            // 3. Edge-type registry.
            for et in &p.edge_types {
                db.graph_register_edge_type(
                    &et.name, et.default_weight, et.default_directed,
                    et.attrs.clone().unwrap_or_else(|| json!({})),
                ).map_err(|e| rpc_err(-32011, e))?;
            }
            // 4. Removals — node removal cascades its edges; both kinds
            //    tombstone so a later anti-entropy round does not
            //    resurrect the deletion.
            let mut nodes_removed = 0u64;
            for rn in &p.removed_nodes {
                let nref = NodeRef::new(rn.node_type.clone(), rn.ref_id.clone());
                if db.graph_remove_node(&nref).map_err(|e| rpc_err(-32011, e))? {
                    nodes_removed += 1;
                    if let Some(c) = db.cluster() {
                        let id = bdslib::graphstorage::node_id_for(&rn.node_type, &rn.ref_id);
                        let _ = c.tombstones.mark_deleted("graph_nodes", id, now);
                    }
                }
            }
            let mut edges_removed = 0u64;
            for re in &p.removed_edges {
                let from = NodeRef::new(re.from.node_type.clone(), re.from.ref_id.clone());
                let to   = NodeRef::new(re.to.node_type.clone(), re.to.ref_id.clone());
                let ids = db.graph_unlink(&from, &to, &re.edge_type)
                    .map_err(|e| rpc_err(-32011, e))?;
                edges_removed += ids.len() as u64;
                if let Some(c) = db.cluster() {
                    for id in &ids {
                        let _ = c.tombstones.mark_deleted("graph_edges", *id, now);
                    }
                }
            }
            Ok(json!({
                "nodes_applied": nodes_applied,
                "edges_applied": edges_applied,
                "nodes_removed": nodes_removed,
                "edges_removed": edges_removed,
            }))
        }).await?;
        Ok::<JsonValue, ErrorObject>(summary)
    }).unwrap();
}

// ── v2/graph.list_ids — anti-entropy enumeration ─────────────────────────────

fn register_list_ids(module: &mut RpcModule<()>) {
    module.register_async_method("v2/graph.list_ids", |_params, _ctx, _| async move {
        let out = blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let nodes = db.graph_node_summaries().map_err(|e| rpc_err(-32011, e))?;
            let edges = db.graph_edge_summaries().map_err(|e| rpc_err(-32011, e))?;

            let node_arr: Vec<JsonValue> = nodes.iter().map(|n| json!({
                "id":         n.id.to_string(),
                "node_type":  n.node_type,
                "ref_id":     n.ref_id,
                "updated_at": n.updated_at,
            })).collect();
            let edge_arr: Vec<JsonValue> = edges.iter().map(|e| json!({
                "id":         e.id.to_string(),
                "src":        e.src.to_string(),
                "dst":        e.dst.to_string(),
                "edge_type":  e.edge_type,
                "valid_from": e.valid_from,
                "updated_at": e.updated_at,
            })).collect();

            // Tombstones — empty when cluster mode is off.
            let tombs = |store: &str| -> Result<Vec<JsonValue>, ErrorObject<'static>> {
                match db.cluster() {
                    Some(c) => Ok(c.tombstones.list_for_store(store)
                        .map_err(|e| rpc_err(-32011, e))?
                        .into_iter()
                        .map(|t| json!({ "id": t.id.to_string(), "deleted_at": t.deleted_at }))
                        .collect()),
                    None => Ok(Vec::new()),
                }
            };

            Ok(json!({
                "n_nodes":         node_arr.len(),
                "n_edges":         edge_arr.len(),
                "nodes":           node_arr,
                "edges":           edge_arr,
                "node_tombstones": tombs("graph_nodes")?,
                "edge_tombstones": tombs("graph_edges")?,
            }))
        }).await?;
        Ok::<JsonValue, ErrorObject>(out)
    }).unwrap();
}

// ── v2/graph.node.get / v2/graph.edge.get — AE full-entity getters ───────────

#[derive(Deserialize)]
struct IdP {
    id: String,
}

fn register_node_get(module: &mut RpcModule<()>) {
    module.register_async_method("v2/graph.node.get", |params, _ctx, _| async move {
        let p: IdP = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let node = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_get_node_by_id(&id).map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(match node {
            Some(n) => json!({ "found": true,  "node": node_to_json(&n) }),
            None    => json!({ "found": false, "node": JsonValue::Null }),
        })
    }).unwrap();
}

fn register_edge_get(module: &mut RpcModule<()>) {
    module.register_async_method("v2/graph.edge.get", |params, _ctx, _| async move {
        let p: IdP = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let edge = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_get_edge(&id).map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(match edge {
            Some(e) => json!({ "found": true,  "edge": edge_to_json(&e) }),
            None    => json!({ "found": false, "edge": JsonValue::Null }),
        })
    }).unwrap();
}

// ── v2/graph.fingerprint — cheap AE divergence probe ─────────────────────────

fn register_fingerprint(module: &mut RpcModule<()>) {
    module.register_async_method("v2/graph.fingerprint", |_params, _ctx, _| async move {
        let f = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_fingerprint().map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(json!({
            "node_count": f.node_count,
            "edge_count": f.edge_count,
            "nodes_hash": f.nodes_hash,
            "edges_hash": f.edges_hash,
        }))
    }).unwrap();
}
