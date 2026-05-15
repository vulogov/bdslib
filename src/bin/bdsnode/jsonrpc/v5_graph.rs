//! `v5/graph.*` — JSON-RPC surface for the relationship graph of the
//! global `ShardsManager` ([`bdslib::graphstorage::GraphStore`]).
//!
//! ## Layering
//!
//! - **Reads** (`node.get`, `outgoing`, `traverse`, `search`, `stats`,
//!   `verify`, `fingerprint`, …) are served from the local replica.
//!   The graph is a fully-replicated cluster store, so a local read
//!   covers the whole cluster — no fan-out.
//! - **Writes** (`node.add`, `link`, `unlink`, `expire`, …) commit
//!   locally, then fan out to every Alive peer's `v2/graph.apply.batch`
//!   receiver via `replicate_to_all` (hint-on-failure).  Every write
//!   response carries an `outcome` block (peers attempted / succeeded /
//!   hints queued).
//! - **Maintenance** (`repair`, `rebuild_fts`, `sync`) is per-node
//!   local — like `v2/retention.sweep`.
//!
//! The matching receiver + anti-entropy surface is `v2/graph.*` in
//! `v2_graph.rs`; the AE sweep itself is `server/cluster.rs::sync_graph`.

use super::params::rpc_err;
use super::v3_replicated::replicate_to_all;
use bdslib::graphstorage::{
    Direction, Edge, EdgeFilter, EdgeSpec, GraphRepairOpts, Node, NodeRef, Strategy, TimeScope,
    TraversalOpts,
};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use uuid::Uuid;

pub fn register(module: &mut RpcModule<()>) {
    // reads
    register_node_get(module);
    register_outgoing(module);
    register_incoming(module);
    register_neighbors(module);
    register_degree(module);
    register_traverse(module);
    register_reachable(module);
    register_shortest_path(module);
    register_search(module);
    register_search_typed(module);
    register_stats(module);
    register_verify(module);
    register_fingerprint(module);
    // writes (replicated)
    register_node_add(module);
    register_node_remove(module);
    register_node_group_add(module);
    register_link(module);
    register_link_batch(module);
    register_unlink(module);
    register_expire(module);
    register_set_weight(module);
    register_edge_type_register(module);
    // maintenance (local)
    register_repair(module);
    register_rebuild_fts(module);
    register_sync(module);
}

// ── shared helpers ───────────────────────────────────────────────────────────

/// Run a blocking `ShardsManager` graph call on the blocking pool.
pub(super) async fn blocking<F, T>(f: F) -> Result<T, ErrorObject<'static>>
where
    F: FnOnce() -> Result<T, ErrorObject<'static>> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?
}

/// Replicate a `v2/graph.apply.batch` payload to every Alive peer.
/// Returns the fan-out outcome as JSON (empty when cluster mode is off).
async fn fan_out(payload: JsonValue) -> JsonValue {
    let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
    match cluster {
        Some(c) => replicate_to_all(c, "v2/graph.apply.batch", payload).await.to_json(),
        None => json!({ "peers_attempted": 0, "peers_succeeded": 0, "hints_queued": 0 }),
    }
}

// ── (de)serialisation of graph types ─────────────────────────────────────────

pub(super) fn node_to_json(n: &Node) -> JsonValue {
    json!({
        "id":         n.id.to_string(),
        "node_type":  n.node_type,
        "ref_id":     n.ref_id,
        "attrs":      n.attrs,
        "created_at": n.created_at,
        "updated_at": n.updated_at,
    })
}

pub(super) fn edge_to_json(e: &Edge) -> JsonValue {
    json!({
        "id":         e.id.to_string(),
        "src":        e.src.to_string(),
        "dst":        e.dst.to_string(),
        "edge_type":  e.edge_type,
        "weight":     e.weight,
        "directed":   e.directed,
        "attrs":      e.attrs,
        "valid_from": e.valid_from,
        "valid_to":   e.valid_to,
        "created_at": e.created_at,
        "updated_at": e.updated_at,
    })
}

