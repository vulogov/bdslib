//! Integration tests for the [`GraphStore`] wired into
//! [`ShardsManager`] via the `graph_*` delegating API
//! (`src/shardsmanager_graph.rs`).
//!
//! These exercise the *integration* — lazy open under `{dbpath}/graph`,
//! the delegating surface, and `Arc`-sharing across manager clones.
//! Exhaustive graph-behaviour coverage lives in
//! `tests/graphstorage_test.rs`.

use bdslib::graphstorage::{
    Direction, EdgeFilter, EdgeSpec, GraphRepairOpts, NodeRef, TraversalOpts,
};
use bdslib::{EmbeddingEngine, ShardsManager};
use fastembed::EmbeddingModel;
use serde_json::json;
use std::sync::OnceLock;
use tempfile::TempDir;

// Loading the embedding model is expensive — share one across the suite.
static ENGINE: OnceLock<EmbeddingEngine> = OnceLock::new();
fn get_engine() -> &'static EmbeddingEngine {
    ENGINE.get_or_init(|| EmbeddingEngine::new(EmbeddingModel::AllMiniLML6V2, None).unwrap())
}

/// Build a `ShardsManager` over a fresh temp dbpath.  Returns the
/// `TempDir` (keep it alive for the test) and the manager.
fn tmp_manager() -> (TempDir, ShardsManager) {
    let dir = TempDir::new().unwrap();
    let dbpath = dir.path().join("db").to_str().unwrap().to_string();
    let config_path = dir.path().join("config.hjson");
    std::fs::write(
        &config_path,
        format!("{{\n  dbpath: \"{dbpath}\"\n  shard_duration: \"1h\"\n  pool_size: 4\n}}"),
    )
    .unwrap();
    let mgr =
        ShardsManager::with_embedding(config_path.to_str().unwrap(), get_engine().clone()).unwrap();
    (dir, mgr)
}

fn telem(id: &str) -> NodeRef {
    NodeRef::new("telemetry", id)
}

// ── lazy open + on-disk layout ───────────────────────────────────────────────

#[test]
fn graph_opens_lazily_under_dbpath() {
    let (dir, mgr) = tmp_manager();
    let graph_dir = dir.path().join("db").join("graph");

    // ShardsManager construction must NOT open the graph store.
    assert!(
        !graph_dir.exists(),
        "graph store opens lazily, not at ShardsManager construction"
    );

    // First graph call opens it — DuckDB file + Tantivy dir appear.
    mgr.graph_add_node(&telem("t1"), json!({"service": "checkout"}))
        .unwrap();
    assert!(graph_dir.exists(), "graph directory created on first use");
    assert!(graph_dir.join("graph.duckdb").exists(), "DuckDB store present");
    assert!(graph_dir.join("fts").exists(), "Tantivy FTS index present");
}

// ── delegating API: nodes + edges ────────────────────────────────────────────

#[test]
fn graph_nodes_and_edges_through_manager() {
    let (_d, mgr) = tmp_manager();
    mgr.graph_add_node(&telem("a"), json!({"service": "edge"})).unwrap();
    mgr.graph_add_node(&telem("b"), json!({"service": "core"})).unwrap();
    mgr.graph_link(&telem("a"), &telem("b"), EdgeSpec::new("depends_on").weight(2.0))
        .unwrap();

    let node = mgr.graph_get_node(&telem("a")).unwrap().unwrap();
    assert_eq!(node.attrs["service"], json!("edge"));

    let out = mgr.graph_outgoing(&telem("a"), &EdgeFilter::new()).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].edge_type, "depends_on");
    assert_eq!(out[0].weight, 2.0);

    assert_eq!(mgr.graph_degree(&telem("a"), Direction::Out).unwrap(), 1);
    assert_eq!(mgr.graph_incoming(&telem("b"), &EdgeFilter::new()).unwrap().len(), 1);

    assert_eq!(mgr.graph_unlink(&telem("a"), &telem("b"), "depends_on").unwrap().len(), 1);
    assert!(mgr.graph_outgoing(&telem("a"), &EdgeFilter::new()).unwrap().is_empty());
}

