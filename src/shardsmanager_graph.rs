//! `ShardsManager` delegating surface for the relationship
//! [`GraphStore`](crate::graphstorage::GraphStore).
//!
//! The graph store lives at `{dbpath}/graph/` and is opened lazily on
//! first use (see [`LazyGraph`](crate::graphstorage::LazyGraph)).
//! Every method here is a thin pass-through; call [`ShardsManager::graph`]
//! directly when you need the `GraphStore` handle itself.

use crate::common::error::Result;
use crate::graphstorage::{
    Direction, Edge, EdgeFilter, EdgeSpec, EdgeSummary, GraphFingerprint, GraphIntegrityReport,
    GraphRepairOpts, GraphRepairReport, GraphStats, GraphStore, Node, NodeRef, NodeSummary, Path,
    TraversalHit, TraversalOpts,
};
use crate::shardsmanager::ShardsManager;
use serde_json::Value as JsonValue;
use uuid::Uuid;

impl ShardsManager {
    /// The lazily-opened relationship graph store.  Cheap to call
    /// repeatedly — the underlying DuckDB + Tantivy engines are opened
    /// once and shared.
    pub fn graph(&self) -> Result<GraphStore> {
        self.graph.get()
    }

    // ── nodes ─────────────────────────────────────────────────────────────────

    pub fn graph_add_node(&self, n: &NodeRef, attrs: JsonValue) -> Result<Uuid> {
        self.graph()?.add_node(n, attrs)
    }

    pub fn graph_get_node(&self, n: &NodeRef) -> Result<Option<Node>> {
        self.graph()?.get_node(n)
    }

    /// Fetch a node by its `Uuid` (used by the replication / AE path).
    pub fn graph_get_node_by_id(&self, id: &Uuid) -> Result<Option<Node>> {
        self.graph()?.get_node_by_id(id)
    }

    /// Fetch an edge by its `Uuid` (used by the replication / AE path).
    pub fn graph_get_edge(&self, edge_id: &Uuid) -> Result<Option<Edge>> {
        self.graph()?.get_edge(edge_id)
    }

    pub fn graph_remove_node(&self, n: &NodeRef) -> Result<bool> {
        self.graph()?.remove_node(n)
    }

    /// Create a `group` node linked to each member (group → member).
    pub fn graph_add_node_group(
        &self,
        group: &NodeRef,
        group_attrs: JsonValue,
        members: &[NodeRef],
        member_edge: &str,
    ) -> Result<Uuid> {
        self.graph()?.add_node_group(group, group_attrs, members, member_edge)
    }

    // ── edges ─────────────────────────────────────────────────────────────────

    pub fn graph_register_edge_type(
        &self,
        name: &str,
        default_weight: f64,
        default_directed: bool,
        attrs: JsonValue,
    ) -> Result<()> {
        self.graph()?.register_edge_type(name, default_weight, default_directed, attrs)
    }

    pub fn graph_link(&self, from: &NodeRef, to: &NodeRef, spec: EdgeSpec) -> Result<Uuid> {
        self.graph()?.link(from, to, spec)
    }

    pub fn graph_link_batch(&self, links: &[(NodeRef, NodeRef, EdgeSpec)]) -> Result<usize> {
        self.graph()?.link_batch(links)
    }

    /// Delete an edge; returns the ids of the removed edge episodes.
    pub fn graph_unlink(
        &self,
        from: &NodeRef,
        to: &NodeRef,
        edge_type: &str,
    ) -> Result<Vec<Uuid>> {
        self.graph()?.unlink(from, to, edge_type)
    }

    /// Close an edge's validity window; returns the ids of the edges updated.
    pub fn graph_expire_edge(
        &self,
        from: &NodeRef,
        to: &NodeRef,
        edge_type: &str,
        at: i64,
    ) -> Result<Vec<Uuid>> {
        self.graph()?.expire_edge(from, to, edge_type, at)
    }

    pub fn graph_set_weight(&self, edge_id: &Uuid, weight: f64) -> Result<bool> {
        self.graph()?.set_weight(edge_id, weight)
    }

    // ── neighbour queries ─────────────────────────────────────────────────────

    pub fn graph_outgoing(&self, n: &NodeRef, f: &EdgeFilter) -> Result<Vec<Edge>> {
        self.graph()?.outgoing(n, f)
    }

    pub fn graph_incoming(&self, n: &NodeRef, f: &EdgeFilter) -> Result<Vec<Edge>> {
        self.graph()?.incoming(n, f)
    }