/// Parse a `Node` from the JSON shape produced by [`node_to_json`].
pub(super) fn json_to_node(v: &JsonValue) -> Result<Node, ErrorObject<'static>> {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let i = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    Ok(Node {
        id: Uuid::parse_str(s("id")).map_err(|e| rpc_err(-32602, format!("bad node id: {e}")))?,
        node_type: s("node_type").to_owned(),
        ref_id: s("ref_id").to_owned(),
        attrs: v.get("attrs").cloned().unwrap_or_else(|| json!({})),
        created_at: i("created_at"),
        updated_at: i("updated_at"),
    })
}

/// Parse an `Edge` from the JSON shape produced by [`edge_to_json`].
pub(super) fn json_to_edge(v: &JsonValue) -> Result<Edge, ErrorObject<'static>> {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let i = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let uuid = |k: &str| {
        Uuid::parse_str(s(k)).map_err(|e| rpc_err(-32602, format!("bad edge {k}: {e}")))
    };
    Ok(Edge {
        id: uuid("id")?,
        src: uuid("src")?,
        dst: uuid("dst")?,
        edge_type: s("edge_type").to_owned(),
        weight: v.get("weight").and_then(|x| x.as_f64()).unwrap_or(1.0),
        directed: v.get("directed").and_then(|x| x.as_bool()).unwrap_or(true),
        attrs: v.get("attrs").cloned().unwrap_or_else(|| json!({})),
        valid_from: i("valid_from"),
        valid_to: i("valid_to"),
        created_at: i("created_at"),
        updated_at: i("updated_at"),
    })
}

// ── param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct NodeRefP {
    node_type: String,
    ref_id:    String,
}
impl NodeRefP {
    fn to_ref(&self) -> NodeRef {
        NodeRef::new(self.node_type.clone(), self.ref_id.clone())
    }
}

#[derive(Deserialize)]
struct EdgeSpecP {
    edge_type: String,
    #[serde(default)] weight:     Option<f64>,
    #[serde(default)] directed:   Option<bool>,
    #[serde(default)] attrs:      Option<JsonValue>,
    #[serde(default)] valid_from: Option<i64>,
    #[serde(default)] valid_to:   Option<i64>,
}
impl EdgeSpecP {
    fn to_spec(&self) -> EdgeSpec {
        let mut s = EdgeSpec::new(self.edge_type.clone());
        if let Some(w) = self.weight { s = s.weight(w); }
        if let Some(d) = self.directed { s = s.directed(d); }
        if let Some(a) = &self.attrs { s = s.attrs(a.clone()); }
        s.valid_from = self.valid_from;
        s.valid_to = self.valid_to;
        s
    }
}

#[derive(Deserialize, Default)]
struct EdgeFilterP {
    #[serde(default)] edge_types: Option<Vec<String>>,
    #[serde(default)] min_weight: Option<f64>,
    #[serde(default)] limit:      Option<usize>,
    /// `TimeScope::At(t)` when set.
    #[serde(default)] at:         Option<i64>,
    /// `TimeScope::Overlap(a, b)` when set (takes precedence behind `at`).
    #[serde(default)] overlap:    Option<[i64; 2]>,
}
impl EdgeFilterP {
    fn to_filter(&self) -> EdgeFilter {
        let mut f = EdgeFilter::new();
        f.edge_types = self.edge_types.clone();
        f.min_weight = self.min_weight;
        f.limit = self.limit;
        f.time = match (self.at, self.overlap) {
            (Some(t), _)        => TimeScope::At(t),
            (None, Some([a, b])) => TimeScope::Overlap(a, b),
            _                   => TimeScope::All,
        };
        f
    }
}

fn parse_direction(s: &str) -> Result<Direction, ErrorObject<'static>> {
    match s {
        "out"  => Ok(Direction::Out),
        "in"   => Ok(Direction::In),
        "both" => Ok(Direction::Both),
        other  => Err(rpc_err(-32602, format!("direction must be out|in|both, got {other:?}"))),
    }
}

