//! Integration tests for [`GraphStore`] — the relationship graph
//! layer (DuckDB + Tantivy) under `ShardsManager`.
//!
//! Each test opens a fresh `GraphStore` in its own `TempDir`.

use bdslib::graphstorage::{
    edge_id_for, node_id_for, Direction, Edge, EdgeFilter, EdgeSpec, GraphRepairOpts, GraphStore,
    Node, NodeRef, Strategy, TimeScope, TraversalOpts, OPEN_END,
};
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

fn store() -> (TempDir, GraphStore) {
    let dir = TempDir::new().unwrap();
    let g = GraphStore::open(dir.path().to_str().unwrap(), 4).unwrap();
    (dir, g)
}

fn telem(id: &str) -> NodeRef { NodeRef::new("telemetry", id) }
fn doc(id: &str) -> NodeRef { NodeRef::new("document", id) }
fn signal(id: &str) -> NodeRef { NodeRef::new("signal", id) }

// ── nodes ────────────────────────────────────────────────────────────────────

#[test]
fn add_and_get_node() {
    let (_d, g) = store();
    let id = g.add_node(&telem("t1"), json!({"service": "checkout", "level": 3})).unwrap();

    let fetched = g.get_node(&telem("t1")).unwrap().unwrap();
    assert_eq!(fetched.id, id);
    assert_eq!(fetched.node_type, "telemetry");
    assert_eq!(fetched.ref_id, "t1");
    assert_eq!(fetched.attrs["service"], json!("checkout"));

    assert!(g.get_node(&telem("nope")).unwrap().is_none());
}

#[test]
fn add_node_is_idempotent_upsert() {
    let (_d, g) = store();
    let id1 = g.add_node(&telem("t1"), json!({"v": 1})).unwrap();
    let id2 = g.add_node(&telem("t1"), json!({"v": 2})).unwrap();
    assert_eq!(id1, id2, "same natural key keeps the same node id");
    assert_eq!(g.get_node(&telem("t1")).unwrap().unwrap().attrs["v"], json!(2));
    assert_eq!(g.stats().unwrap().node_count, 1);
}

#[test]
fn resolve_caches_id() {
    let (_d, g) = store();
    let id = g.add_node(&telem("t1"), json!({})).unwrap();
    // First resolve populates the cache, second should hit it.
    assert_eq!(g.resolve(&telem("t1")).unwrap(), Some(id));
    assert_eq!(g.resolve(&telem("t1")).unwrap(), Some(id));
    assert!(g.stats().unwrap().cache.resolve_hits >= 1);
}

#[test]
fn remove_node_cascades_edges() {
    let (_d, g) = store();
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("calls")).unwrap();
    g.link(&telem("b"), &telem("c"), EdgeSpec::new("calls")).unwrap();
    assert_eq!(g.stats().unwrap().edge_count, 2);

    assert!(g.remove_node(&telem("b")).unwrap());
    // both edges touched b — both gone
    assert_eq!(g.stats().unwrap().edge_count, 0);
    assert!(g.get_node(&telem("b")).unwrap().is_none());
    assert!(!g.remove_node(&telem("b")).unwrap(), "second remove is a no-op");
}

// ── edges: directed / undirected / weighted ──────────────────────────────────

#[test]
fn directed_edge_outgoing_incoming() {
    let (_d, g) = store();
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("depends_on").weight(2.5)).unwrap();

    let out_a = g.outgoing(&telem("a"), &EdgeFilter::new()).unwrap();
    assert_eq!(out_a.len(), 1);
    assert_eq!(out_a[0].edge_type, "depends_on");
    assert_eq!(out_a[0].weight, 2.5);
    assert!(out_a[0].directed);

    // directed: a has no incoming, b has no outgoing
    assert!(g.incoming(&telem("a"), &EdgeFilter::new()).unwrap().is_empty());
    assert!(g.outgoing(&telem("b"), &EdgeFilter::new()).unwrap().is_empty());
    assert_eq!(g.incoming(&telem("b"), &EdgeFilter::new()).unwrap().len(), 1);
}