// ── Arc-sharing across manager clones ────────────────────────────────────────

#[test]
fn graph_is_shared_across_manager_clones() {
    let (_d, mgr) = tmp_manager();
    let mgr2 = mgr.clone();

    // Write through one clone...
    mgr.graph_add_node(&telem("shared"), json!({"v": 1})).unwrap();
    mgr.graph_link(&telem("shared"), &telem("other"), EdgeSpec::new("e")).unwrap();

    // ...visible through the other — both share one Arc<LazyGraph>.
    assert!(mgr2.graph_get_node(&telem("shared")).unwrap().is_some());
    assert_eq!(mgr2.graph_stats().unwrap().edge_count, 1);

    // And the lazily-opened GraphStore handle is the same store.
    assert_eq!(mgr.graph().unwrap().stats().unwrap().node_count, 2);
}

// ── realistic scenario: incident group + blast radius ────────────────────────

#[test]
fn graph_group_and_blast_radius_through_manager() {
    let (_d, mgr) = tmp_manager();

    // A dependency chain: edge-svc → api-svc → db.
    mgr.graph_link(&telem("edge-svc"), &telem("api-svc"), EdgeSpec::new("affects"))
        .unwrap();
    mgr.graph_link(&telem("api-svc"), &telem("db"), EdgeSpec::new("affects"))
        .unwrap();

    // An incident group over two of the services.
    mgr.graph_add_node_group(
        &NodeRef::new("group", "incident-1"),
        json!({"summary": "db slowdown"}),
        &[telem("edge-svc"), telem("api-svc")],
        "has_member",
    )
    .unwrap();

    // Blast radius from edge-svc reaches everything downstream.
    let opts = TraversalOpts::new(Direction::Out, 10, 100);
    let hits = mgr.graph_traverse(&telem("edge-svc"), &opts).unwrap();
    let reached: Vec<_> = hits.iter().map(|h| h.node.ref_id.clone()).collect();
    assert!(reached.contains(&"api-svc".to_string()));
    assert!(reached.contains(&"db".to_string()));

    // Group membership is queryable through the manager.
    let members = mgr
        .graph_outgoing(
            &NodeRef::new("group", "incident-1"),
            &EdgeFilter::new().edge_type("has_member"),
        )
        .unwrap();
    assert_eq!(members.len(), 2);

    // Shortest path edge-svc → db.
    let path = mgr
        .graph_shortest_path(&telem("edge-svc"), &telem("db"), &opts)
        .unwrap()
        .unwrap();
    assert_eq!(path.nodes.len(), 3);
}

// ── FTS search over node metadata through the manager ────────────────────────

#[test]
fn graph_search_through_manager() {
    let (_d, mgr) = tmp_manager();
    mgr.graph_add_node(&telem("t1"), json!({"msg": "payment latency spike"}))
        .unwrap();
    mgr.graph_add_node(&telem("t2"), json!({"msg": "inventory sync ok"}))
        .unwrap();
    mgr.graph_add_node(&NodeRef::new("document", "d1"), json!({"title": "payment runbook"}))
        .unwrap();

    let hits = mgr.graph_search_nodes("payment", 10).unwrap();
    let refs: Vec<_> = hits.iter().map(|(n, _)| n.ref_id.clone()).collect();
    assert!(refs.contains(&"t1".to_string()));
    assert!(refs.contains(&"d1".to_string()));
    assert!(!refs.contains(&"t2".to_string()));

    let typed = mgr.graph_search_nodes_typed("payment", &["telemetry"], 10).unwrap();
    assert!(typed.iter().all(|(n, _)| n.node_type == "telemetry"));
}

// ── self-healing primitives through the manager ──────────────────────────────

