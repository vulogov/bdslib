/// graph_demo — A tour of the GraphStore relationship layer.
///
/// Sections:
///   1. Setup            — open an in-memory-ish GraphStore in a temp dir
///   2. Nodes            — typed entities (telemetry, document, signal, group)
///   3. Edges            — directed / undirected / weighted, with edge-type defaults
///   4. Groups           — a "group of telemetry items" as a first-class node
///   5. Neighbour queries — outgoing / incoming / degree, with filters
///   6. Blast radius     — bounded BFS traversal of a dependency graph
///   7. Temporal edges   — time-bounded relationships + point-in-time scope
///   8. Signal analysis  — weighted correlation edges + shortest path
///   9. FTS search       — find nodes by their metadata
///  10. Stats            — counts + cache effectiveness
use bdslib::graphstorage::{
    Direction, EdgeFilter, EdgeSpec, GraphStore, NodeRef, Strategy, TimeScope, TraversalOpts,
};
use serde_json::json;
use tempfile::TempDir;

fn telem(id: &str) -> NodeRef { NodeRef::new("telemetry", id) }
fn signal(id: &str) -> NodeRef { NodeRef::new("signal", id) }

fn main() {
    // ── 1. Setup ──────────────────────────────────────────────────────────────
    let dir = TempDir::new().unwrap();
    let g = GraphStore::open(dir.path().to_str().unwrap(), 4).unwrap();
    println!("=== GraphStore opened at {} ===\n", dir.path().display());

    // ── 2. Nodes ──────────────────────────────────────────────────────────────
    g.add_node(&telem("checkout-svc"), json!({"service": "checkout", "tier": "edge"})).unwrap();
    g.add_node(&telem("payments-svc"), json!({"service": "payments", "tier": "core"})).unwrap();
    g.add_node(&telem("ledger-db"),    json!({"service": "ledger",   "tier": "data"})).unwrap();
    g.add_node(&NodeRef::new("document", "payments-runbook"),
               json!({"title": "Payments incident runbook"})).unwrap();
    println!("2. Added 3 telemetry nodes + 1 document\n");

    // ── 3. Edges (directed, weighted, configured) ─────────────────────────────
    // Register an edge type so links can inherit its defaults.
    g.register_edge_type("depends_on", 1.0, true, json!({"category": "topology"})).unwrap();
    g.link(&telem("checkout-svc"), &telem("payments-svc"),
           g.edge_spec("depends_on").unwrap().weight(2.0)).unwrap();
    g.link(&telem("payments-svc"), &telem("ledger-db"),
           g.edge_spec("depends_on").unwrap()).unwrap();
    g.link(&telem("payments-svc"), &NodeRef::new("document", "payments-runbook"),
           EdgeSpec::new("documented_by").undirected()).unwrap();
    println!("3. Linked the dependency topology (checkout → payments → ledger)\n");

    // ── 4. Groups ─────────────────────────────────────────────────────────────
    // A "group of telemetry items" is a first-class node + membership edges.
    g.add_node_group(
        &NodeRef::new("group", "incident-2026-05-14"),
        json!({"summary": "payment latency spike"}),
        &[telem("checkout-svc"), telem("payments-svc")],
        "has_member",
    ).unwrap();
    let members = g.outgoing(
        &NodeRef::new("group", "incident-2026-05-14"),
        &EdgeFilter::new().edge_type("has_member"),
    ).unwrap();
    println!("4. Incident group has {} member telemetry items\n", members.len());

    // ── 5. Neighbour queries ──────────────────────────────────────────────────
    let out = g.outgoing(&telem("payments-svc"), &EdgeFilter::new()).unwrap();
    let inc = g.incoming(&telem("payments-svc"), &EdgeFilter::new()).unwrap();
    println!("5. payments-svc: {} outgoing, {} incoming edges; out-degree = {}\n",
             out.len(), inc.len(), g.degree(&telem("payments-svc"), Direction::Out).unwrap());

    // ── 6. Blast radius — bounded BFS down the dependency graph ───────────────
    let opts = TraversalOpts::new(Direction::Out, 10, 1000);
    let blast = g.traverse(&telem("checkout-svc"), &opts).unwrap();
    println!("6. Blast radius of checkout-svc ({} entities affected):", blast.len());
    for hit in &blast {
        println!("     depth {} → {}:{}", hit.depth, hit.node.node_type, hit.node.ref_id);
    }
    println!();

    // ── 7. Temporal edges ─────────────────────────────────────────────────────
    // A relationship that only held during a window.
    g.link(&telem("checkout-svc"), &telem("ledger-db"),
           EdgeSpec::new("hot_path").valid(1_000, 2_000)).unwrap();
    let all_time   = g.outgoing(&telem("checkout-svc"), &EdgeFilter::new()).unwrap().len();
    let at_t1500   = g.outgoing(&telem("checkout-svc"),
                       &EdgeFilter::new().time(TimeScope::At(1_500))).unwrap().len();
    let at_t9000   = g.outgoing(&telem("checkout-svc"),
                       &EdgeFilter::new().time(TimeScope::At(9_000))).unwrap().len();
    println!("7. checkout-svc outgoing edges — all-time: {all_time}, at t=1500: {at_t1500}, at t=9000: {at_t9000}\n");

    // ── 8. Signal relationship analysis ───────────────────────────────────────
    // Undirected weighted correlation edges; weight = correlation strength.
    g.link(&signal("latency-high"),  &signal("error-rate-up"),
           EdgeSpec::new("correlates_with").undirected().weight(0.4)).unwrap();
    g.link(&signal("error-rate-up"), &signal("payment-failures"),
           EdgeSpec::new("correlates_with").undirected().weight(0.9)).unwrap();
    g.link(&signal("latency-high"),  &signal("payment-failures"),
           EdgeSpec::new("correlates_with").undirected().weight(0.2)).unwrap();
    // Strongest-correlation path (Dijkstra would minimise weight; here we
    // just show the shortest hop path between two signals).
    let path = g.shortest_path(
        &signal("latency-high"), &signal("payment-failures"),
        &TraversalOpts::new(Direction::Both, 10, 1000).strategy(Strategy::Bfs),
    ).unwrap();
    if let Some(p) = path {
        let chain: Vec<_> = p.nodes.iter().map(|n| n.ref_id.clone()).collect();
        println!("8. Signal path latency-high → payment-failures: {}\n", chain.join(" → "));
    }

    // ── 9. FTS search over node metadata ──────────────────────────────────────
    let hits = g.search_nodes("payments", 10).unwrap();
    println!("9. Nodes whose metadata matches \"payments\":");
    for (node, score) in &hits {
        println!("     {:.3}  {}:{}", score, node.node_type, node.ref_id);
    }
    println!();

    // ── 10. Stats ─────────────────────────────────────────────────────────────
    g.sync().unwrap();
    let s = g.stats().unwrap();
    println!("10. Stats: {} nodes, {} edges, {} edge types, {} FTS docs",
             s.node_count, s.edge_count, s.edge_type_count, s.fts_doc_count);
    println!("    Cache: resolve {}h/{}m, node {}h/{}m, adjacency {}h/{}m",
             s.cache.resolve_hits, s.cache.resolve_misses,
             s.cache.node_hits, s.cache.node_misses,
             s.cache.adj_hits, s.cache.adj_misses);
}