#[derive(Deserialize)]
struct TraversalP {
    #[serde(default = "default_dir")] direction: String,
    max_depth: usize,
    max_nodes: usize,
    #[serde(default)] strategy:    Option<String>,
    #[serde(default)] edge_filter: EdgeFilterP,
}
fn default_dir() -> String { "out".to_owned() }
impl TraversalP {
    fn to_opts(&self) -> Result<TraversalOpts, ErrorObject<'static>> {
        let strategy = match self.strategy.as_deref() {
            Some("dijkstra") => Strategy::Dijkstra,
            Some("bfs") | None => Strategy::Bfs,
            Some(o) => return Err(rpc_err(-32602, format!("strategy must be bfs|dijkstra, got {o:?}"))),
        };
        Ok(TraversalOpts {
            direction:   parse_direction(&self.direction)?,
            max_depth:   self.max_depth,
            max_nodes:   self.max_nodes,
            edge_filter: self.edge_filter.to_filter(),
            strategy,
        })
    }
}

// ── reads ────────────────────────────────────────────────────────────────────

fn register_node_get(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.node.get", |params, _ctx, _| async move {
        let p: NodeRefP = params.parse()?;
        let node = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_get_node(&p.to_ref()).map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(match node {
            Some(n) => json!({ "found": true, "node": node_to_json(&n) }),
            None    => json!({ "found": false, "node": JsonValue::Null }),
        })
    }).unwrap();
}

fn register_outgoing(module: &mut RpcModule<()>) {
    register_dir_edges(module, "v5/graph.outgoing", Direction::Out);
}
fn register_incoming(module: &mut RpcModule<()>) {
    register_dir_edges(module, "v5/graph.incoming", Direction::In);
}

#[derive(Deserialize)]
struct EdgesP {
    node_type: String,
    ref_id:    String,
    #[serde(default)] filter: EdgeFilterP,
}

fn register_dir_edges(module: &mut RpcModule<()>, method: &'static str, dir: Direction) {
    module.register_async_method(method, move |params, _ctx, _| async move {
        let p: EdgesP = params.parse()?;
        let edges = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let nref = NodeRef::new(p.node_type, p.ref_id);
            let f = p.filter.to_filter();
            match dir {
                Direction::Out => db.graph_outgoing(&nref, &f),
                _              => db.graph_incoming(&nref, &f),
            }.map_err(|e| rpc_err(-32011, e))
        }).await?;
        let arr: Vec<JsonValue> = edges.iter().map(edge_to_json).collect();
        Ok::<JsonValue, ErrorObject>(json!({ "count": arr.len(), "edges": arr }))
    }).unwrap();
}

#[derive(Deserialize)]
struct NeighborsP {
    node_type: String,
    ref_id:    String,
    #[serde(default = "default_dir")] direction: String,
    #[serde(default)] filter: EdgeFilterP,
}

fn register_neighbors(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.neighbors", |params, _ctx, _| async move {
        let p: NeighborsP = params.parse()?;
        let dir = parse_direction(&p.direction)?;
        let nodes = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let nref = NodeRef::new(p.node_type, p.ref_id);
            db.graph_neighbors(&nref, dir, &p.filter.to_filter())
                .map_err(|e| rpc_err(-32011, e))
        }).await?;
        let arr: Vec<JsonValue> = nodes.iter().map(node_to_json).collect();
        Ok::<JsonValue, ErrorObject>(json!({ "count": arr.len(), "nodes": arr }))
    }).unwrap();
}

#[derive(Deserialize)]
struct DegreeP {
    node_type: String,
    ref_id:    String,
    #[serde(default = "default_dir")] direction: String,
}