#[test]
fn undirected_edge_visible_from_both_ends() {
    let (_d, g) = store();
    g.link(&signal("s1"), &signal("s2"), EdgeSpec::new("correlates_with").undirected().weight(0.8))
        .unwrap();

    // an undirected edge is an outgoing AND incoming edge of both ends
    assert_eq!(g.outgoing(&signal("s1"), &EdgeFilter::new()).unwrap().len(), 1);
    assert_eq!(g.outgoing(&signal("s2"), &EdgeFilter::new()).unwrap().len(), 1);
    assert_eq!(g.incoming(&signal("s1"), &EdgeFilter::new()).unwrap().len(), 1);
    assert_eq!(g.incoming(&signal("s2"), &EdgeFilter::new()).unwrap().len(), 1);

    // stored exactly once
    assert_eq!(g.stats().unwrap().edge_count, 1);
}

#[test]
fn link_upserts_on_episode() {
    let (_d, g) = store();
    let spec = EdgeSpec::new("correlates_with").undirected().weight(1.0).valid(1000, OPEN_END);
    let e1 = g.link(&signal("s1"), &signal("s2"), spec.clone()).unwrap();
    // same (src,dst,type,valid_from) → upsert, same edge id, new weight
    let e2 = g.link(&signal("s1"), &signal("s2"), spec.weight(5.0)).unwrap();
    assert_eq!(e1, e2);
    assert_eq!(g.stats().unwrap().edge_count, 1);
    let edges = g.outgoing(&signal("s1"), &EdgeFilter::new()).unwrap();
    assert_eq!(edges[0].weight, 5.0);
}

#[test]
fn unlink_removes_edge() {
    let (_d, g) = store();
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("calls")).unwrap();
    assert_eq!(g.unlink(&telem("a"), &telem("b"), "calls").unwrap(), 1);
    assert_eq!(g.unlink(&telem("a"), &telem("b"), "calls").unwrap(), 0);
    assert!(g.outgoing(&telem("a"), &EdgeFilter::new()).unwrap().is_empty());
}

#[test]
fn ensure_node_auto_vivifies_on_link() {
    let (_d, g) = store();
    // neither endpoint pre-created
    g.link(&telem("x"), &doc("y"), EdgeSpec::new("references")).unwrap();
    assert!(g.get_node(&telem("x")).unwrap().is_some());
    assert!(g.get_node(&doc("y")).unwrap().is_some());
}

#[test]
fn link_batch_writes_all() {
    let (_d, g) = store();
    let links = vec![
        (telem("a"), telem("b"), EdgeSpec::new("calls")),
        (telem("b"), telem("c"), EdgeSpec::new("calls")),
        (telem("c"), telem("d"), EdgeSpec::new("calls")),
    ];
    assert_eq!(g.link_batch(&links).unwrap(), 3);
    assert_eq!(g.stats().unwrap().edge_count, 3);
}

// ── groups ───────────────────────────────────────────────────────────────────

#[test]
fn node_group_links_members() {
    let (_d, g) = store();
    let members = vec![telem("t1"), telem("t2"), telem("t3")];
    let group_id = g
        .add_node_group(&NodeRef::new("group", "incident-42"), json!({"name": "spike"}), &members, "has_member")
        .unwrap();

    let grp = g.get_node(&NodeRef::new("group", "incident-42")).unwrap().unwrap();
    assert_eq!(grp.id, group_id);

    // group → 3 members
    let f = EdgeFilter::new().edge_type("has_member");
    assert_eq!(g.outgoing(&NodeRef::new("group", "incident-42"), &f).unwrap().len(), 3);
    // each member has the group as an incoming edge
    assert_eq!(g.incoming(&telem("t1"), &f).unwrap().len(), 1);

    let neigh = g.neighbors(&NodeRef::new("group", "incident-42"), Direction::Out, &f).unwrap();
    assert_eq!(neigh.len(), 3);
}

// ── filters ──────────────────────────────────────────────────────────────────

#[test]
fn edge_filter_by_type_and_weight() {
    let (_d, g) = store();
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("calls").weight(1.0)).unwrap();
    g.link(&telem("a"), &telem("c"), EdgeSpec::new("calls").weight(9.0)).unwrap();
    g.link(&telem("a"), &telem("d"), EdgeSpec::new("emits").weight(1.0)).unwrap();

    assert_eq!(g.outgoing(&telem("a"), &EdgeFilter::new()).unwrap().len(), 3);
    assert_eq!(
        g.outgoing(&telem("a"), &EdgeFilter::new().edge_type("calls")).unwrap().len(),
        2
    );
    assert_eq!(
        g.outgoing(&telem("a"), &EdgeFilter::new().min_weight(5.0)).unwrap().len(),
        1
    );
    assert_eq!(
        g.outgoing(&telem("a"), &EdgeFilter::new().limit(2)).unwrap().len(),
        2
    );
}

