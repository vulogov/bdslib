//! `GraphStore` — a directed/undirected weighted property graph over
//! the entities ShardsManager manages (telemetry items, groups,
//! documents, signals, …).
//!
//! ## Layout
//!
//! Self-contained directory, opened lazily by [`ShardsManager`]:
//!
//! ```text
//! {dbpath}/graph/
//! ├── graph.duckdb   ← StorageEngine — nodes, edges, edge_types (source of truth)
//! └── fts/           ← FTSEngine (Tantivy) — node-metadata full-text index
//! ```
//!
//! DuckDB is authoritative; the Tantivy index is a derived,
//! rebuildable search index over node metadata — the same split
//! [`DocumentStorage`](crate::documentstorage::DocumentStorage) and
//! [`Shard`](crate::shard::Shard) use.
//!
//! ## Model
//!
//! - **Nodes** are typed and identified by a `(node_type, ref_id)`
//!   natural key plus a stable `Uuid`.  A "group of telemetry items"
//!   is a first-class `group` node linked to its members — not a
//!   special storage construct.
//! - **Edges** are typed, weighted, directed-or-not, and **time-bounded**
//!   (`valid_from` / `valid_to`).  An open edge uses the [`OPEN_END`]
//!   sentinel for `valid_to`.  The same `(src, dst, edge_type)` may
//!   recur as distinct time episodes.
//!
//! ## Caching
//!
//! All point-lookup / traversal work rides [`GraphCache`]: a
//! `(type,ref)→id` resolve cache, a node-metadata cache, and a
//! full-adjacency cache per node.  `EdgeFilter` is applied in Rust on
//! a cache hit, so filter variety never explodes the key space.
//! Supernodes (degree over [`GraphStore::supernode_degree`]) are not
//! adjacency-cached.

use crate::common::cache_graph::{GraphCache, GraphCacheStats};
use crate::common::error::{err_msg, Result};
use crate::fts::FTSEngine;
use crate::storageengine::{SqlParam, StorageEngine};
use rust_dynamic::value::Value as DynamicValue;
use serde_json::Value as JsonValue;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// `valid_to` sentinel for an edge with no end — "still valid".
/// Keeps every temporal predicate a plain range comparison.
pub const OPEN_END: i64 = i64::MAX;

const DEFAULT_CAP_RESOLVE:      usize = 50_000;
const DEFAULT_CAP_NODES:        usize = 20_000;
const DEFAULT_CAP_ADJ:          usize = 10_000;
const DEFAULT_SUPERNODE_DEGREE: usize = 10_000;

/// Fixed namespace UUID for deterministic graph-entity ids.  Every
/// node/edge id is a UUIDv5 of its natural key under this namespace,
/// so the *same* logical entity gets the *same* id on every replica —
/// the foundation that lets anti-entropy / LWW converge a replicated
/// graph (an edge created independently on two nodes is one row, not
/// two).  An arbitrary constant; never change it for an existing dbpath.
const GRAPH_NS: Uuid = Uuid::from_u128(0x62_64_73_6c_69_62_2d_67_72_61_70_68_2d_6e_73_31);

/// Deterministic id for a node from its `(node_type, ref_id)` natural key.
pub fn node_id_for(node_type: &str, ref_id: &str) -> Uuid {
    Uuid::new_v5(&GRAPH_NS, format!("node\u{1f}{node_type}\u{1f}{ref_id}").as_bytes())
}

/// Deterministic id for an edge from its `(src, dst, edge_type,
/// valid_from)` natural key.
pub fn edge_id_for(src: &Uuid, dst: &Uuid, edge_type: &str, valid_from: i64) -> Uuid {
    Uuid::new_v5(
        &GRAPH_NS,
        format!("edge\u{1f}{src}\u{1f}{dst}\u{1f}{edge_type}\u{1f}{valid_from}").as_bytes(),
    )
}

/// Schema — UUIDs stored as TEXT (works with `StorageEngine`'s row
/// extraction and aligns with `FTSEngine`'s string keys); `attrs`
/// stored as TEXT containing JSON.  Fully idempotent.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS nodes (
  node_id    TEXT PRIMARY KEY,
  node_type  TEXT   NOT NULL,
  ref_id     TEXT   NOT NULL,
  attrs      TEXT   NOT NULL DEFAULT '{}',
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_nodes_natural ON nodes(node_type, ref_id);

CREATE TABLE IF NOT EXISTS edges (
  edge_id    TEXT PRIMARY KEY,
  src        TEXT   NOT NULL,
  dst        TEXT   NOT NULL,
  edge_type  TEXT   NOT NULL,
  weight     DOUBLE NOT NULL DEFAULT 1.0,
  directed   BOOLEAN NOT NULL DEFAULT TRUE,
  attrs      TEXT   NOT NULL DEFAULT '{}',
  valid_from BIGINT NOT NULL,
  valid_to   BIGINT NOT NULL DEFAULT 9223372036854775807,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_edges_triple ON edges(src, dst, edge_type, valid_from);
CREATE INDEX IF NOT EXISTS ix_edges_src   ON edges(src, edge_type);
CREATE INDEX IF NOT EXISTS ix_edges_dst   ON edges(dst, edge_type);
CREATE INDEX IF NOT EXISTS ix_edges_valid ON edges(valid_from, valid_to);

CREATE TABLE IF NOT EXISTS edge_types (
  edge_type        TEXT PRIMARY KEY,
  default_weight   DOUBLE  NOT NULL DEFAULT 1.0,
  default_directed BOOLEAN NOT NULL DEFAULT TRUE,
  attrs            TEXT    NOT NULL DEFAULT '{}'
);
";

const NODE_COLS: &str = "node_id, node_type, ref_id, attrs, created_at, updated_at";
const EDGE_COLS: &str =
    "edge_id, src, dst, edge_type, weight, directed, attrs, valid_from, valid_to, created_at, updated_at";

// ── public types ─────────────────────────────────────────────────────────────

/// Natural identity of a node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeRef {
    pub node_type: String,
    pub ref_id:    String,
}

impl NodeRef {
    pub fn new(node_type: impl Into<String>, ref_id: impl Into<String>) -> Self {
        Self { node_type: node_type.into(), ref_id: ref_id.into() }
    }
}

/// A stored node.
#[derive(Debug, Clone)]
pub struct Node {
    pub id:         Uuid,
    pub node_type:  String,
    pub ref_id:     String,
    pub attrs:      JsonValue,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Node {
    pub fn node_ref(&self) -> NodeRef {
        NodeRef::new(self.node_type.clone(), self.ref_id.clone())
    }
}

/// A stored edge.
#[derive(Debug, Clone)]
pub struct Edge {
    pub id:         Uuid,
    pub src:        Uuid,
    pub dst:        Uuid,
    pub edge_type:  String,
    pub weight:     f64,
    pub directed:   bool,
    pub attrs:      JsonValue,
    pub valid_from: i64,
    pub valid_to:   i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Edge {
    /// The endpoint of this edge that is not `id` (the "other side").
    pub fn other(&self, id: &Uuid) -> Uuid {
        if &self.src == id { self.dst } else { self.src }
    }
}

/// Full specification of an edge to create.  Use [`EdgeSpec::new`] +
/// the builder setters; `weight` defaults to `1.0`, `directed` to
/// `true`, `valid_from` to "now", `valid_to` to open-ended.
#[derive(Debug, Clone)]
pub struct EdgeSpec {
    pub edge_type:  String,
    pub weight:     f64,
    pub directed:   bool,
    pub attrs:      JsonValue,
    pub valid_from: Option<i64>,
    pub valid_to:   Option<i64>,
}

impl EdgeSpec {
    pub fn new(edge_type: impl Into<String>) -> Self {
        Self {
            edge_type:  edge_type.into(),
            weight:     1.0,
            directed:   true,
            attrs:      JsonValue::Object(Default::default()),
            valid_from: None,
            valid_to:   None,
        }
    }
    pub fn weight(mut self, w: f64) -> Self { self.weight = w; self }
    pub fn directed(mut self, d: bool) -> Self { self.directed = d; self }
    pub fn undirected(mut self) -> Self { self.directed = false; self }
    pub fn attrs(mut self, a: JsonValue) -> Self { self.attrs = a; self }
    pub fn valid(mut self, from: i64, to: i64) -> Self {
        self.valid_from = Some(from);
        self.valid_to = Some(to);
        self
    }
}

/// Temporal scope for an [`EdgeFilter`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeScope {
    /// No temporal filtering — the cumulative graph (default).
    All,
    /// Edges valid at instant `t`: `valid_from <= t < valid_to`.
    At(i64),
    /// Edges active during `[a, b)`: `valid_from < b AND valid_to > a`.
    Overlap(i64, i64),
}

impl Default for TimeScope {
    fn default() -> Self { TimeScope::All }
}

impl TimeScope {
    fn matches(&self, valid_from: i64, valid_to: i64) -> bool {
        match *self {
            TimeScope::All            => true,
            TimeScope::At(t)          => valid_from <= t && t < valid_to,
            TimeScope::Overlap(a, b)  => valid_from < b && valid_to > a,
        }
    }
}

/// Post-cache filter applied to a node's adjacency.  All fields
/// optional; `time` defaults to [`TimeScope::All`].
#[derive(Debug, Clone, Default)]
pub struct EdgeFilter {
    pub edge_types: Option<Vec<String>>,
    pub min_weight: Option<f64>,
    pub limit:      Option<usize>,
    pub time:       TimeScope,
}

impl EdgeFilter {
    pub fn new() -> Self { Self::default() }
    pub fn edge_type(mut self, t: impl Into<String>) -> Self {
        self.edge_types.get_or_insert_with(Vec::new).push(t.into());
        self
    }
    pub fn min_weight(mut self, w: f64) -> Self { self.min_weight = Some(w); self }
    pub fn limit(mut self, n: usize) -> Self { self.limit = Some(n); self }
    pub fn time(mut self, t: TimeScope) -> Self { self.time = t; self }