fn register_degree(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.degree", |params, _ctx, _| async move {
        let p: DegreeP = params.parse()?;
        let dir = parse_direction(&p.direction)?;
        let degree = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_degree(&NodeRef::new(p.node_type, p.ref_id), dir)
                .map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(json!({ "degree": degree }))
    }).unwrap();
}

#[derive(Deserialize)]
struct TraverseP {
    start: NodeRefP,
    #[serde(flatten)] opts: TraversalP,
}

fn register_traverse(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.traverse", |params, _ctx, _| async move {
        let p: TraverseP = params.parse()?;
        let opts = p.opts.to_opts()?;
        let start = p.start.to_ref();
        let hits = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_traverse(&start, &opts).map_err(|e| rpc_err(-32011, e))
        }).await?;
        let arr: Vec<JsonValue> = hits.iter().map(|h| json!({
            "node":      node_to_json(&h.node),
            "depth":     h.depth,
            "path_cost": h.path_cost,
        })).collect();
        Ok::<JsonValue, ErrorObject>(json!({ "count": arr.len(), "hits": arr }))
    }).unwrap();
}

fn register_reachable(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.reachable", |params, _ctx, _| async move {
        let p: TraverseP = params.parse()?;
        let opts = p.opts.to_opts()?;
        let start = p.start.to_ref();
        let refs = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_reachable(&start, &opts).map_err(|e| rpc_err(-32011, e))
        }).await?;
        let arr: Vec<JsonValue> = refs.iter()
            .map(|r| json!({ "node_type": r.node_type, "ref_id": r.ref_id }))
            .collect();
        Ok::<JsonValue, ErrorObject>(json!({ "count": arr.len(), "nodes": arr }))
    }).unwrap();
}

#[derive(Deserialize)]
struct ShortestPathP {
    from: NodeRefP,
    to:   NodeRefP,
    #[serde(flatten)] opts: TraversalP,
}

fn register_shortest_path(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.shortest_path", |params, _ctx, _| async move {
        let p: ShortestPathP = params.parse()?;
        let opts = p.opts.to_opts()?;
        let (from, to) = (p.from.to_ref(), p.to.to_ref());
        let path = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_shortest_path(&from, &to, &opts).map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(match path {
            Some(pa) => json!({
                "found":        true,
                "nodes":        pa.nodes.iter().map(node_to_json).collect::<Vec<_>>(),
                "edges":        pa.edges.iter().map(edge_to_json).collect::<Vec<_>>(),
                "total_weight": pa.total_weight,
            }),
            None => json!({ "found": false }),
        })
    }).unwrap();
}

#[derive(Deserialize)]
struct SearchP {
    query: String,
    #[serde(default = "default_limit")] limit: usize,
}
fn default_limit() -> usize { 20 }

fn register_search(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.search", |params, _ctx, _| async move {
        let p: SearchP = params.parse()?;
        let hits = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_search_nodes(&p.query, p.limit).map_err(|e| rpc_err(-32011, e))
        }).await?;
        let arr: Vec<JsonValue> = hits.iter()
            .map(|(n, score)| json!({ "node": node_to_json(n), "score": score }))
            .collect();
        Ok::<JsonValue, ErrorObject>(json!({ "count": arr.len(), "results": arr }))
    }).unwrap();
}

#[derive(Deserialize)]
struct SearchTypedP {
    query: String,
    types: Vec<String>,
    #[serde(default = "default_limit")] limit: usize,
}

fn register_search_typed(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.search.typed", |params, _ctx, _| async move {
        let p: SearchTypedP = params.parse()?;
        let hits = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let types: Vec<&str> = p.types.iter().map(String::as_str).collect();
            db.graph_search_nodes_typed(&p.query, &types, p.limit)
                .map_err(|e| rpc_err(-32011, e))
        }).await?;
        let arr: Vec<JsonValue> = hits.iter()
            .map(|(n, score)| json!({ "node": node_to_json(n), "score": score }))
            .collect();
        Ok::<JsonValue, ErrorObject>(json!({ "count": arr.len(), "results": arr }))
    }).unwrap();
}