#[test]
fn degree_counts() {
    let (_d, g) = store();
    g.link(&telem("hub"), &telem("a"), EdgeSpec::new("calls")).unwrap();
    g.link(&telem("hub"), &telem("b"), EdgeSpec::new("calls")).unwrap();
    g.link(&telem("c"), &telem("hub"), EdgeSpec::new("calls")).unwrap();
    assert_eq!(g.degree(&telem("hub"), Direction::Out).unwrap(), 2);
    assert_eq!(g.degree(&telem("hub"), Direction::In).unwrap(), 1);
    assert_eq!(g.degree(&telem("hub"), Direction::Both).unwrap(), 3);
}

// ── temporal edges ───────────────────────────────────────────────────────────

#[test]
fn time_scope_filters_edges() {
    let (_d, g) = store();
    // an edge active only during [100, 200)
    g.link(
        &signal("s1"),
        &signal("s2"),
        EdgeSpec::new("correlates_with").undirected().valid(100, 200),
    )
    .unwrap();
    // an open-ended edge from t=1000
    g.link(
        &signal("s1"),
        &signal("s3"),
        EdgeSpec::new("correlates_with").undirected().valid(1000, OPEN_END),
    )
    .unwrap();

    let all = EdgeFilter::new();
    assert_eq!(g.outgoing(&signal("s1"), &all).unwrap().len(), 2);

    // at t=150 only the [100,200) edge is valid
    let at_150 = EdgeFilter::new().time(TimeScope::At(150));
    assert_eq!(g.outgoing(&signal("s1"), &at_150).unwrap().len(), 1);

    // at t=5000 only the open-ended edge is valid
    let at_5000 = EdgeFilter::new().time(TimeScope::At(5000));
    let hits = g.outgoing(&signal("s1"), &at_5000).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].valid_to, OPEN_END);

    // overlap window [50, 120) catches the [100,200) edge
    let overlap = EdgeFilter::new().time(TimeScope::Overlap(50, 120));
    assert_eq!(g.outgoing(&signal("s1"), &overlap).unwrap().len(), 1);
}

#[test]
fn expire_edge_closes_window_preserves_history() {
    let (_d, g) = store();
    g.link(
        &signal("s1"),
        &signal("s2"),
        EdgeSpec::new("correlates_with").undirected().valid(100, OPEN_END),
    )
    .unwrap();
    // relationship ends at t=500
    assert_eq!(g.expire_edge(&signal("s1"), &signal("s2"), "correlates_with", 500).unwrap(), 1);

    // edge still exists (history preserved), but no longer open at t=1000
    assert_eq!(g.outgoing(&signal("s1"), &EdgeFilter::new()).unwrap().len(), 1);
    assert!(g.outgoing(&signal("s1"), &EdgeFilter::new().time(TimeScope::At(1000))).unwrap().is_empty());
    // ...still queryable at t=300 (inside the now-closed window)
    assert_eq!(
        g.outgoing(&signal("s1"), &EdgeFilter::new().time(TimeScope::At(300))).unwrap().len(),
        1
    );
}

// ── traversal: blast radius ──────────────────────────────────────────────────

#[test]
fn traverse_bfs_blast_radius() {
    let (_d, g) = store();
    // a → b → c → d  ;  a → e
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("affects")).unwrap();
    g.link(&telem("b"), &telem("c"), EdgeSpec::new("affects")).unwrap();
    g.link(&telem("c"), &telem("d"), EdgeSpec::new("affects")).unwrap();
    g.link(&telem("a"), &telem("e"), EdgeSpec::new("affects")).unwrap();

    // full downstream blast radius from a
    let opts = TraversalOpts::new(Direction::Out, 10, 100);
    let hits = g.traverse(&telem("a"), &opts).unwrap();
    let reached: Vec<_> = hits.iter().map(|h| h.node.ref_id.clone()).collect();
    assert_eq!(reached.len(), 4); // b, c, d, e
    assert!(reached.contains(&"d".to_string()));

    // depth-bounded: only 1 hop
    let shallow = TraversalOpts::new(Direction::Out, 1, 100);
    let near = g.traverse(&telem("a"), &shallow).unwrap();
    assert_eq!(near.len(), 2); // b, e only
    assert!(near.iter().all(|h| h.depth == 1));
}