    fn keep(&self, e: &Edge) -> bool {
        if let Some(types) = &self.edge_types {
            if !types.iter().any(|t| t == &e.edge_type) {
                return false;
            }
        }
        if let Some(min) = self.min_weight {
            if e.weight < min {
                return false;
            }
        }
        self.time.matches(e.valid_from, e.valid_to)
    }
}

/// Direction of edges to follow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction { Out, In, Both }

/// Traversal strategy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Strategy {
    /// Breadth-first; path cost = hop count.
    Bfs,
    /// Lowest cumulative `weight`; path cost = summed edge weights.
    Dijkstra,
}

/// Bounded-traversal options.  `max_depth` and `max_nodes` are
/// **required** — every traversal is bounded so a high-load caller
/// can't launch a runaway query.
#[derive(Debug, Clone)]
pub struct TraversalOpts {
    pub direction:   Direction,
    pub max_depth:   usize,
    pub max_nodes:   usize,
    pub edge_filter: EdgeFilter,
    pub strategy:    Strategy,
}

impl TraversalOpts {
    pub fn new(direction: Direction, max_depth: usize, max_nodes: usize) -> Self {
        Self {
            direction,
            max_depth,
            max_nodes,
            edge_filter: EdgeFilter::default(),
            strategy:    Strategy::Bfs,
        }
    }
    pub fn edge_filter(mut self, f: EdgeFilter) -> Self { self.edge_filter = f; self }
    pub fn strategy(mut self, s: Strategy) -> Self { self.strategy = s; self }
}

/// One node reached by a traversal.
#[derive(Debug, Clone)]
pub struct TraversalHit {
    pub node:        Node,
    /// Hops from the start node (start itself is depth 0 and is not
    /// emitted as a hit).
    pub depth:       usize,
    /// Cost of the path used to first reach this node — hop count
    /// under [`Strategy::Bfs`], summed weight under [`Strategy::Dijkstra`].
    pub path_cost:   f64,
}

/// A concrete path between two nodes.
#[derive(Debug, Clone)]
pub struct Path {
    pub nodes:        Vec<Node>,
    pub edges:        Vec<Edge>,
    pub total_weight: f64,
}

/// Operational snapshot.
#[derive(Debug, Clone)]
pub struct GraphStats {
    pub node_count:     u64,
    pub edge_count:     u64,
    pub edge_type_count: u64,
    pub fts_doc_count:  u64,
    pub cache:          GraphCacheStats,
}

// ── self-healing types ───────────────────────────────────────────────────────

/// Read-only integrity scan result from [`GraphStore::verify`].
#[derive(Debug, Clone)]
pub struct GraphIntegrityReport {
    pub node_count: u64,
    pub edge_count: u64,
    /// Edges whose `src` or `dst` has no `nodes` row — meaningless
    /// edges that a replication race or corruption can leave behind.
    pub dangling_edges: u64,
    /// Edges with `valid_from >= valid_to` — a corrupt temporal range.
    pub invalid_temporal_edges: u64,
    /// Documents in the Tantivy index.
    pub fts_doc_count: u64,
    /// `fts_doc_count - node_count`.  Non-zero means the derived FTS
    /// index has drifted from the authoritative `nodes` table.
    pub fts_drift: i64,
    /// `true` when every defect counter is zero.
    pub healthy: bool,
}

/// Knobs for [`GraphStore::repair`].
#[derive(Debug, Clone)]
pub struct GraphRepairOpts {
    pub prune_dangling:  bool,
    pub prune_invalid:   bool,
    /// Rebuild the FTS index from `nodes` when [`GraphIntegrityReport::fts_drift`]
    /// is non-zero.
    pub fix_fts_drift:   bool,
    /// Report what *would* be done without mutating anything.
    pub dry_run:         bool,
}

impl Default for GraphRepairOpts {
    fn default() -> Self {
        Self { prune_dangling: true, prune_invalid: true, fix_fts_drift: true, dry_run: false }
    }
}

/// Outcome of [`GraphStore::repair`].
#[derive(Debug, Clone)]
pub struct GraphRepairReport {
    /// Integrity scan taken before any repair.
    pub before:          GraphIntegrityReport,
    pub dangling_pruned: usize,
    pub invalid_pruned:  usize,
    pub fts_rebuilt:     bool,
    pub fts_docs_after:  u64,
    pub dry_run:         bool,
}

/// Cheap, order-independent whole-store digest.  Two replicas with
/// identical content produce identical fingerprints; any divergence
/// flips a hash.  Lets a cluster cheaply detect "have these two graph
/// replicas diverged?" before paying for a full id-set diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFingerprint {
    pub node_count: u64,
    pub edge_count: u64,
    /// XOR-folded hash of every node row (id + updated_at + attrs).
    pub nodes_hash: String,
    /// XOR-folded hash of every edge row (id + weight + valid_to + updated_at + attrs).
    pub edges_hash: String,
}

/// One node's identity + LWW timestamp — the unit an anti-entropy diff
/// compares.  Keyed by the natural `(node_type, ref_id)`; `id` is
/// derivable but carried for convenience.
#[derive(Debug, Clone)]
pub struct NodeSummary {
    pub id:         Uuid,
    pub node_type:  String,
    pub ref_id:     String,
    pub updated_at: i64,
}

/// One edge's identity + LWW timestamp.  Keyed by the natural
/// `(src, dst, edge_type, valid_from)` tuple — **not** the surrogate
/// id — so anti-entropy compares logical edges, not row ids.
#[derive(Debug, Clone)]
pub struct EdgeSummary {
    pub id:         Uuid,
    pub src:        Uuid,
    pub dst:        Uuid,
    pub edge_type:  String,
    pub valid_from: i64,
    pub updated_at: i64,
}

/// Resolved on-disk paths for one graph store.
struct GraphPaths {
    db:  String,
    fts: String,
}

impl GraphPaths {
    fn from(root: &str) -> Result<Self> {
        let root = FsPath::new(root);
        std::fs::create_dir_all(root)
            .map_err(|e| err_msg(format!("cannot create graph root {root:?}: {e}")))?;
        Ok(Self {
            db:  root.join("graph.duckdb").to_string_lossy().into_owned(),
            fts: root.join("fts").to_string_lossy().into_owned(),
        })
    }
}

// ── GraphStore ───────────────────────────────────────────────────────────────

/// Directed/undirected weighted property graph.  Cheap to clone — the
/// DuckDB engine, FTS engine, and cache are all `Arc`-shared.
#[derive(Clone)]
pub struct GraphStore {
    db:    Arc<StorageEngine>,
    fts:   Arc<FTSEngine>,
    cache: GraphCache,
    /// Nodes with more edges than this are not adjacency-cached —
    /// their queries always hit indexed SQL.
    supernode_degree: usize,
}

impl GraphStore {
    /// Open (or create) a graph store rooted at `root` — typically
    /// `{dbpath}/graph`.  The Tantivy index is `probe()`d eagerly so a
    /// corrupt index fails here rather than on a later search.
    pub fn open(root: &str, pool_size: u32) -> Result<Self> {
        let paths = GraphPaths::from(root)?;
        let db = StorageEngine::new(paths.db.as_str(), SCHEMA, pool_size.max(1))?;
        let fts = FTSEngine::new(&paths.fts)?;
        fts.probe()
            .map_err(|e| err_msg(format!("graph '{root}': FTS index unusable: {e}")))?;
        Ok(Self {
            db:    Arc::new(db),
            fts:   Arc::new(fts),
            cache: GraphCache::new(DEFAULT_CAP_RESOLVE, DEFAULT_CAP_NODES, DEFAULT_CAP_ADJ),
            supernode_degree: DEFAULT_SUPERNODE_DEGREE,
        })
    }

    /// Override the adjacency-cache supernode cutoff (default 10 000).
    pub fn with_supernode_degree(mut self, degree: usize) -> Self {
        self.supernode_degree = degree;
        self
    }

    // ── edge-type registry ────────────────────────────────────────────────────