fn register_stats(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.stats", |_params, _ctx, _| async move {
        let s = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_stats().map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(json!({
            "node_count":      s.node_count,
            "edge_count":      s.edge_count,
            "edge_type_count": s.edge_type_count,
            "fts_doc_count":   s.fts_doc_count,
            "cache": {
                "resolve_hits":   s.cache.resolve_hits,
                "resolve_misses": s.cache.resolve_misses,
                "node_hits":      s.cache.node_hits,
                "node_misses":    s.cache.node_misses,
                "adj_hits":       s.cache.adj_hits,
                "adj_misses":     s.cache.adj_misses,
            },
        }))
    }).unwrap();
}

fn register_verify(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.verify", |_params, _ctx, _| async move {
        let r = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_verify().map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(json!({
            "healthy":                r.healthy,
            "node_count":             r.node_count,
            "edge_count":             r.edge_count,
            "dangling_edges":         r.dangling_edges,
            "invalid_temporal_edges": r.invalid_temporal_edges,
            "fts_doc_count":          r.fts_doc_count,
            "fts_drift":              r.fts_drift,
        }))
    }).unwrap();
}

fn register_fingerprint(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.fingerprint", |_params, _ctx, _| async move {
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

// ── writes (commit local, fan out to peers) ──────────────────────────────────

#[derive(Deserialize)]
struct NodeAddP {
    node_type: String,
    ref_id:    String,
    #[serde(default)] attrs: Option<JsonValue>,
}

fn register_node_add(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.node.add", |params, _ctx, _| async move {
        let p: NodeAddP = params.parse()?;
        let node = blocking(move || -> Result<Node, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let nref = NodeRef::new(p.node_type, p.ref_id);
            let id = db.graph_add_node(&nref, p.attrs.unwrap_or_else(|| json!({})))
                .map_err(|e| rpc_err(-32011, e))?;
            db.graph_get_node_by_id(&id).map_err(|e| rpc_err(-32011, e))?
                .ok_or_else(|| rpc_err(-32011, "node vanished after add"))
        }).await?;
        let id = node.id.to_string();
        let outcome = fan_out(json!({ "nodes": [node_to_json(&node)] })).await;
        Ok::<JsonValue, ErrorObject>(json!({ "id": id, "outcome": outcome }))
    }).unwrap();
}

fn register_node_remove(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.node.remove", |params, _ctx, _| async move {
        let p: NodeRefP = params.parse()?;
        let nref = p.to_ref();
        let removed = blocking({
            let nref = nref.clone();
            move || -> Result<bool, ErrorObject<'static>> {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let removed = db.graph_remove_node(&nref).map_err(|e| rpc_err(-32011, e))?;
                // Tombstone the node so anti-entropy propagates the delete.
                if removed {
                    if let Some(c) = db.cluster() {
                        let id = bdslib::graphstorage::node_id_for(&nref.node_type, &nref.ref_id);
                        let _ = c.tombstones.mark_deleted("graph_nodes", id, now_secs() as i64);
                    }
                }
                Ok(removed)
            }
        }).await?;
        let outcome = fan_out(json!({
            "removed_nodes": [{ "node_type": nref.node_type, "ref_id": nref.ref_id }]
        })).await;
        Ok::<JsonValue, ErrorObject>(json!({ "removed": removed, "outcome": outcome }))
    }).unwrap();
}

#[derive(Deserialize)]
struct NodeGroupAddP {
    group:       NodeRefP,
    #[serde(default)] group_attrs: Option<JsonValue>,
    members:     Vec<NodeRefP>,
    member_edge: String,
}