#[test]
fn traverse_respects_time_scope() {
    let (_d, g) = store();
    // a→b valid all time; b→c valid only during [100,200)
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("affects").valid(0, OPEN_END)).unwrap();
    g.link(&telem("b"), &telem("c"), EdgeSpec::new("affects").valid(100, 200)).unwrap();

    // blast radius "as of t=150" reaches c
    let during = TraversalOpts::new(Direction::Out, 10, 100)
        .edge_filter(EdgeFilter::new().time(TimeScope::At(150)));
    assert_eq!(g.traverse(&telem("a"), &during).unwrap().len(), 2);

    // "as of t=5000" — b→c no longer valid, c is unreachable
    let after = TraversalOpts::new(Direction::Out, 10, 100)
        .edge_filter(EdgeFilter::new().time(TimeScope::At(5000)));
    assert_eq!(g.traverse(&telem("a"), &after).unwrap().len(), 1);
}

#[test]
fn shortest_path_bfs_and_dijkstra() {
    let (_d, g) = store();
    // two routes a→d:  a-b-d (2 hops, weight 1+1=2) and a-c-d (2 hops, weight 5+5=10)
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("e").weight(1.0)).unwrap();
    g.link(&telem("b"), &telem("d"), EdgeSpec::new("e").weight(1.0)).unwrap();
    g.link(&telem("a"), &telem("c"), EdgeSpec::new("e").weight(5.0)).unwrap();
    g.link(&telem("c"), &telem("d"), EdgeSpec::new("e").weight(5.0)).unwrap();

    let opts = TraversalOpts::new(Direction::Out, 10, 100);
    let path = g.shortest_path(&telem("a"), &telem("d"), &opts).unwrap().unwrap();
    assert_eq!(path.nodes.len(), 3); // a, _, d
    assert_eq!(path.edges.len(), 2);

    // dijkstra must pick the cheap route (total weight 2, not 10)
    let dij = opts.clone().strategy(Strategy::Dijkstra);
    let dpath = g.shortest_path(&telem("a"), &telem("d"), &dij).unwrap().unwrap();
    assert_eq!(dpath.total_weight, 2.0);

    // unreachable
    assert!(g.shortest_path(&telem("a"), &telem("nope"), &opts).unwrap().is_none());
}

#[test]
fn reachable_lists_nodes() {
    let (_d, g) = store();
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("x")).unwrap();
    g.link(&telem("b"), &telem("c"), EdgeSpec::new("x")).unwrap();
    let opts = TraversalOpts::new(Direction::Out, 10, 100);
    let reachable = g.reachable(&telem("a"), &opts).unwrap();
    assert_eq!(reachable.len(), 2);
}

// ── FTS search over node metadata ────────────────────────────────────────────

#[test]
fn search_nodes_by_metadata() {
    let (_d, g) = store();
    g.add_node(&telem("t1"), json!({"service": "checkout", "msg": "payment latency spike"})).unwrap();
    g.add_node(&telem("t2"), json!({"service": "inventory", "msg": "stock sync ok"})).unwrap();
    g.add_node(&doc("d1"), json!({"title": "checkout runbook"})).unwrap();

    let hits = g.search_nodes("checkout", 10).unwrap();
    let refs: Vec<_> = hits.iter().map(|(n, _)| (n.node_type.clone(), n.ref_id.clone())).collect();
    assert!(refs.contains(&("telemetry".to_string(), "t1".to_string())));
    assert!(refs.contains(&("document".to_string(), "d1".to_string())));
    assert!(!refs.iter().any(|(_, r)| r == "t2"));

    // typed restriction
    let typed = g.search_nodes_typed("checkout", &["telemetry"], 10).unwrap();
    assert!(typed.iter().all(|(n, _)| n.node_type == "telemetry"));
    assert_eq!(typed.len(), 1);
}