    /// Register defaults for a "configured" edge type.  `link` callers
    /// can then build an [`EdgeSpec`] from just the name via
    /// [`GraphStore::edge_spec`].
    pub fn register_edge_type(
        &self,
        name: &str,
        default_weight: f64,
        default_directed: bool,
        attrs: JsonValue,
    ) -> Result<()> {
        self.db.execute_params(
            "INSERT INTO edge_types (edge_type, default_weight, default_directed, attrs)
             VALUES (?, ?, ?, ?)
             ON CONFLICT (edge_type) DO UPDATE SET
               default_weight   = excluded.default_weight,
               default_directed = excluded.default_directed,
               attrs            = excluded.attrs",
            &[
                SqlParam::Text(name.to_owned()),
                SqlParam::Real(default_weight),
                SqlParam::Bool(default_directed),
                SqlParam::Text(attrs.to_string()),
            ],
        )?;
        Ok(())
    }

    /// Build an [`EdgeSpec`] from a registered edge type, inheriting
    /// its default weight / directedness.  Falls back to plain
    /// `EdgeSpec::new` defaults when the type was never registered.
    pub fn edge_spec(&self, edge_type: &str) -> Result<EdgeSpec> {
        let rows = self.db.select_all_params(
            "SELECT default_weight, default_directed, attrs FROM edge_types WHERE edge_type = ?",
            &[SqlParam::Text(edge_type.to_owned())],
        )?;
        match rows.first() {
            Some(r) => Ok(EdgeSpec {
                edge_type:  edge_type.to_owned(),
                weight:     dyn_float(&r[0])?,
                directed:   dyn_bool(&r[1])?,
                attrs:      dyn_json(&r[2])?,
                valid_from: None,
                valid_to:   None,
            }),
            None => Ok(EdgeSpec::new(edge_type)),
        }
    }

    // ── nodes ─────────────────────────────────────────────────────────────────

    /// Resolve a [`NodeRef`] to its `Uuid`, or `None` if it does not
    /// exist.  Cached.
    pub fn resolve(&self, n: &NodeRef) -> Result<Option<Uuid>> {
        if let Some(id) = self.cache.get_resolve(&n.node_type, &n.ref_id) {
            return Ok(Some(id));
        }
        let rows = self.db.select_all_params(
            "SELECT node_id FROM nodes WHERE node_type = ? AND ref_id = ?",
            &[SqlParam::Text(n.node_type.clone()), SqlParam::Text(n.ref_id.clone())],
        )?;
        match rows.first() {
            Some(r) => {
                let id = dyn_uuid(&r[0])?;
                self.cache.put_resolve(&n.node_type, &n.ref_id, id);
                Ok(Some(id))
            }
            None => Ok(None),
        }
    }

    /// Insert a node, or update its `attrs` if `(node_type, ref_id)`
    /// already exists.  Returns the node's deterministic `Uuid` (a
    /// UUIDv5 of the natural key — identical on every replica).
    /// Re-indexes the node's metadata in the FTS index.  `created_at`
    /// is preserved across updates.
    pub fn add_node(&self, n: &NodeRef, attrs: JsonValue) -> Result<Uuid> {
        let id = node_id_for(&n.node_type, &n.ref_id);
        let now = now_secs();
        let attrs_s = attrs.to_string();
        let body = fts_body(&n.node_type, &n.ref_id, &attrs);

        // One idempotent upsert keyed on the deterministic id; the
        // ON CONFLICT clause preserves created_at.
        self.db.execute_params(
            &format!(
                "INSERT INTO nodes ({NODE_COLS}) VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT (node_id) DO UPDATE SET
                   attrs = excluded.attrs, updated_at = excluded.updated_at"
            ),
            &[
                SqlParam::Text(id.to_string()),
                SqlParam::Text(n.node_type.clone()),
                SqlParam::Text(n.ref_id.clone()),
                SqlParam::Text(attrs_s),
                SqlParam::Int(now),
                SqlParam::Int(now),
            ],
        )?;

        // FTS is a derived index — DuckDB above is the source of truth.
        self.fts.add_document_with_id(id, &body)?;
        // Drop any stale cached metadata; the fresh row is lazily
        // re-cached on the next read.
        self.cache.invalidate_node(&id, Some((&n.node_type, &n.ref_id)));
        self.cache.put_resolve(&n.node_type, &n.ref_id, id);
        Ok(id)
    }

    /// Resolve `n`, creating it with empty attrs if it does not exist.
    /// The auto-vivify path for [`GraphStore::link`].  Returns the
    /// node's deterministic id.
    pub fn ensure_node(&self, n: &NodeRef) -> Result<Uuid> {
        let id = node_id_for(&n.node_type, &n.ref_id);
        if self.cache.get_resolve(&n.node_type, &n.ref_id).is_some() {
            return Ok(id); // known to exist
        }
        let now = now_secs();
        // Create the bare node only if absent — an existing row (and
        // its attrs) is left untouched.
        let inserted = self.db.execute_params(
            &format!(
                "INSERT INTO nodes ({NODE_COLS}) VALUES (?, ?, ?, '{{}}', ?, ?)
                 ON CONFLICT (node_id) DO NOTHING"
            ),
            &[
                SqlParam::Text(id.to_string()),
                SqlParam::Text(n.node_type.clone()),
                SqlParam::Text(n.ref_id.clone()),
                SqlParam::Int(now),
                SqlParam::Int(now),
            ],
        )?;
        // Only a freshly-created node needs an FTS entry.
        if inserted > 0 {
            let body = fts_body(&n.node_type, &n.ref_id, &JsonValue::Object(Default::default()));
            self.fts.add_document_with_id(id, &body)?;
        }
        self.cache.put_resolve(&n.node_type, &n.ref_id, id);
        Ok(id)
    }

    /// Fetch a node by natural key.  Cached.
    pub fn get_node(&self, n: &NodeRef) -> Result<Option<Node>> {
        match self.resolve(n)? {
            Some(id) => self.get_node_by_id(&id),
            None => Ok(None),
        }
    }

    /// Fetch a node by `Uuid`.  Cached.
    pub fn get_node_by_id(&self, id: &Uuid) -> Result<Option<Node>> {
        if let Some(node) = self.cache.get_node(id) {
            return Ok(Some(node));
        }
        let rows = self.db.select_all_params(
            &format!("SELECT {NODE_COLS} FROM nodes WHERE node_id = ?"),
            &[SqlParam::Text(id.to_string())],
        )?;
        match rows.first() {
            Some(r) => {
                let node = row_to_node(r)?;
                self.cache.put_node(node.clone());
                Ok(Some(node))
            }
            None => Ok(None),
        }
    }

    /// Create a `group` node and link it to each member via
    /// `member_edge` edges (group → member), auto-vivifying any member
    /// that does not yet exist.  One DuckDB transaction for the edges.
    pub fn add_node_group(
        &self,
        group: &NodeRef,
        group_attrs: JsonValue,
        members: &[NodeRef],
        member_edge: &str,
    ) -> Result<Uuid> {
        let group_id = self.add_node(group, group_attrs)?;
        let now = now_secs();
        let mut stmts: Vec<(String, Vec<SqlParam>)> = Vec::with_capacity(members.len());
        let mut endpoints: Vec<Uuid> = Vec::with_capacity(members.len() * 2);
        for m in members {
            let member_id = self.ensure_node(m)?;
            endpoints.push(member_id);
            let edge_id = edge_id_for(&group_id, &member_id, member_edge, now);
            stmts.push(edge_upsert_stmt(
                edge_id, group_id, member_id, member_edge, 1.0, true,
                &JsonValue::Object(Default::default()), now, OPEN_END, now, now,
            ));
        }
        self.db.execute_many_params(&stmts)?;
        endpoints.push(group_id);
        self.cache.invalidate_adjacency_of(&endpoints);
        Ok(group_id)
    }

    /// Delete a node and every edge touching it.  Returns `false` when
    /// the node did not exist.
    pub fn remove_node(&self, n: &NodeRef) -> Result<bool> {
        let id = match self.resolve(n)? {
            Some(id) => id,
            None => return Ok(false),
        };
        let id_s = id.to_string();
        // Neighbours first — their cached adjacency referenced this node.
        let neighbours = self.neighbour_ids(&id)?;
        self.db.execute_many_params(&[
            ("DELETE FROM edges WHERE src = ? OR dst = ?".to_owned(),
                vec![SqlParam::Text(id_s.clone()), SqlParam::Text(id_s.clone())]),
            ("DELETE FROM nodes WHERE node_id = ?".to_owned(),
                vec![SqlParam::Text(id_s.clone())]),
        ])?;
        self.fts.drop_document(id)?;
        self.cache.invalidate_node(&id, Some((&n.node_type, &n.ref_id)));
        self.cache.invalidate_adjacency_of(&neighbours);
        Ok(true)
    }

    // ── edges ─────────────────────────────────────────────────────────────────