fn register_node_group_add(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.node.group.add", |params, _ctx, _| async move {
        let p: NodeGroupAddP = params.parse()?;
        // Local write, then read back every affected node + edge for fan-out.
        let (group_id, nodes, edges) = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let group = p.group.to_ref();
            let members: Vec<NodeRef> = p.members.iter().map(NodeRefP::to_ref).collect();
            let gid = db.graph_add_node_group(
                &group, p.group_attrs.unwrap_or_else(|| json!({})), &members, &p.member_edge,
            ).map_err(|e| rpc_err(-32011, e))?;

            let mut nodes: Vec<Node> = Vec::with_capacity(members.len() + 1);
            if let Some(n) = db.graph_get_node_by_id(&gid).map_err(|e| rpc_err(-32011, e))? {
                nodes.push(n);
            }
            let mut edges: Vec<Edge> = Vec::new();
            for m in &members {
                if let Some(n) = db.graph_get_node(m).map_err(|e| rpc_err(-32011, e))? {
                    nodes.push(n);
                }
                // the group → member edges, filtered to the member-edge type
                let f = EdgeFilter::new().edge_type(p.member_edge.clone());
                for e in db.graph_outgoing(&group, &f).map_err(|e| rpc_err(-32011, e))? {
                    edges.push(e);
                }
            }
            // de-dup edges (graph_outgoing was queried once per member)
            edges.sort_by_key(|e| e.id);
            edges.dedup_by_key(|e| e.id);
            Ok::<_, ErrorObject<'static>>((gid.to_string(), nodes, edges))
        }).await?;

        let payload = json!({
            "nodes": nodes.iter().map(node_to_json).collect::<Vec<_>>(),
            "edges": edges.iter().map(edge_to_json).collect::<Vec<_>>(),
        });
        let outcome = fan_out(payload).await;
        Ok::<JsonValue, ErrorObject>(json!({ "id": group_id, "outcome": outcome }))
    }).unwrap();
}

#[derive(Deserialize)]
struct LinkP {
    from: NodeRefP,
    to:   NodeRefP,
    #[serde(flatten)] spec: EdgeSpecP,
}

fn register_link(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.link", |params, _ctx, _| async move {
        let p: LinkP = params.parse()?;
        let (edge_id, nodes, edge) = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let (from, to) = (p.from.to_ref(), p.to.to_ref());
            let eid = db.graph_link(&from, &to, p.spec.to_spec())
                .map_err(|e| rpc_err(-32011, e))?;
            // Read back the endpoint nodes (auto-vivified) + the edge.
            let mut nodes: Vec<Node> = Vec::with_capacity(2);
            for n in [&from, &to] {
                if let Some(node) = db.graph_get_node(n).map_err(|e| rpc_err(-32011, e))? {
                    nodes.push(node);
                }
            }
            let edge = db.graph_get_edge(&eid).map_err(|e| rpc_err(-32011, e))?;
            Ok::<_, ErrorObject<'static>>((eid.to_string(), nodes, edge))
        }).await?;

        let payload = json!({
            "nodes": nodes.iter().map(node_to_json).collect::<Vec<_>>(),
            "edges": edge.iter().map(edge_to_json).collect::<Vec<_>>(),
        });
        let outcome = fan_out(payload).await;
        Ok::<JsonValue, ErrorObject>(json!({ "id": edge_id, "outcome": outcome }))
    }).unwrap();
}

#[derive(Deserialize)]
struct LinkBatchP {
    links: Vec<LinkP>,
}