#[test]
fn search_reflects_metadata_update() {
    let (_d, g) = store();
    g.add_node(&telem("t1"), json!({"msg": "everything nominal"})).unwrap();
    assert!(g.search_nodes("catastrophe", 10).unwrap().is_empty());
    // re-add with new metadata → FTS re-indexed
    g.add_node(&telem("t1"), json!({"msg": "catastrophe in progress"})).unwrap();
    assert_eq!(g.search_nodes("catastrophe", 10).unwrap().len(), 1);
}

#[test]
fn reindex_fts_rebuilds() {
    let (_d, g) = store();
    g.add_node(&telem("t1"), json!({"msg": "alpha"})).unwrap();
    g.add_node(&telem("t2"), json!({"msg": "beta"})).unwrap();
    assert_eq!(g.reindex_fts().unwrap(), 2);
    assert_eq!(g.search_nodes("alpha", 10).unwrap().len(), 1);
}

// ── edge-type registry ───────────────────────────────────────────────────────

#[test]
fn registered_edge_type_supplies_defaults() {
    let (_d, g) = store();
    g.register_edge_type("caused_by", 3.0, true, json!({"category": "causal"})).unwrap();
    let spec = g.edge_spec("caused_by").unwrap();
    assert_eq!(spec.weight, 3.0);
    assert!(spec.directed);

    // unregistered → plain defaults
    let plain = g.edge_spec("never_registered").unwrap();
    assert_eq!(plain.weight, 1.0);
}

// ── cache invalidation ───────────────────────────────────────────────────────

#[test]
fn adjacency_cache_invalidated_on_edge_write() {
    let (_d, g) = store();
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("e")).unwrap();
    // warm the adjacency cache
    assert_eq!(g.outgoing(&telem("a"), &EdgeFilter::new()).unwrap().len(), 1);
    // a new edge from a must invalidate a's cached adjacency
    g.link(&telem("a"), &telem("c"), EdgeSpec::new("e")).unwrap();
    assert_eq!(g.outgoing(&telem("a"), &EdgeFilter::new()).unwrap().len(), 2);
}

#[test]
fn stats_report_counts() {
    let (_d, g) = store();
    g.add_node(&telem("t1"), json!({})).unwrap();
    g.link(&telem("t1"), &telem("t2"), EdgeSpec::new("e")).unwrap();
    g.register_edge_type("e", 1.0, true, json!({})).unwrap();
    let s = g.stats().unwrap();
    assert_eq!(s.node_count, 2);
    assert_eq!(s.edge_count, 1);
    assert_eq!(s.edge_type_count, 1);
    assert_eq!(s.fts_doc_count, 2);
}

// ── self-healing: deterministic ids ──────────────────────────────────────────

#[test]
fn entity_ids_are_deterministic_across_replicas() {
    // Two independent stores — same logical entity must get the same id,
    // so anti-entropy / LWW can converge a replicated graph.
    let (_d1, g1) = store();
    let (_d2, g2) = store();

    let n1 = g1.add_node(&telem("t1"), json!({"v": 1})).unwrap();
    let n2 = g2.add_node(&telem("t1"), json!({"v": 999})).unwrap();
    assert_eq!(n1, n2, "same (node_type, ref_id) → same id on every replica");
    assert_eq!(n1, node_id_for("telemetry", "t1"));

    let e1 = g1.link(&telem("a"), &telem("b"), EdgeSpec::new("calls").valid(100, OPEN_END)).unwrap();
    let e2 = g2.link(&telem("a"), &telem("b"), EdgeSpec::new("calls").valid(100, OPEN_END)).unwrap();
    assert_eq!(e1, e2, "same edge episode → same id on every replica");
}

// ── self-healing: probe / verify / repair ────────────────────────────────────

#[test]
fn probe_validates_engines() {
    let (_d, g) = store();
    g.add_node(&telem("t1"), json!({})).unwrap();
    assert!(g.probe().is_ok());
}

#[test]
fn verify_reports_healthy_store() {
    let (_d, g) = store();
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("e")).unwrap();
    let report = g.verify().unwrap();
    assert!(report.healthy);
    assert_eq!(report.node_count, 2);
    assert_eq!(report.edge_count, 1);
    assert_eq!(report.dangling_edges, 0);
    assert_eq!(report.fts_drift, 0);
}