    /// Create or update an edge `from → to`.  Both endpoints are
    /// auto-vivified if missing.  Undirected edges are stored once in
    /// canonical `src < dst` order.  The edge id is deterministic
    /// (UUIDv5 of `(src, dst, edge_type, valid_from)`) — re-`link`ing
    /// the same episode upserts the same row, and the *same* logical
    /// edge created independently on two cluster replicas converges to
    /// one row.  Returns the edge `Uuid`.
    pub fn link(&self, from: &NodeRef, to: &NodeRef, spec: EdgeSpec) -> Result<Uuid> {
        let mut src = self.ensure_node(from)?;
        let mut dst = self.ensure_node(to)?;
        if !spec.directed && src > dst {
            std::mem::swap(&mut src, &mut dst);
        }
        let now = now_secs();
        let valid_from = spec.valid_from.unwrap_or(now);
        let valid_to   = spec.valid_to.unwrap_or(OPEN_END);
        let edge_id    = edge_id_for(&src, &dst, &spec.edge_type, valid_from);

        let (sql, params) = edge_upsert_stmt(
            edge_id, src, dst, &spec.edge_type, spec.weight, spec.directed,
            &spec.attrs, valid_from, valid_to, now, now,
        );
        self.db.execute_params(&sql, &params)?;

        self.cache.invalidate_edge_endpoints(&src, &dst);
        Ok(edge_id)
    }

    /// Create many edges in one DuckDB transaction.  Endpoints are
    /// auto-vivified (resolve-cached, so repeats are cheap).  Returns
    /// the number of edges written.  Uses `ON CONFLICT` upsert
    /// semantics — for the precise edge id of a single link use
    /// [`GraphStore::link`].
    pub fn link_batch(&self, links: &[(NodeRef, NodeRef, EdgeSpec)]) -> Result<usize> {
        if links.is_empty() {
            return Ok(0);
        }
        let now = now_secs();
        let mut stmts: Vec<(String, Vec<SqlParam>)> = Vec::with_capacity(links.len());
        let mut endpoints: HashSet<Uuid> = HashSet::new();
        for (from, to, spec) in links {
            let mut src = self.ensure_node(from)?;
            let mut dst = self.ensure_node(to)?;
            if !spec.directed && src > dst {
                std::mem::swap(&mut src, &mut dst);
            }
            endpoints.insert(src);
            endpoints.insert(dst);
            let valid_from = spec.valid_from.unwrap_or(now);
            let edge_id = edge_id_for(&src, &dst, &spec.edge_type, valid_from);
            stmts.push(edge_upsert_stmt(
                edge_id, src, dst, &spec.edge_type, spec.weight, spec.directed,
                &spec.attrs, valid_from, spec.valid_to.unwrap_or(OPEN_END), now, now,
            ));
        }
        self.db.execute_many_params(&stmts)?;
        let endpoints: Vec<Uuid> = endpoints.into_iter().collect();
        self.cache.invalidate_adjacency_of(&endpoints);
        Ok(stmts.len())
    }

    /// Delete every episode of an edge between `from` and `to` of
    /// `edge_type`, in either stored orientation.  Returns the ids of
    /// the edges removed (empty when nothing matched) — the caller uses
    /// them to write per-edge tombstones for cluster replication.
    pub fn unlink(&self, from: &NodeRef, to: &NodeRef, edge_type: &str) -> Result<Vec<Uuid>> {
        let (a, b) = match (self.resolve(from)?, self.resolve(to)?) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let rows = self.db.select_all_params(
            "DELETE FROM edges WHERE edge_type = ?
             AND ((src = ? AND dst = ?) OR (src = ? AND dst = ?))
             RETURNING edge_id",
            &[
                SqlParam::Text(edge_type.to_owned()),
                SqlParam::Text(a.to_string()), SqlParam::Text(b.to_string()),
                SqlParam::Text(b.to_string()), SqlParam::Text(a.to_string()),
            ],
        )?;
        let ids: Vec<Uuid> = rows.iter().map(|r| dyn_uuid(&r[0])).collect::<Result<_>>()?;
        if !ids.is_empty() {
            self.cache.invalidate_edge_endpoints(&a, &b);
        }
        Ok(ids)
    }

    /// Fetch an edge by id.  Used by the replication coordinator to
    /// read back a just-written edge for fan-out.
    pub fn get_edge(&self, edge_id: &Uuid) -> Result<Option<Edge>> {
        let rows = self.db.select_all_params(
            &format!("SELECT {EDGE_COLS} FROM edges WHERE edge_id = ?"),
            &[SqlParam::Text(edge_id.to_string())],
        )?;
        match rows.first() {
            Some(r) => Ok(Some(row_to_edge(r)?)),
            None => Ok(None),
        }
    }