fn register_link_batch(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.link.batch", |params, _ctx, _| async move {
        let p: LinkBatchP = params.parse()?;
        let (count, nodes, edges) = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let links: Vec<(NodeRef, NodeRef, EdgeSpec)> = p.links.iter()
                .map(|l| (l.from.to_ref(), l.to.to_ref(), l.spec.to_spec()))
                .collect();
            let count = db.graph_link_batch(&links).map_err(|e| rpc_err(-32011, e))?;

            // Read back every distinct endpoint node + edge for fan-out.
            let mut node_keys: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();
            for (f, t, _) in &links {
                node_keys.insert((f.node_type.clone(), f.ref_id.clone()));
                node_keys.insert((t.node_type.clone(), t.ref_id.clone()));
            }
            let mut nodes: Vec<Node> = Vec::new();
            let mut edges: Vec<Edge> = Vec::new();
            let mut seen_edges: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
            for (nt, ri) in &node_keys {
                let nref = NodeRef::new(nt.clone(), ri.clone());
                if let Some(n) = db.graph_get_node(&nref).map_err(|e| rpc_err(-32011, e))? {
                    nodes.push(n);
                }
                for e in db.graph_outgoing(&nref, &EdgeFilter::new())
                    .map_err(|e| rpc_err(-32011, e))?
                {
                    if seen_edges.insert(e.id) {
                        edges.push(e);
                    }
                }
            }
            Ok::<_, ErrorObject<'static>>((count, nodes, edges))
        }).await?;

        let payload = json!({
            "nodes": nodes.iter().map(node_to_json).collect::<Vec<_>>(),
            "edges": edges.iter().map(edge_to_json).collect::<Vec<_>>(),
        });
        let outcome = fan_out(payload).await;
        Ok::<JsonValue, ErrorObject>(json!({ "count": count, "outcome": outcome }))
    }).unwrap();
}

#[derive(Deserialize)]
struct UnlinkP {
    from:      NodeRefP,
    to:        NodeRefP,
    edge_type: String,
}

fn register_unlink(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.unlink", |params, _ctx, _| async move {
        let p: UnlinkP = params.parse()?;
        let (from, to, et) = (p.from.to_ref(), p.to.to_ref(), p.edge_type.clone());
        let removed_ids = blocking({
            let (from, to, et) = (from.clone(), to.clone(), et.clone());
            move || -> Result<Vec<Uuid>, ErrorObject<'static>> {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let ids = db.graph_unlink(&from, &to, &et).map_err(|e| rpc_err(-32011, e))?;
                if let Some(c) = db.cluster() {
                    for id in &ids {
                        let _ = c.tombstones.mark_deleted("graph_edges", *id, now_secs() as i64);
                    }
                }
                Ok(ids)
            }
        }).await?;
        let outcome = fan_out(json!({
            "removed_edges": [{
                "from":      { "node_type": from.node_type, "ref_id": from.ref_id },
                "to":        { "node_type": to.node_type,   "ref_id": to.ref_id },
                "edge_type": et,
            }]
        })).await;
        let ids: Vec<String> = removed_ids.iter().map(|i| i.to_string()).collect();
        Ok::<JsonValue, ErrorObject>(json!({ "removed": ids, "outcome": outcome }))
    }).unwrap();
}

#[derive(Deserialize)]
struct ExpireP {
    from:      NodeRefP,
    to:        NodeRefP,
    edge_type: String,
    at:        i64,
}

fn register_expire(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.expire", |params, _ctx, _| async move {
        let p: ExpireP = params.parse()?;
        // Expire is an update — replicate the resulting full edges.
        let (updated_ids, edges) = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let ids = db.graph_expire_edge(&p.from.to_ref(), &p.to.to_ref(), &p.edge_type, p.at)
                .map_err(|e| rpc_err(-32011, e))?;
            let mut edges: Vec<Edge> = Vec::with_capacity(ids.len());
            for id in &ids {
                if let Some(e) = db.graph_get_edge(id).map_err(|e| rpc_err(-32011, e))? {
                    edges.push(e);
                }
            }
            Ok::<_, ErrorObject<'static>>((ids, edges))
        }).await?;
        let outcome = fan_out(json!({
            "edges": edges.iter().map(edge_to_json).collect::<Vec<_>>()
        })).await;
        let ids: Vec<String> = updated_ids.iter().map(|i| i.to_string()).collect();
        Ok::<JsonValue, ErrorObject>(json!({ "updated": ids, "outcome": outcome }))
    }).unwrap();
}

#[derive(Deserialize)]
struct SetWeightP {
    edge_id: String,
    weight:  f64,
}