#[test]
fn verify_detects_and_repair_prunes_dangling_edges() {
    let (_d, g) = store();
    g.add_node(&telem("real"), json!({})).unwrap();
    // Inject a dangling edge via the replication primitive — both
    // endpoints reference nodes that do not exist locally.
    let ghost = Edge {
        id:         Uuid::nil(), // recomputed inside apply_edge_lww
        src:        Uuid::new_v4(),
        dst:        Uuid::new_v4(),
        edge_type:  "phantom".into(),
        weight:     1.0,
        directed:   true,
        attrs:      json!({}),
        valid_from: 1_000,
        valid_to:   OPEN_END,
        created_at: 1_000,
        updated_at: 1_000,
    };
    assert!(g.apply_edge_lww(&ghost).unwrap());

    let before = g.verify().unwrap();
    assert_eq!(before.dangling_edges, 1);
    assert!(!before.healthy);

    let report = g.repair(&GraphRepairOpts::default()).unwrap();
    assert_eq!(report.dangling_pruned, 1);

    let after = g.verify().unwrap();
    assert_eq!(after.dangling_edges, 0);
    assert!(after.healthy);
}

#[test]
fn prune_invalid_edges_removes_temporal_inversions() {
    let (_d, g) = store();
    g.add_node(&telem("a"), json!({})).unwrap();
    g.add_node(&telem("b"), json!({})).unwrap();
    // valid_from > valid_to — a corrupt temporal range, injected via apply.
    let bad = Edge {
        id:         Uuid::nil(),
        src:        node_id_for("telemetry", "a"),
        dst:        node_id_for("telemetry", "b"),
        edge_type:  "e".into(),
        weight:     1.0,
        directed:   true,
        attrs:      json!({}),
        valid_from: 9_000,
        valid_to:   1_000,
        created_at: 1_000,
        updated_at: 1_000,
    };
    g.apply_edge_lww(&bad).unwrap();
    assert_eq!(g.verify().unwrap().invalid_temporal_edges, 1);
    assert_eq!(g.prune_invalid_edges().unwrap(), 1);
    assert!(g.verify().unwrap().healthy);
}

#[test]
fn repair_dry_run_does_not_mutate() {
    let (_d, g) = store();
    g.add_node(&telem("real"), json!({})).unwrap();
    let ghost = Edge {
        id: Uuid::nil(), src: Uuid::new_v4(), dst: Uuid::new_v4(),
        edge_type: "phantom".into(), weight: 1.0, directed: true, attrs: json!({}),
        valid_from: 1, valid_to: OPEN_END, created_at: 1, updated_at: 1,
    };
    g.apply_edge_lww(&ghost).unwrap();

    let opts = GraphRepairOpts { dry_run: true, ..GraphRepairOpts::default() };
    let report = g.repair(&opts).unwrap();
    assert!(report.dry_run);
    assert_eq!(report.dangling_pruned, 0);
    // still dangling — dry run mutated nothing
    assert_eq!(g.verify().unwrap().dangling_edges, 1);
}

#[test]
fn rebuild_fts_recovers_drift() {
    let (_d, g) = store();
    g.add_node(&telem("t1"), json!({"msg": "alpha"})).unwrap();
    g.add_node(&telem("t2"), json!({"msg": "beta"})).unwrap();
    let n = g.rebuild_fts().unwrap();
    assert_eq!(n, 2);
    assert!(g.verify().unwrap().healthy);
    assert_eq!(g.search_nodes("alpha", 10).unwrap().len(), 1);
}

// ── self-healing: fingerprint & convergence ──────────────────────────────────

#[test]
fn fingerprint_converges_after_replication() {
    let (_d1, g1) = store();
    let (_d2, g2) = store();
    g1.add_node(&telem("a"), json!({"v": 1})).unwrap();
    g1.add_node(&telem("b"), json!({"v": 2})).unwrap();
    g1.link(&telem("a"), &telem("b"), EdgeSpec::new("e").weight(1.5).valid(100, 200)).unwrap();

    // The fingerprint is stable for a fixed store state...
    assert_eq!(g1.fingerprint().unwrap(), g1.fingerprint().unwrap());
    // ...and the two replicas differ before replication.  (Locally
    // created nodes carry wall-clock `updated_at`, so convergence comes
    // from the timestamp-preserving LWW apply primitives, not from
    // re-running add_node independently.)
    assert_ne!(g1.fingerprint().unwrap(), g2.fingerprint().unwrap());

    // Replicate g1 → g2 via the LWW apply primitives.
    for ns in g1.node_summaries().unwrap() {
        let nref = NodeRef::new(ns.node_type.clone(), ns.ref_id.clone());
        let node = g1.get_node(&nref).unwrap().unwrap();
        g2.apply_node_lww(&node).unwrap();
    }
    for ns in g1.node_summaries().unwrap() {
        let nref = NodeRef::new(ns.node_type.clone(), ns.ref_id.clone());
        for edge in g1.outgoing(&nref, &EdgeFilter::new()).unwrap() {
            g2.apply_edge_lww(&edge).unwrap();
        }
    }
    // Now the replicas have converged — identical fingerprints.
    assert_eq!(g1.fingerprint().unwrap(), g2.fingerprint().unwrap());

    // Divergence flips the fingerprint again.
    g2.add_node(&telem("c"), json!({})).unwrap();
    assert_ne!(g1.fingerprint().unwrap(), g2.fingerprint().unwrap());
}