    /// Delete a single edge by id.  Returns `false` when it did not
    /// exist.  Used by anti-entropy to apply a peer's edge tombstone.
    pub fn delete_edge(&self, edge_id: &Uuid) -> Result<bool> {
        let rows = self.db.select_all_params(
            "DELETE FROM edges WHERE edge_id = ? RETURNING src, dst",
            &[SqlParam::Text(edge_id.to_string())],
        )?;
        match rows.first() {
            Some(r) => {
                let (src, dst) = (dyn_uuid(&r[0])?, dyn_uuid(&r[1])?);
                self.cache.invalidate_edge_endpoints(&src, &dst);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Close the validity window of the currently-open episode(s) of an
    /// edge by setting `valid_to = at` — the relationship "ended" but
    /// stays queryable historically.  Returns the ids of the edges
    /// updated, so the caller can read them back for cluster fan-out.
    pub fn expire_edge(
        &self,
        from: &NodeRef,
        to: &NodeRef,
        edge_type: &str,
        at: i64,
    ) -> Result<Vec<Uuid>> {
        let (a, b) = match (self.resolve(from)?, self.resolve(to)?) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let now = now_secs();
        let rows = self.db.select_all_params(
            "UPDATE edges SET valid_to = ?, updated_at = ?
             WHERE edge_type = ? AND valid_to > ?
             AND ((src = ? AND dst = ?) OR (src = ? AND dst = ?))
             RETURNING edge_id",
            &[
                SqlParam::Int(at), SqlParam::Int(now),
                SqlParam::Text(edge_type.to_owned()), SqlParam::Int(at),
                SqlParam::Text(a.to_string()), SqlParam::Text(b.to_string()),
                SqlParam::Text(b.to_string()), SqlParam::Text(a.to_string()),
            ],
        )?;
        let ids: Vec<Uuid> = rows.iter().map(|r| dyn_uuid(&r[0])).collect::<Result<_>>()?;
        if !ids.is_empty() {
            self.cache.invalidate_edge_endpoints(&a, &b);
        }
        Ok(ids)
    }

    /// Set the `weight` of an edge by id.  No-op (returns `false`) when
    /// the edge does not exist.
    pub fn set_weight(&self, edge_id: &Uuid, weight: f64) -> Result<bool> {
        let rows = self.db.select_all_params(
            "SELECT src, dst FROM edges WHERE edge_id = ?",
            &[SqlParam::Text(edge_id.to_string())],
        )?;
        let Some(r) = rows.first() else { return Ok(false); };
        let (src, dst) = (dyn_uuid(&r[0])?, dyn_uuid(&r[1])?);
        self.db.execute_params(
            "UPDATE edges SET weight = ?, updated_at = ? WHERE edge_id = ?",
            &[SqlParam::Real(weight), SqlParam::Int(now_secs()), SqlParam::Text(edge_id.to_string())],
        )?;
        self.cache.invalidate_edge_endpoints(&src, &dst);
        Ok(true)
    }

    // ── neighbour queries ─────────────────────────────────────────────────────

    /// Outgoing edges of `n` (directed edges leaving `n`, plus
    /// undirected edges touching `n`), filtered.
    pub fn outgoing(&self, n: &NodeRef, f: &EdgeFilter) -> Result<Vec<Edge>> {
        match self.resolve(n)? {
            Some(id) => Ok(apply_filter(&self.adjacency(&id, Direction::Out)?, f)),
            None => Ok(Vec::new()),
        }
    }

    /// Incoming edges of `n` (directed edges arriving at `n`, plus
    /// undirected edges touching `n`), filtered.
    pub fn incoming(&self, n: &NodeRef, f: &EdgeFilter) -> Result<Vec<Edge>> {
        match self.resolve(n)? {
            Some(id) => Ok(apply_filter(&self.adjacency(&id, Direction::In)?, f)),
            None => Ok(Vec::new()),
        }
    }

    /// Neighbour *nodes* of `n` in `dir`, filtered, hydrated.
    pub fn neighbors(&self, n: &NodeRef, dir: Direction, f: &EdgeFilter) -> Result<Vec<Node>> {
        let id = match self.resolve(n)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let edges = self.directional_edges(&id, dir, f)?;
        let ids: Vec<Uuid> = edges.iter().map(|e| e.other(&id)).collect();
        self.hydrate_nodes(&ids)
    }

    /// Edge count of `n` in `dir` — a `COUNT(*)`, so it never fetches a
    /// supernode's full adjacency.
    pub fn degree(&self, n: &NodeRef, dir: Direction) -> Result<u64> {
        let id = match self.resolve(n)? {
            Some(id) => id,
            None => return Ok(0),
        };
        let id_s = id.to_string();
        let (sql, params): (&str, Vec<SqlParam>) = match dir {
            Direction::Out => (
                "SELECT COUNT(*) FROM edges WHERE src = ? OR (directed = FALSE AND dst = ?)",
                vec![SqlParam::Text(id_s.clone()), SqlParam::Text(id_s.clone())],
            ),
            Direction::In => (
                "SELECT COUNT(*) FROM edges WHERE dst = ? OR (directed = FALSE AND src = ?)",
                vec![SqlParam::Text(id_s.clone()), SqlParam::Text(id_s.clone())],
            ),
            Direction::Both => (
                "SELECT COUNT(*) FROM edges WHERE src = ? OR dst = ?",
                vec![SqlParam::Text(id_s.clone()), SqlParam::Text(id_s.clone())],
            ),
        };
        let rows = self.db.select_all_params(sql, &params)?;
        Ok(rows.first().map(|r| dyn_int(&r[0]).unwrap_or(0)).unwrap_or(0) as u64)
    }

    // ── traversal ─────────────────────────────────────────────────────────────

    /// Bounded breadth-first / Dijkstra traversal from `start`.  Each
    /// reached node is emitted once, with the depth and path-cost of
    /// the first (BFS) or cheapest (Dijkstra) route to it.  `start`
    /// itself is not emitted.
    pub fn traverse(&self, start: &NodeRef, opts: &TraversalOpts) -> Result<Vec<TraversalHit>> {
        let start_id = match self.resolve(start)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let reached = match opts.strategy {
            Strategy::Bfs      => self.bfs(start_id, opts)?,
            Strategy::Dijkstra => self.dijkstra(start_id, opts)?,
        };
        let mut hits = Vec::with_capacity(reached.len());
        for (id, depth, cost) in reached {
            if let Some(node) = self.get_node_by_id(&id)? {
                hits.push(TraversalHit { node, depth, path_cost: cost });
            }
        }
        Ok(hits)
    }

    /// Every node reachable from `from` within the traversal bounds.
    pub fn reachable(&self, from: &NodeRef, opts: &TraversalOpts) -> Result<Vec<NodeRef>> {
        Ok(self.traverse(from, opts)?.into_iter().map(|h| h.node.node_ref()).collect())
    }

    /// Lowest-cost path from `from` to `to` within the bounds, or
    /// `None` if unreachable.  Cost is hop count under `Strategy::Bfs`,
    /// summed edge weight under `Strategy::Dijkstra`.
    pub fn shortest_path(
        &self,
        from: &NodeRef,
        to: &NodeRef,
        opts: &TraversalOpts,
    ) -> Result<Option<Path>> {
        let (src_id, dst_id) = match (self.resolve(from)?, self.resolve(to)?) {
            (Some(s), Some(d)) => (s, d),
            _ => return Ok(None),
        };
        if src_id == dst_id {
            let node = self.get_node_by_id(&src_id)?;
            return Ok(node.map(|n| Path { nodes: vec![n], edges: vec![], total_weight: 0.0 }));
        }
        // parent: reached node -> (predecessor, edge used)
        let mut parent: HashMap<Uuid, (Uuid, Edge)> = HashMap::new();
        let found = match opts.strategy {
            Strategy::Bfs      => self.bfs_to(src_id, dst_id, opts, &mut parent)?,
            Strategy::Dijkstra => self.dijkstra_to(src_id, dst_id, opts, &mut parent)?,
        };
        if !found {
            return Ok(None);
        }
        // Reconstruct from dst back to src.
        let mut edge_chain: Vec<Edge> = Vec::new();
        let mut cur = dst_id;
        while cur != src_id {
            let (prev, edge) = parent
                .get(&cur)
                .ok_or_else(|| err_msg("path reconstruction: broken parent chain"))?
                .clone();
            edge_chain.push(edge);
            cur = prev;
        }
        edge_chain.reverse();
        let total_weight: f64 = edge_chain.iter().map(|e| e.weight).sum();
        // Node chain: src, then the "other end" of each edge in order.
        let mut node_ids = vec![src_id];
        let mut walk = src_id;
        for e in &edge_chain {
            walk = e.other(&walk);
            node_ids.push(walk);
        }
        // `hydrate_nodes` does not preserve input order (it splits cache
        // hits from batch-fetched misses) — re-order by the path chain.
        let by_id: HashMap<Uuid, Node> =
            self.hydrate_nodes(&node_ids)?.into_iter().map(|n| (n.id, n)).collect();
        let nodes: Vec<Node> =
            node_ids.iter().filter_map(|id| by_id.get(id).cloned()).collect();
        Ok(Some(Path { nodes, edges: edge_chain, total_weight }))
    }

    // ── full-text search over node metadata ───────────────────────────────────

    /// Full-text search node metadata.  Mirrors
    /// `DocumentStorage::search_document_text`: the FTS index yields
    /// `(node_id, score)`, which are hydrated through the node cache.
    pub fn search_nodes(&self, query: &str, limit: usize) -> Result<Vec<(Node, f32)>> {
        let hits = self.fts.search_with_scores(query, limit)?;
        let ids: Vec<Uuid> = hits.iter().map(|(id, _)| *id).collect();
        let nodes = self.hydrate_nodes(&ids)?;
        let by_id: HashMap<Uuid, Node> = nodes.into_iter().map(|n| (n.id, n)).collect();
        Ok(hits
            .into_iter()
            .filter_map(|(id, score)| by_id.get(&id).cloned().map(|n| (n, score)))
            .collect())
    }

    /// [`search_nodes`](Self::search_nodes) restricted to one or more
    /// `node_type`s.  Over-fetches from the FTS index, then filters and
    /// truncates to `limit`.
    pub fn search_nodes_typed(
        &self,
        query: &str,
        types: &[&str],
        limit: usize,
    ) -> Result<Vec<(Node, f32)>> {
        let raw = self.search_nodes(query, limit.saturating_mul(4).max(limit))?;
        Ok(raw
            .into_iter()
            .filter(|(n, _)| types.iter().any(|t| *t == n.node_type))
            .take(limit)
            .collect())
    }

    /// Rebuild the FTS index from the DuckDB `nodes` table.  Re-adds
    /// every current node (fixing stale or missing entries); it does
    /// not garbage-collect FTS docs for nodes already deleted from
    /// DuckDB.  Returns the number of nodes re-indexed.
    pub fn reindex_fts(&self) -> Result<u64> {
        let rows = self.db.select_all(
            "SELECT node_id, node_type, ref_id, attrs FROM nodes",
        )?;
        let mut docs: Vec<(Uuid, String)> = Vec::with_capacity(rows.len());
        for r in &rows {
            let id = dyn_uuid(&r[0])?;
            let node_type = dyn_string(&r[1])?;
            let ref_id = dyn_string(&r[2])?;
            let attrs = dyn_json(&r[3])?;
            docs.push((id, fts_body(&node_type, &ref_id, &attrs)));
        }
        let n = docs.len() as u64;
        self.fts.add_documents_batch(&docs)?;
        self.cache.clear();
        Ok(n)
    }

    // ── maintenance ───────────────────────────────────────────────────────────

    /// Flush DuckDB (`CHECKPOINT`) and the Tantivy writer to disk.
    pub fn sync(&self) -> Result<()> {
        self.db.sync()?;
        self.fts.sync()?;
        Ok(())
    }

    /// Node / edge counts, FTS doc count, and cache effectiveness.
    pub fn stats(&self) -> Result<GraphStats> {
        let count = |sql: &str| -> Result<u64> {
            let rows = self.db.select_all(sql)?;
            Ok(rows.first().map(|r| dyn_int(&r[0]).unwrap_or(0)).unwrap_or(0) as u64)
        };
        Ok(GraphStats {
            node_count:      count("SELECT COUNT(*) FROM nodes")?,
            edge_count:      count("SELECT COUNT(*) FROM edges")?,
            edge_type_count: count("SELECT COUNT(*) FROM edge_types")?,
            fts_doc_count:   self.fts.doc_count().unwrap_or(0),
            cache:           self.cache.stats(),
        })
    }

    // ── self-healing primitives ───────────────────────────────────────────────

    /// Eager-validate both engines — DuckDB is reachable and the
    /// Tantivy index opens.  Mirrors `FTSEngine::probe` and the shard
    /// open-time validation: catches a corrupt store at open time
    /// rather than on a later query.
    pub fn probe(&self) -> Result<()> {
        self.db.select_all("SELECT 1")?;
        self.fts.probe()?;
        Ok(())
    }

    /// Read-only integrity scan — never mutates.  Detects dangling
    /// edges (endpoint with no node row), temporally-inverted edges
    /// (`valid_from >= valid_to`), and FTS/DuckDB count drift.
    pub fn verify(&self) -> Result<GraphIntegrityReport> {
        let count = |sql: &str| -> Result<u64> {
            let rows = self.db.select_all(sql)?;
            Ok(rows.first().map(|r| dyn_int(&r[0]).unwrap_or(0)).unwrap_or(0) as u64)
        };
        let node_count = count("SELECT COUNT(*) FROM nodes")?;
        let edge_count = count("SELECT COUNT(*) FROM edges")?;
        let dangling = count(
            "SELECT COUNT(*) FROM edges e
             WHERE NOT EXISTS (SELECT 1 FROM nodes n WHERE n.node_id = e.src)
                OR NOT EXISTS (SELECT 1 FROM nodes n WHERE n.node_id = e.dst)",
        )?;
        let invalid_temporal = count("SELECT COUNT(*) FROM edges WHERE valid_from >= valid_to")?;
        let fts_doc_count = self.fts.doc_count().unwrap_or(0);
        let fts_drift = fts_doc_count as i64 - node_count as i64;
        Ok(GraphIntegrityReport {
            node_count,
            edge_count,
            dangling_edges: dangling,
            invalid_temporal_edges: invalid_temporal,
            fts_doc_count,
            fts_drift,
            healthy: dangling == 0 && invalid_temporal == 0 && fts_drift == 0,
        })
    }

    /// Delete every edge whose `src` or `dst` has no `nodes` row.
    /// Returns the count removed; clears the adjacency cache when it
    /// removes anything.
    pub fn prune_dangling_edges(&self) -> Result<usize> {
        let n = self.db.execute_params(
            "DELETE FROM edges WHERE
               src NOT IN (SELECT node_id FROM nodes)
               OR dst NOT IN (SELECT node_id FROM nodes)",
            &[],
        )?;
        if n > 0 {
            self.cache.clear();
        }
        Ok(n)
    }

    /// Delete every edge with a corrupt temporal range
    /// (`valid_from >= valid_to`).  Returns the count removed.
    pub fn prune_invalid_edges(&self) -> Result<usize> {
        let n = self
            .db
            .execute_params("DELETE FROM edges WHERE valid_from >= valid_to", &[])?;
        if n > 0 {
            self.cache.clear();
        }
        Ok(n)
    }

    /// Wipe the Tantivy index and rebuild it from the authoritative
    /// `nodes` table.  Unlike [`reindex_fts`](Self::reindex_fts) — which
    /// only re-adds current nodes — this also garbage-collects FTS docs
    /// for nodes already deleted from DuckDB.  Returns the node count
    /// re-indexed.
    pub fn rebuild_fts(&self) -> Result<u64> {
        self.fts.rebuild()?;
        self.reindex_fts()
    }

    /// Detect and repair store-internal inconsistency: prune dangling
    /// and temporally-invalid edges, and rebuild the FTS index when it
    /// has drifted from `nodes`.  With `dry_run`, only the pre-scan is
    /// returned and nothing is mutated.
    pub fn repair(&self, opts: &GraphRepairOpts) -> Result<GraphRepairReport> {
        let before = self.verify()?;
        if opts.dry_run {
            return Ok(GraphRepairReport {
                fts_docs_after: before.fts_doc_count,
                before,
                dangling_pruned: 0,
                invalid_pruned: 0,
                fts_rebuilt: false,
                dry_run: true,
            });
        }
        let dangling_pruned =
            if opts.prune_dangling { self.prune_dangling_edges()? } else { 0 };
        let invalid_pruned =
            if opts.prune_invalid { self.prune_invalid_edges()? } else { 0 };
        let (fts_rebuilt, fts_docs_after) = if opts.fix_fts_drift && before.fts_drift != 0 {
            (true, self.rebuild_fts()?)
        } else {
            (false, before.fts_doc_count)
        };
        Ok(GraphRepairReport {
            before,
            dangling_pruned,
            invalid_pruned,
            fts_rebuilt,
            fts_docs_after,
            dry_run: false,
        })
    }

    // ── replication-convergence primitives ────────────────────────────────────

    /// Cheap, order-independent whole-store fingerprint.  Two replicas
    /// compare fingerprints to detect divergence in one scan each,
    /// before paying for a full id-set diff.  Computed entirely in
    /// DuckDB (XOR-folded per-row hashes).
    pub fn fingerprint(&self) -> Result<GraphFingerprint> {
        let nrows = self.db.select_all(
            "SELECT COUNT(*),
                    COALESCE(CAST(bit_xor(hash(
                      node_id || '\u{1f}' || CAST(updated_at AS VARCHAR) || '\u{1f}' || attrs
                    )) AS VARCHAR), '0')
             FROM nodes",
        )?;
        let erows = self.db.select_all(
            "SELECT COUNT(*),
                    COALESCE(CAST(bit_xor(hash(
                      edge_id || '\u{1f}' || CAST(weight AS VARCHAR) || '\u{1f}'
                      || CAST(valid_to AS VARCHAR) || '\u{1f}' || CAST(updated_at AS VARCHAR)
                      || '\u{1f}' || attrs
                    )) AS VARCHAR), '0')
             FROM edges",
        )?;
        let nrow = nrows.first().ok_or_else(|| err_msg("fingerprint: empty nodes result"))?;
        let erow = erows.first().ok_or_else(|| err_msg("fingerprint: empty edges result"))?;
        Ok(GraphFingerprint {
            node_count: dyn_int(&nrow[0])? as u64,
            edge_count: dyn_int(&erow[0])? as u64,
            nodes_hash: dyn_string(&nrow[1])?,
            edges_hash: dyn_string(&erow[1])?,
        })
    }

    /// Every node's identity + LWW `updated_at` — the input an
    /// anti-entropy diff compares against a peer's enumeration.
    pub fn node_summaries(&self) -> Result<Vec<NodeSummary>> {
        let rows = self
            .db
            .select_all("SELECT node_id, node_type, ref_id, updated_at FROM nodes")?;
        rows.iter()
            .map(|r| {
                Ok(NodeSummary {
                    id:         dyn_uuid(&r[0])?,
                    node_type:  dyn_string(&r[1])?,
                    ref_id:     dyn_string(&r[2])?,
                    updated_at: dyn_int(&r[3])?,
                })
            })
            .collect()
    }

    /// Every edge's natural key + LWW `updated_at`.  Keyed by
    /// `(src, dst, edge_type, valid_from)` so anti-entropy compares
    /// *logical* edges — and since ids are deterministic, a matching
    /// natural key implies a matching id across replicas.
    pub fn edge_summaries(&self) -> Result<Vec<EdgeSummary>> {
        let rows = self.db.select_all(
            "SELECT edge_id, src, dst, edge_type, valid_from, updated_at FROM edges",
        )?;
        rows.iter()
            .map(|r| {
                Ok(EdgeSummary {
                    id:         dyn_uuid(&r[0])?,
                    src:        dyn_uuid(&r[1])?,
                    dst:        dyn_uuid(&r[2])?,
                    edge_type:  dyn_string(&r[3])?,
                    valid_from: dyn_int(&r[4])?,
                    updated_at: dyn_int(&r[5])?,
                })
            })
            .collect()
    }

    /// Idempotent last-writer-wins upsert of a node received from a
    /// replica.  Writes only when the incoming `updated_at` is newer
    /// than the local row's (or the node is absent), preserving the
    /// replica's timestamps.  Keeps the FTS index and the cache
    /// coherent — the anti-entropy / replication receiver MUST use
    /// this rather than a raw `INSERT`.  The id is recomputed from the
    /// natural key, so a peer cannot inject a mismatched id.  Returns
    /// `true` when local state changed.
    pub fn apply_node_lww(&self, node: &Node) -> Result<bool> {
        let id = node_id_for(&node.node_type, &node.ref_id);
        let id_s = id.to_string();
        let local = self.db.select_all_params(
            "SELECT updated_at FROM nodes WHERE node_id = ?",
            &[SqlParam::Text(id_s.clone())],
        )?;
        if let Some(r) = local.first() {
            if dyn_int(&r[0])? >= node.updated_at {
                return Ok(false); // local is at least as fresh
            }
        }
        self.db.execute_params(
            &format!(
                "INSERT INTO nodes ({NODE_COLS}) VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT (node_id) DO UPDATE SET
                   node_type = excluded.node_type, ref_id = excluded.ref_id,
                   attrs = excluded.attrs, updated_at = excluded.updated_at"
            ),
            &[
                SqlParam::Text(id_s),
                SqlParam::Text(node.node_type.clone()),
                SqlParam::Text(node.ref_id.clone()),
                SqlParam::Text(node.attrs.to_string()),
                SqlParam::Int(node.created_at),
                SqlParam::Int(node.updated_at),
            ],
        )?;
        self.fts
            .add_document_with_id(id, &fts_body(&node.node_type, &node.ref_id, &node.attrs))?;
        self.cache
            .invalidate_node(&id, Some((&node.node_type, &node.ref_id)));
        self.cache.put_resolve(&node.node_type, &node.ref_id, id);
        Ok(true)
    }

    /// Idempotent last-writer-wins upsert of an edge received from a
    /// replica.  Writes only when the incoming `updated_at` is newer
    /// than the local row's (or the edge is absent), preserving the
    /// replica's timestamps and keeping the adjacency cache coherent.
    /// The id is recomputed from the natural key.  Returns `true` when
    /// local state changed.
    pub fn apply_edge_lww(&self, edge: &Edge) -> Result<bool> {
        let id = edge_id_for(&edge.src, &edge.dst, &edge.edge_type, edge.valid_from);
        let local = self.db.select_all_params(
            "SELECT updated_at FROM edges WHERE edge_id = ?",
            &[SqlParam::Text(id.to_string())],
        )?;
        if let Some(r) = local.first() {
            if dyn_int(&r[0])? >= edge.updated_at {
                return Ok(false);
            }
        }
        let (sql, params) = edge_upsert_stmt(
            id, edge.src, edge.dst, &edge.edge_type, edge.weight, edge.directed,
            &edge.attrs, edge.valid_from, edge.valid_to, edge.created_at, edge.updated_at,
        );
        self.db.execute_params(&sql, &params)?;
        self.cache.invalidate_edge_endpoints(&edge.src, &edge.dst);
        Ok(true)
    }

    // ── internals ─────────────────────────────────────────────────────────────

    /// Full, unfiltered adjacency of `id` in one direction — cache
    /// first, then indexed SQL.  Supernodes (degree over
    /// `supernode_degree`) are returned but not cached.
    fn adjacency(&self, id: &Uuid, dir: Direction) -> Result<Arc<Vec<Edge>>> {
        if dir == Direction::Both {
            // `Both` is the union of Out + In with edges de-duplicated
            // by id; it is not cached as its own layer.
            let out = self.adjacency(id, Direction::Out)?;
            let inc = self.adjacency(id, Direction::In)?;
            let mut seen: HashSet<Uuid> = HashSet::new();
            let mut merged: Vec<Edge> = Vec::with_capacity(out.len() + inc.len());
            for e in out.iter().chain(inc.iter()) {
                if seen.insert(e.id) {
                    merged.push(e.clone());
                }
            }
            return Ok(Arc::new(merged));
        }

        let cached = match dir {
            Direction::Out => self.cache.get_adj_out(id),
            Direction::In  => self.cache.get_adj_in(id),
            Direction::Both => unreachable!(),
        };
        if let Some(edges) = cached {
            return Ok(edges);
        }

        let id_s = id.to_string();
        let sql = match dir {
            Direction::Out => format!(
                "SELECT {EDGE_COLS} FROM edges WHERE src = ? OR (directed = FALSE AND dst = ?)"
            ),
            Direction::In => format!(
                "SELECT {EDGE_COLS} FROM edges WHERE dst = ? OR (directed = FALSE AND src = ?)"
            ),
            Direction::Both => unreachable!(),
        };
        let rows = self.db.select_all_params(
            &sql,
            &[SqlParam::Text(id_s.clone()), SqlParam::Text(id_s)],
        )?;
        let mut edges = Vec::with_capacity(rows.len());
        for r in &rows {
            edges.push(row_to_edge(r)?);
        }
        let arc = Arc::new(edges);
        // Supernode guard — don't let one hub evict the whole cache.
        if arc.len() <= self.supernode_degree {
            match dir {
                Direction::Out => self.cache.put_adj_out(*id, arc.clone()),
                Direction::In  => self.cache.put_adj_in(*id, arc.clone()),
                Direction::Both => unreachable!(),
            }
        }
        Ok(arc)
    }

    /// Filtered edges of `id` in `dir` (`Both` = union of Out + In).
    fn directional_edges(&self, id: &Uuid, dir: Direction, f: &EdgeFilter) -> Result<Vec<Edge>> {
        Ok(apply_filter(&self.adjacency(id, dir)?, f))
    }

    /// Distinct ids of every node sharing an edge with `id` (used to
    /// invalidate neighbours' cached adjacency on node removal).
    fn neighbour_ids(&self, id: &Uuid) -> Result<Vec<Uuid>> {
        let id_s = id.to_string();
        let rows = self.db.select_all_params(
            "SELECT DISTINCT dst FROM edges WHERE src = ?
             UNION SELECT DISTINCT src FROM edges WHERE dst = ?",
            &[SqlParam::Text(id_s.clone()), SqlParam::Text(id_s)],
        )?;
        rows.iter().map(|r| dyn_uuid(&r[0])).collect()
    }

    /// Hydrate a set of node ids, cache-first, with one batched SQL
    /// `IN (…)` for the misses.  Output order is not guaranteed to
    /// match the input.
    fn hydrate_nodes(&self, ids: &[Uuid]) -> Result<Vec<Node>> {
        let mut found: Vec<Node> = Vec::with_capacity(ids.len());
        let mut missing: Vec<Uuid> = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();
        for id in ids {
            if !seen.insert(*id) {
                continue;
            }
            match self.cache.get_node(id) {
                Some(n) => found.push(n),
                None    => missing.push(*id),
            }
        }
        if !missing.is_empty() {
            let placeholders = vec!["?"; missing.len()].join(", ");
            let params: Vec<SqlParam> =
                missing.iter().map(|id| SqlParam::Text(id.to_string())).collect();
            let rows = self.db.select_all_params(
                &format!("SELECT {NODE_COLS} FROM nodes WHERE node_id IN ({placeholders})"),
                &params,
            )?;
            for r in &rows {
                let node = row_to_node(r)?;
                self.cache.put_node(node.clone());
                found.push(node);
            }
        }
        Ok(found)
    }

    /// BFS reachability — returns `(node_id, depth, hop_cost)` for each
    /// node first-reached within the bounds.
    fn bfs(&self, start: Uuid, opts: &TraversalOpts) -> Result<Vec<(Uuid, usize, f64)>> {
        let mut visited: HashSet<Uuid> = HashSet::from([start]);
        let mut frontier: Vec<Uuid> = vec![start];
        let mut out: Vec<(Uuid, usize, f64)> = Vec::new();
        for depth in 1..=opts.max_depth {
            if frontier.is_empty() || out.len() >= opts.max_nodes {
                break;
            }
            let mut next: Vec<Uuid> = Vec::new();
            for node in &frontier {
                for e in self.directional_edges(node, opts.direction, &opts.edge_filter)? {
                    let other = e.other(node);
                    if visited.insert(other) {
                        next.push(other);
                        out.push((other, depth, depth as f64));
                        if out.len() >= opts.max_nodes {
                            break;
                        }
                    }
                }
                if out.len() >= opts.max_nodes {
                    break;
                }
            }
            frontier = next;
        }
        Ok(out)
    }

    /// BFS variant that stops at `target`, recording the parent chain.
    fn bfs_to(
        &self,
        start: Uuid,
        target: Uuid,
        opts: &TraversalOpts,
        parent: &mut HashMap<Uuid, (Uuid, Edge)>,
    ) -> Result<bool> {
        let mut visited: HashSet<Uuid> = HashSet::from([start]);
        let mut frontier: Vec<Uuid> = vec![start];
        let mut explored = 0usize;
        for _ in 1..=opts.max_depth {
            if frontier.is_empty() || explored >= opts.max_nodes {
                break;
            }
            let mut next: Vec<Uuid> = Vec::new();
            for node in &frontier {
                for e in self.directional_edges(node, opts.direction, &opts.edge_filter)? {
                    let other = e.other(node);
                    if visited.insert(other) {
                        parent.insert(other, (*node, e));
                        explored += 1;
                        if other == target {
                            return Ok(true);
                        }
                        next.push(other);
                        if explored >= opts.max_nodes {
                            break;
                        }
                    }
                }
                if explored >= opts.max_nodes {
                    break;
                }
            }
            frontier = next;
        }
        Ok(false)
    }

    /// Dijkstra reachability over edge `weight` — returns
    /// `(node_id, depth, summed_weight)` for each settled node.
    fn dijkstra(&self, start: Uuid, opts: &TraversalOpts) -> Result<Vec<(Uuid, usize, f64)>> {
        let mut best: HashMap<Uuid, (f64, usize)> = HashMap::from([(start, (0.0, 0))]);
        let mut heap: BinaryHeap<DijkstraEntry> = BinaryHeap::new();
        heap.push(DijkstraEntry { cost: 0.0, depth: 0, node: start });
        let mut settled: HashSet<Uuid> = HashSet::new();
        let mut out: Vec<(Uuid, usize, f64)> = Vec::new();
        while let Some(DijkstraEntry { cost, depth, node }) = heap.pop() {
            if !settled.insert(node) {
                continue;
            }
            if node != start {
                out.push((node, depth, cost));
                if out.len() >= opts.max_nodes {
                    break;
                }
            }
            if depth >= opts.max_depth {
                continue;
            }
            for e in self.directional_edges(&node, opts.direction, &opts.edge_filter)? {
                let other = e.other(&node);
                if settled.contains(&other) {
                    continue;
                }
                let next_cost = cost + e.weight.max(0.0);
                let better = best.get(&other).map(|(c, _)| next_cost < *c).unwrap_or(true);
                if better {
                    best.insert(other, (next_cost, depth + 1));
                    heap.push(DijkstraEntry { cost: next_cost, depth: depth + 1, node: other });
                }
            }
        }
        Ok(out)
    }

    /// Dijkstra variant that stops at `target`, recording the parent chain.
    fn dijkstra_to(
        &self,
        start: Uuid,
        target: Uuid,
        opts: &TraversalOpts,
        parent: &mut HashMap<Uuid, (Uuid, Edge)>,
    ) -> Result<bool> {
        let mut best: HashMap<Uuid, f64> = HashMap::from([(start, 0.0)]);
        let mut heap: BinaryHeap<DijkstraEntry> = BinaryHeap::new();
        heap.push(DijkstraEntry { cost: 0.0, depth: 0, node: start });
        let mut settled: HashSet<Uuid> = HashSet::new();
        while let Some(DijkstraEntry { cost, depth, node }) = heap.pop() {
            if !settled.insert(node) {
                continue;
            }
            if node == target {
                return Ok(true);
            }
            if depth >= opts.max_depth || settled.len() > opts.max_nodes {
                continue;
            }
            for e in self.directional_edges(&node, opts.direction, &opts.edge_filter)? {
                let other = e.other(&node);
                if settled.contains(&other) {
                    continue;
                }
                let next_cost = cost + e.weight.max(0.0);
                let better = best.get(&other).map(|c| next_cost < *c).unwrap_or(true);
                if better {
                    best.insert(other, next_cost);
                    parent.insert(other, (node, e));
                    heap.push(DijkstraEntry { cost: next_cost, depth: depth + 1, node: other });
                }
            }
        }
        Ok(false)
    }
}

// ── lazy holder for ShardsManager ────────────────────────────────────────────

/// Lazily-opened [`GraphStore`] holder.  The store — DuckDB + Tantivy
/// — is opened only on first graph use, so a node that never touches
/// the graph pays no open cost.  Wrapped in `Arc` by
/// [`ShardsManager`](crate::shardsmanager::ShardsManager) so all its
/// clones share one store.
pub struct LazyGraph {
    root:      String,
    pool_size: u32,
    store:     std::sync::Mutex<Option<GraphStore>>,
}

impl LazyGraph {
    pub fn new(root: impl Into<String>, pool_size: u32) -> Self {
        Self {
            root:      root.into(),
            pool_size: pool_size.max(1),
            store:     std::sync::Mutex::new(None),
        }
    }

    /// Open-on-first-use accessor; returns a cheap clone of the shared
    /// [`GraphStore`].
    pub fn get(&self) -> Result<GraphStore> {
        let mut slot = self.store.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(GraphStore::open(&self.root, self.pool_size)?);
        }
        Ok(slot.as_ref().expect("just initialised").clone())
    }
}

// ── min-heap entry for Dijkstra ──────────────────────────────────────────────

struct DijkstraEntry {
    cost:  f64,
    depth: usize,
    node:  Uuid,
}

impl PartialEq for DijkstraEntry {
    fn eq(&self, other: &Self) -> bool { self.cost == other.cost }
}
impl Eq for DijkstraEntry {}
impl Ord for DijkstraEntry {
    // Reverse cost ordering so `BinaryHeap` (a max-heap) pops the
    // lowest-cost entry first.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.cost.partial_cmp(&self.cost).unwrap_or(std::cmp::Ordering::Equal)
    }
}
impl PartialOrd for DijkstraEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// ── free helpers ─────────────────────────────────────────────────────────────

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Apply an [`EdgeFilter`] to a cached adjacency list, honouring `limit`.
fn apply_filter(edges: &[Edge], f: &EdgeFilter) -> Vec<Edge> {
    let mut out: Vec<Edge> = edges.iter().filter(|e| f.keep(e)).cloned().collect();
    if let Some(limit) = f.limit {
        out.truncate(limit);
    }
    out
}

/// Build the `INSERT … ON CONFLICT DO UPDATE` statement + params for
/// one edge.  `created_at` is set on insert and preserved on conflict;
/// pass the same value for both timestamps for a normal local write,
/// or the replica's original timestamps for an anti-entropy apply.
#[allow(clippy::too_many_arguments)]
fn edge_upsert_stmt(
    edge_id: Uuid,
    src: Uuid,
    dst: Uuid,
    edge_type: &str,
    weight: f64,
    directed: bool,
    attrs: &JsonValue,
    valid_from: i64,
    valid_to: i64,
    created_at: i64,
    updated_at: i64,
) -> (String, Vec<SqlParam>) {
    let sql = format!(
        "INSERT INTO edges ({EDGE_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (src, dst, edge_type, valid_from) DO UPDATE SET
           weight = excluded.weight, directed = excluded.directed,
           attrs = excluded.attrs, valid_to = excluded.valid_to,
           updated_at = excluded.updated_at"
    );
    let params = vec![
        SqlParam::Text(edge_id.to_string()),
        SqlParam::Text(src.to_string()),
        SqlParam::Text(dst.to_string()),
        SqlParam::Text(edge_type.to_owned()),
        SqlParam::Real(weight),
        SqlParam::Bool(directed),
        SqlParam::Text(attrs.to_string()),
        SqlParam::Int(valid_from),
        SqlParam::Int(valid_to),
        SqlParam::Int(created_at),
        SqlParam::Int(updated_at),
    ];
    (sql, params)
}

/// FTS body for a node — `node_type`, `ref_id`, and the flattened
/// searchable values of its metadata.
fn fts_body(node_type: &str, ref_id: &str, attrs: &JsonValue) -> String {
    let mut parts = vec![node_type.to_owned(), ref_id.to_owned()];
    flatten_json(attrs, &mut parts);
    parts.join(" ")
}

/// Recursively collect string and number leaves of a JSON value.
fn flatten_json(v: &JsonValue, out: &mut Vec<String>) {
    match v {
        JsonValue::String(s) => out.push(s.clone()),
        JsonValue::Number(n) => out.push(n.to_string()),
        JsonValue::Bool(b)   => out.push(b.to_string()),
        JsonValue::Array(a)  => a.iter().for_each(|x| flatten_json(x, out)),
        JsonValue::Object(o) => o.iter().for_each(|(k, x)| {
            out.push(k.clone());
            flatten_json(x, out);
        }),
        JsonValue::Null => {}
    }
}

// ── row → struct extraction ──────────────────────────────────────────────────

fn dyn_string(v: &DynamicValue) -> Result<String> {
    v.cast_string().map_err(|e| err_msg(format!("expected text column: {e}")))
}
fn dyn_int(v: &DynamicValue) -> Result<i64> {
    v.cast_int().map_err(|e| err_msg(format!("expected integer column: {e}")))
}
fn dyn_float(v: &DynamicValue) -> Result<f64> {
    v.cast_float().map_err(|e| err_msg(format!("expected float column: {e}")))
}
fn dyn_bool(v: &DynamicValue) -> Result<bool> {
    v.cast_bool().map_err(|e| err_msg(format!("expected boolean column: {e}")))
}
fn dyn_uuid(v: &DynamicValue) -> Result<Uuid> {
    let s = dyn_string(v)?;
    Uuid::parse_str(&s).map_err(|e| err_msg(format!("invalid UUID '{s}': {e}")))
}
fn dyn_json(v: &DynamicValue) -> Result<JsonValue> {
    let s = dyn_string(v)?;
    if s.is_empty() {
        return Ok(JsonValue::Object(Default::default()));
    }
    Ok(serde_json::from_str(&s).unwrap_or(JsonValue::Object(Default::default())))
}

/// `node_id, node_type, ref_id, attrs, created_at, updated_at`
fn row_to_node(r: &[DynamicValue]) -> Result<Node> {
    if r.len() < 6 {
        return Err(err_msg("node row: expected 6 columns"));
    }
    Ok(Node {
        id:         dyn_uuid(&r[0])?,
        node_type:  dyn_string(&r[1])?,
        ref_id:     dyn_string(&r[2])?,
        attrs:      dyn_json(&r[3])?,
        created_at: dyn_int(&r[4])?,
        updated_at: dyn_int(&r[5])?,
    })
}

/// `edge_id, src, dst, edge_type, weight, directed, attrs, valid_from, valid_to, created_at, updated_at`
fn row_to_edge(r: &[DynamicValue]) -> Result<Edge> {
    if r.len() < 11 {
        return Err(err_msg("edge row: expected 11 columns"));
    }
    Ok(Edge {
        id:         dyn_uuid(&r[0])?,
        src:        dyn_uuid(&r[1])?,
        dst:        dyn_uuid(&r[2])?,
        edge_type:  dyn_string(&r[3])?,
        weight:     dyn_float(&r[4])?,
        directed:   dyn_bool(&r[5])?,
        attrs:      dyn_json(&r[6])?,
        valid_from: dyn_int(&r[7])?,
        valid_to:   dyn_int(&r[8])?,
        created_at: dyn_int(&r[9])?,
        updated_at: dyn_int(&r[10])?,
    })
}