    pub fn graph_neighbors(
        &self,
        n: &NodeRef,
        dir: Direction,
        f: &EdgeFilter,
    ) -> Result<Vec<Node>> {
        self.graph()?.neighbors(n, dir, f)
    }

    pub fn graph_degree(&self, n: &NodeRef, dir: Direction) -> Result<u64> {
        self.graph()?.degree(n, dir)
    }

    // ── traversal ─────────────────────────────────────────────────────────────

    pub fn graph_traverse(
        &self,
        start: &NodeRef,
        opts: &TraversalOpts,
    ) -> Result<Vec<TraversalHit>> {
        self.graph()?.traverse(start, opts)
    }

    pub fn graph_reachable(
        &self,
        from: &NodeRef,
        opts: &TraversalOpts,
    ) -> Result<Vec<NodeRef>> {
        self.graph()?.reachable(from, opts)
    }

    pub fn graph_shortest_path(
        &self,
        from: &NodeRef,
        to: &NodeRef,
        opts: &TraversalOpts,
    ) -> Result<Option<Path>> {
        self.graph()?.shortest_path(from, to, opts)
    }

    // ── full-text search over node metadata ───────────────────────────────────

    pub fn graph_search_nodes(&self, query: &str, limit: usize) -> Result<Vec<(Node, f32)>> {
        self.graph()?.search_nodes(query, limit)
    }

    pub fn graph_search_nodes_typed(
        &self,
        query: &str,
        types: &[&str],
        limit: usize,
    ) -> Result<Vec<(Node, f32)>> {
        self.graph()?.search_nodes_typed(query, types, limit)
    }

    pub fn graph_reindex_fts(&self) -> Result<u64> {
        self.graph()?.reindex_fts()
    }

    // ── maintenance ───────────────────────────────────────────────────────────

    pub fn graph_sync(&self) -> Result<()> {
        self.graph()?.sync()
    }

    pub fn graph_stats(&self) -> Result<GraphStats> {
        self.graph()?.stats()
    }

    // ── self-healing ──────────────────────────────────────────────────────────

    /// Eager-validate the graph's DuckDB + Tantivy engines.
    pub fn graph_probe(&self) -> Result<()> {
        self.graph()?.probe()
    }

    /// Read-only integrity scan (dangling edges, temporal inversions,
    /// FTS drift).
    pub fn graph_verify(&self) -> Result<GraphIntegrityReport> {
        self.graph()?.verify()
    }

    /// Detect + repair store-internal inconsistency.
    pub fn graph_repair(&self, opts: &GraphRepairOpts) -> Result<GraphRepairReport> {
        self.graph()?.repair(opts)
    }

    /// Wipe + rebuild the FTS index from the authoritative `nodes` table.
    pub fn graph_rebuild_fts(&self) -> Result<u64> {
        self.graph()?.rebuild_fts()
    }

    /// Cheap whole-store divergence fingerprint (for replica comparison).
    pub fn graph_fingerprint(&self) -> Result<GraphFingerprint> {
        self.graph()?.fingerprint()
    }

    // ── cluster replication / anti-entropy ────────────────────────────────────
    //
    // These back the v2/graph.* receivers and the anti-entropy sweep —
    // see `src/bin/bdsnode/jsonrpc/v2_graph.rs` and
    // `src/bin/bdsnode/server/cluster.rs`.

    /// Enumerate every node's natural key + LWW timestamp (AE diff input).
    pub fn graph_node_summaries(&self) -> Result<Vec<NodeSummary>> {
        self.graph()?.node_summaries()
    }

    /// Enumerate every edge's natural key + LWW timestamp (AE diff input).
    pub fn graph_edge_summaries(&self) -> Result<Vec<EdgeSummary>> {
        self.graph()?.edge_summaries()
    }

    /// Idempotent LWW upsert of a replica-sourced node (cache + FTS coherent).
    pub fn graph_apply_node_lww(&self, node: &Node) -> Result<bool> {
        self.graph()?.apply_node_lww(node)
    }

    /// Idempotent LWW upsert of a replica-sourced edge (cache coherent).
    pub fn graph_apply_edge_lww(&self, edge: &Edge) -> Result<bool> {
        self.graph()?.apply_edge_lww(edge)
    }

    /// Delete a single edge by id — used by anti-entropy to apply a
    /// peer's edge tombstone.
    pub fn graph_delete_edge(&self, edge_id: &Uuid) -> Result<bool> {
        self.graph()?.delete_edge(edge_id)
    }
}