#[test]
fn summaries_enumerate_natural_keys() {
    let (_d, g) = store();
    g.add_node(&telem("a"), json!({})).unwrap();
    g.add_node(&telem("b"), json!({})).unwrap();
    g.link(&telem("a"), &telem("b"), EdgeSpec::new("e").valid(100, OPEN_END)).unwrap();

    let nodes = g.node_summaries().unwrap();
    assert_eq!(nodes.len(), 2);
    assert!(nodes.iter().any(|n| n.node_type == "telemetry" && n.ref_id == "a"));

    let edges = g.edge_summaries().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].edge_type, "e");
    assert_eq!(edges[0].valid_from, 100);
    // id is deterministic from the natural key
    assert_eq!(
        edges[0].id,
        edge_id_for(&edges[0].src, &edges[0].dst, "e", 100),
    );
}

#[test]
fn apply_node_lww_respects_timestamps() {
    let (_d, g) = store();
    let base = Node {
        id:         node_id_for("telemetry", "t1"),
        node_type:  "telemetry".into(),
        ref_id:     "t1".into(),
        attrs:      json!({"v": 5}),
        created_at: 1_000,
        updated_at: 5_000,
    };
    // absent locally → applied
    assert!(g.apply_node_lww(&base).unwrap());
    assert_eq!(g.get_node(&telem("t1")).unwrap().unwrap().attrs["v"], json!(5));

    // older update → rejected
    let older = Node { updated_at: 3_000, attrs: json!({"v": 1}), ..base.clone() };
    assert!(!g.apply_node_lww(&older).unwrap());
    assert_eq!(g.get_node(&telem("t1")).unwrap().unwrap().attrs["v"], json!(5));

    // newer update → applied, and the FTS index follows
    let newer = Node { updated_at: 9_000, attrs: json!({"v": 7, "msg": "newer-state"}), ..base.clone() };
    assert!(g.apply_node_lww(&newer).unwrap());
    assert_eq!(g.get_node(&telem("t1")).unwrap().unwrap().attrs["v"], json!(7));
    assert_eq!(g.search_nodes("newer-state", 10).unwrap().len(), 1);
}

#[test]
fn apply_edge_lww_converges_replica() {
    let (_d, g) = store();
    g.add_node(&telem("a"), json!({})).unwrap();
    g.add_node(&telem("b"), json!({})).unwrap();
    let edge = Edge {
        id:         Uuid::nil(),
        src:        node_id_for("telemetry", "a"),
        dst:        node_id_for("telemetry", "b"),
        edge_type:  "calls".into(),
        weight:     2.0,
        directed:   true,
        attrs:      json!({}),
        valid_from: 100,
        valid_to:   OPEN_END,
        created_at: 100,
        updated_at: 5_000,
    };
    assert!(g.apply_edge_lww(&edge).unwrap());
    let stored = g.outgoing(&telem("a"), &EdgeFilter::new()).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].weight, 2.0);

    // stale update rejected, fresh update applied
    let stale = Edge { updated_at: 3_000, weight: 99.0, ..edge.clone() };
    assert!(!g.apply_edge_lww(&stale).unwrap());
    let fresh = Edge { updated_at: 9_000, weight: 4.0, ..edge.clone() };
    assert!(g.apply_edge_lww(&fresh).unwrap());
    assert_eq!(g.outgoing(&telem("a"), &EdgeFilter::new()).unwrap()[0].weight, 4.0);
}