fn register_set_weight(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.edge.set_weight", |params, _ctx, _| async move {
        let p: SetWeightP = params.parse()?;
        let edge_id = Uuid::parse_str(&p.edge_id)
            .map_err(|e| rpc_err(-32602, format!("invalid edge_id: {e}")))?;
        let edge = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph().map_err(|e| rpc_err(-32011, e))?
                .set_weight(&edge_id, p.weight).map_err(|e| rpc_err(-32011, e))?;
            db.graph_get_edge(&edge_id).map_err(|e| rpc_err(-32011, e))
        }).await?;
        let (updated, payload) = match &edge {
            Some(e) => (true, json!({ "edges": [edge_to_json(e)] })),
            None    => (false, json!({})),
        };
        let outcome = fan_out(payload).await;
        Ok::<JsonValue, ErrorObject>(json!({ "updated": updated, "outcome": outcome }))
    }).unwrap();
}

#[derive(Deserialize)]
struct EdgeTypeP {
    name:             String,
    #[serde(default = "default_weight")] default_weight:   f64,
    #[serde(default = "default_directed")] default_directed: bool,
    #[serde(default)] attrs: Option<JsonValue>,
}
fn default_weight() -> f64 { 1.0 }
fn default_directed() -> bool { true }

fn register_edge_type_register(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.edge_type.register", |params, _ctx, _| async move {
        let p: EdgeTypeP = params.parse()?;
        let attrs = p.attrs.clone().unwrap_or_else(|| json!({}));
        blocking({
            let (name, attrs) = (p.name.clone(), attrs.clone());
            move || {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                db.graph_register_edge_type(&name, p.default_weight, p.default_directed, attrs)
                    .map_err(|e| rpc_err(-32011, e))
            }
        }).await?;
        let outcome = fan_out(json!({
            "edge_types": [{
                "name": p.name, "default_weight": p.default_weight,
                "default_directed": p.default_directed, "attrs": attrs,
            }]
        })).await;
        Ok::<JsonValue, ErrorObject>(json!({ "ok": true, "outcome": outcome }))
    }).unwrap();
}

// ── maintenance (local-only) ─────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct RepairP {
    #[serde(default = "yes")] prune_dangling: bool,
    #[serde(default = "yes")] prune_invalid:  bool,
    #[serde(default = "yes")] fix_fts_drift:  bool,
    #[serde(default)]         dry_run:        bool,
}
fn yes() -> bool { true }

fn register_repair(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.repair", |params, _ctx, _| async move {
        let p: RepairP = params.parse().unwrap_or_default();
        let report = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let opts = GraphRepairOpts {
                prune_dangling: p.prune_dangling,
                prune_invalid:  p.prune_invalid,
                fix_fts_drift:  p.fix_fts_drift,
                dry_run:        p.dry_run,
            };
            db.graph_repair(&opts).map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(json!({
            "dry_run":         report.dry_run,
            "dangling_pruned": report.dangling_pruned,
            "invalid_pruned":  report.invalid_pruned,
            "fts_rebuilt":     report.fts_rebuilt,
            "fts_docs_after":  report.fts_docs_after,
            "before": {
                "healthy":                report.before.healthy,
                "node_count":             report.before.node_count,
                "edge_count":             report.before.edge_count,
                "dangling_edges":         report.before.dangling_edges,
                "invalid_temporal_edges": report.before.invalid_temporal_edges,
                "fts_drift":              report.before.fts_drift,
            },
        }))
    }).unwrap();
}

fn register_rebuild_fts(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.rebuild_fts", |_params, _ctx, _| async move {
        let n = blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_rebuild_fts().map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(json!({ "reindexed": n }))
    }).unwrap();
}

fn register_sync(module: &mut RpcModule<()>) {
    module.register_async_method("v5/graph.sync", |_params, _ctx, _| async move {
        blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.graph_sync().map_err(|e| rpc_err(-32011, e))
        }).await?;
        Ok::<JsonValue, ErrorObject>(json!({ "ok": true }))
    }).unwrap();
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