#[test]
fn graph_self_healing_through_manager() {
    let (_d, mgr) = tmp_manager();
    mgr.graph_add_node(&telem("t1"), json!({})).unwrap();
    mgr.graph_link(&telem("t1"), &telem("t2"), EdgeSpec::new("e")).unwrap();

    // probe — engines validate
    assert!(mgr.graph_probe().is_ok());

    // verify — a freshly-built store is healthy
    let report = mgr.graph_verify().unwrap();
    assert!(report.healthy);
    assert_eq!(report.node_count, 2);
    assert_eq!(report.edge_count, 1);

    // repair on a healthy store is a no-op
    let repair = mgr.graph_repair(&GraphRepairOpts::default()).unwrap();
    assert_eq!(repair.dangling_pruned, 0);
    assert!(!repair.fts_rebuilt);

    // rebuild_fts is reachable and re-indexes every node
    assert_eq!(mgr.graph_rebuild_fts().unwrap(), 2);

    // fingerprint is callable through the manager
    let fp = mgr.graph_fingerprint().unwrap();
    assert_eq!(fp.node_count, 2);
    assert_eq!(fp.edge_count, 1);
}

// ── maintenance: stats + sync ────────────────────────────────────────────────

#[test]
fn graph_stats_and_sync_through_manager() {
    let (_d, mgr) = tmp_manager();
    mgr.graph_add_node(&telem("t1"), json!({})).unwrap();
    mgr.graph_register_edge_type("calls", 1.0, true, json!({})).unwrap();
    mgr.graph_link(&telem("t1"), &telem("t2"), EdgeSpec::new("calls")).unwrap();

    mgr.graph_sync().unwrap();

    let stats = mgr.graph_stats().unwrap();
    assert_eq!(stats.node_count, 2);
    assert_eq!(stats.edge_count, 1);
    assert_eq!(stats.edge_type_count, 1);
    assert_eq!(stats.fts_doc_count, 2);
}

// ── temporal edges + batch linking through the manager ───────────────────────

#[test]
fn graph_temporal_and_batch_through_manager() {
    use bdslib::graphstorage::{TimeScope, OPEN_END};
    let (_d, mgr) = tmp_manager();

    // Batch-link a fan-out in one call.  Explicit `valid_from = 0` so
    // these edges are valid at any later instant we query.
    let links: Vec<_> = (0..5)
        .map(|i| {
            (
                telem("hub"),
                telem(&format!("leaf-{i}")),
                EdgeSpec::new("calls").valid(0, OPEN_END),
            )
        })
        .collect();
    assert_eq!(mgr.graph_link_batch(&links).unwrap(), 5);
    assert_eq!(mgr.graph_degree(&telem("hub"), Direction::Out).unwrap(), 5);

    // A time-bounded edge active during [1000, OPEN_END), then expired at t=2000.
    mgr.graph_link(
        &telem("hub"),
        &telem("transient"),
        EdgeSpec::new("hot_path").valid(1_000, OPEN_END),
    )
    .unwrap();
    assert_eq!(
        mgr.graph_expire_edge(&telem("hub"), &telem("transient"), "hot_path", 2_000).unwrap().len(),
        1
    );

    // All-time view keeps the expired edge (history preserved)...
    let all = mgr.graph_outgoing(&telem("hub"), &EdgeFilter::new()).unwrap();
    assert_eq!(all.len(), 6, "5 batch + 1 expired hot_path (history preserved)");

    // ...the hot_path edge is valid "as of t=1500" (inside its window)...
    let at_1500 = mgr
        .graph_outgoing(&telem("hub"), &EdgeFilter::new().time(TimeScope::At(1_500)))
        .unwrap();
    assert_eq!(at_1500.len(), 6, "all 5 calls + the still-open-then hot_path");

    // ...but gone "as of t=5000" (after it was expired at 2000).
    let at_5000 = mgr
        .graph_outgoing(&telem("hub"), &EdgeFilter::new().time(TimeScope::At(5_000)))
        .unwrap();
    assert_eq!(at_5000.len(), 5, "expired hot_path edge no longer valid at t=5000");
}
