/// shardsmanager_graph_demo — the relationship GraphStore through the
/// `ShardsManager` API layer (`graph_*` delegating methods).
///
/// Unlike `graph_demo`, which drives a bare `GraphStore`, this shows
/// the graph as ShardsManager exposes it: lazily opened under
/// `{dbpath}/graph/`, shared across manager clones, alongside the
/// shards / docstore / signals stores.
///
/// Sections:
///   1. Setup            — a ShardsManager over a temp dbpath
///   2. Lazy open        — the graph store opens on first `graph_*` call
///   3. Nodes + edges    — typed entities + a weighted dependency edge
///   4. Incident group   — a "group of telemetry items" as a node
///   5. Blast radius     — bounded traversal through the manager
///   6. Metadata search  — FTS over node attrs via the manager
///   7. Self-healing     — probe / verify / repair / fingerprint
///   8. Shared handle    — a ShardsManager clone sees the same graph
///
/// Run with:
///
/// ```bash
/// cargo run --example shardsmanager_graph_demo
/// ```
use bdslib::graphstorage::{Direction, EdgeFilter, EdgeSpec, GraphRepairOpts, NodeRef, TraversalOpts};
use bdslib::{EmbeddingEngine, ShardsManager};
use fastembed::EmbeddingModel;
use serde_json::json;
use tempfile::TempDir;

fn telem(id: &str) -> NodeRef {
    NodeRef::new("telemetry", id)
}

fn section(title: &str) {
    println!("\n=== {title} ===");
}

fn main() {
    // ── 1. Setup ──────────────────────────────────────────────────────────────
    let dir = TempDir::new().expect("tempdir");
    let dbpath = dir.path().join("db").to_str().unwrap().to_string();
    let config_path = dir.path().join("config.hjson");
    std::fs::write(
        &config_path,
        format!("{{\n  dbpath: \"{dbpath}\"\n  shard_duration: \"1h\"\n  pool_size: 4\n}}"),
    )
    .unwrap();
    let engine = EmbeddingEngine::new(EmbeddingModel::AllMiniLML6V2, None)
        .expect("embedding engine");
    let mgr = ShardsManager::with_embedding(config_path.to_str().unwrap(), engine)
        .expect("shards manager");
    println!("ShardsManager opened over {dbpath}");

    // ── 2. Lazy open ──────────────────────────────────────────────────────────
    section("2. Lazy open");
    let graph_dir = dir.path().join("db").join("graph");
    println!(
        "   {}/graph exists before first graph call? {}",
        dbpath,
        graph_dir.exists()
    );
    // First graph_* call opens DuckDB + Tantivy under {dbpath}/graph.
    mgr.graph_add_node(&telem("checkout-svc"), json!({"service": "checkout", "tier": "edge"}))
        .unwrap();
    println!(
        "   after first graph_add_node, {}/graph exists? {}",
        dbpath,
        graph_dir.exists()
    );

    // ── 3. Nodes + edges ──────────────────────────────────────────────────────
    section("3. Nodes + edges through the manager");
    mgr.graph_add_node(&telem("payments-svc"), json!({"service": "payments", "tier": "core"}))
        .unwrap();
    mgr.graph_add_node(&telem("ledger-db"), json!({"service": "ledger", "tier": "data"}))
        .unwrap();
    mgr.graph_register_edge_type("depends_on", 1.0, true, json!({"category": "topology"}))
        .unwrap();
    mgr.graph_link(&telem("checkout-svc"), &telem("payments-svc"),
                   EdgeSpec::new("depends_on").weight(2.0)).unwrap();
    mgr.graph_link(&telem("payments-svc"), &telem("ledger-db"),
                   EdgeSpec::new("depends_on")).unwrap();
    println!(
        "   checkout-svc out-degree = {}",
        mgr.graph_degree(&telem("checkout-svc"), Direction::Out).unwrap()
    );

    // ── 4. Incident group ─────────────────────────────────────────────────────
    section("4. Incident group");
    mgr.graph_add_node_group(
        &NodeRef::new("group", "incident-2026-05-14"),
        json!({"summary": "payment latency spike"}),
        &[telem("checkout-svc"), telem("payments-svc")],
        "has_member",
    )
    .unwrap();
    let members = mgr
        .graph_outgoing(
            &NodeRef::new("group", "incident-2026-05-14"),
            &EdgeFilter::new().edge_type("has_member"),
        )
        .unwrap();
    println!("   incident group has {} member telemetry items", members.len());

    // ── 5. Blast radius ───────────────────────────────────────────────────────
    section("5. Blast radius (bounded traversal)");
    let opts = TraversalOpts::new(Direction::Out, 10, 1000);
    let blast = mgr.graph_traverse(&telem("checkout-svc"), &opts).unwrap();
    println!("   downstream of checkout-svc ({} entities):", blast.len());
    for hit in &blast {
        println!("     depth {} → {}:{}", hit.depth, hit.node.node_type, hit.node.ref_id);
    }
    if let Some(path) =
        mgr.graph_shortest_path(&telem("checkout-svc"), &telem("ledger-db"), &opts).unwrap()
    {
        let chain: Vec<_> = path.nodes.iter().map(|n| n.ref_id.clone()).collect();
        println!("   shortest path checkout-svc → ledger-db: {}", chain.join(" → "));
    }

    // ── 6. Metadata search ────────────────────────────────────────────────────
    section("6. FTS search over node metadata");
    let hits = mgr.graph_search_nodes("payments", 10).unwrap();
    for (node, score) in &hits {
        println!("   {:.3}  {}:{}", score, node.node_type, node.ref_id);
    }

    // ── 7. Self-healing primitives ────────────────────────────────────────────
    section("7. Self-healing");
    mgr.graph_probe().expect("engines validate");
    let report = mgr.graph_verify().unwrap();
    println!(
        "   verify: healthy={} nodes={} edges={} dangling={} fts_drift={}",
        report.healthy, report.node_count, report.edge_count,
        report.dangling_edges, report.fts_drift,
    );
    let repair = mgr.graph_repair(&GraphRepairOpts::default()).unwrap();
    println!(
        "   repair: dangling_pruned={} invalid_pruned={} fts_rebuilt={}",
        repair.dangling_pruned, repair.invalid_pruned, repair.fts_rebuilt,
    );
    let fp = mgr.graph_fingerprint().unwrap();
    println!(
        "   fingerprint: {} nodes / {} edges  nodes_hash={}",
        fp.node_count, fp.edge_count, fp.nodes_hash,
    );

    // ── 8. Shared handle across clones ────────────────────────────────────────
    section("8. Graph shared across ShardsManager clones");
    let mgr2 = mgr.clone();
    mgr2.graph_add_node(&telem("via-clone"), json!({"added_by": "clone"}))
        .unwrap();
    let seen = mgr.graph_get_node(&telem("via-clone")).unwrap().is_some();
    println!("   node added via mgr.clone() is visible through the original: {seen}");

    mgr.graph_sync().unwrap();
    let stats = mgr.graph_stats().unwrap();
    println!(
        "\nFinal graph: {} nodes, {} edges, {} edge types, {} FTS docs",
        stats.node_count, stats.edge_count, stats.edge_type_count, stats.fts_doc_count,
    );
}
