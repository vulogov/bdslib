//! In-memory cache for [`GraphStore`](crate::graphstorage::GraphStore).
//!
//! Three layers, all behind one `Mutex`, all capacity-bounded with
//! random eviction (the same cheap strategy [`JsonCache`] uses — no
//! LRU bookkeeping, no background thread; graph data is invalidated
//! by writes, not by time):
//!
//! - **resolve** — `(node_type, ref_id)` → `node_id`.  The hot path
//!   for `link()`, which would otherwise do a natural-key lookup per
//!   endpoint.
//! - **nodes** — `node_id` → [`Node`].  Serves `get_node` and the
//!   hydration step of FTS `search_nodes`.
//! - **adjacency** — `node_id` → `Arc<Vec<Edge>>`, separate maps for
//!   outgoing and incoming.  The *full* adjacency of a node is cached;
//!   `EdgeFilter` (type / weight / time) is applied in Rust on a hit,
//!   so filter variety never explodes the key space.  The caller
//!   (GraphStore) declines to cache supernodes whose degree exceeds a
//!   threshold.
//!
//! Invalidation is precise: an edge write touching `(src, dst)` evicts
//! exactly those two endpoints' adjacency entries; a node mutation
//! evicts that node's entries.
//!
//! [`JsonCache`]: crate::common::cache_json::JsonCache
//! [`Node`]: crate::graphstorage::Node

use crate::graphstorage::{Edge, Node};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// `(node_type, ref_id)` natural key.
type NodeKey = (String, String);

struct Inner {
    resolve: HashMap<NodeKey, Uuid>,
    nodes:   HashMap<Uuid, Node>,
    adj_out: HashMap<Uuid, Arc<Vec<Edge>>>,
    adj_in:  HashMap<Uuid, Arc<Vec<Edge>>>,
    cap_resolve: usize,
    cap_nodes:   usize,
    cap_adj:     usize,
}

impl Inner {
    /// Evict random entries until `map` has room for one more (`len < cap`).
    /// No-op when `cap == 0` is handled by the callers (they skip the put).
    fn make_room<K: Clone + Eq + Hash, V>(map: &mut HashMap<K, V>, cap: usize) {
        while map.len() >= cap && !map.is_empty() {
            let idx = fastrand::usize(0..map.len());
            match map.keys().nth(idx).cloned() {
                Some(k) => { map.remove(&k); }
                None => break,
            }
        }
    }
}

#[derive(Default)]
struct Stats {
    resolve_hit:  AtomicU64,
    resolve_miss: AtomicU64,
    node_hit:     AtomicU64,
    node_miss:    AtomicU64,
    adj_hit:      AtomicU64,
    adj_miss:     AtomicU64,
}

/// Snapshot of cache effectiveness, surfaced through `GraphStore::stats`.
#[derive(Debug, Clone, Default)]
pub struct GraphCacheStats {
    pub resolve_hits:   u64,
    pub resolve_misses: u64,
    pub node_hits:      u64,
    pub node_misses:    u64,
    pub adj_hits:       u64,
    pub adj_misses:     u64,
    pub resolve_len:    usize,
    pub nodes_len:      usize,
    pub adj_out_len:    usize,
    pub adj_in_len:     usize,
}

/// Cloneable handle to the shared graph cache; every clone shares the
/// same underlying maps and counters.
#[derive(Clone)]
pub struct GraphCache {
    inner: Arc<Mutex<Inner>>,
    stats: Arc<Stats>,
}

impl GraphCache {
    /// `cap_resolve` slots for the natural-key→id map, `cap_nodes` for
    /// node metadata, `cap_adj` for *each* adjacency direction.  A
    /// capacity of `0` disables that layer.
    pub fn new(cap_resolve: usize, cap_nodes: usize, cap_adj: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                resolve: HashMap::new(),
                nodes:   HashMap::new(),
                adj_out: HashMap::new(),
                adj_in:  HashMap::new(),
                cap_resolve,
                cap_nodes,
                cap_adj,
            })),
            stats: Arc::new(Stats::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means a panic mid-mutation; the cache is just
        // an optimisation, so recover the guard and carry on.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ── resolve: (node_type, ref_id) → node_id ────────────────────────────────

    pub fn get_resolve(&self, node_type: &str, ref_id: &str) -> Option<Uuid> {
        let key = (node_type.to_owned(), ref_id.to_owned());
        let hit = self.lock().resolve.get(&key).copied();
        match hit {
            Some(_) => self.stats.resolve_hit.fetch_add(1, Ordering::Relaxed),
            None    => self.stats.resolve_miss.fetch_add(1, Ordering::Relaxed),
        };
        hit
    }

    pub fn put_resolve(&self, node_type: &str, ref_id: &str, id: Uuid) {
        let mut g = self.lock();
        let cap = g.cap_resolve;
        if cap == 0 {
            return;
        }
        Inner::make_room(&mut g.resolve, cap);
        g.resolve.insert((node_type.to_owned(), ref_id.to_owned()), id);
    }

    // ── nodes: node_id → Node ─────────────────────────────────────────────────

    pub fn get_node(&self, id: &Uuid) -> Option<Node> {
        let hit = self.lock().nodes.get(id).cloned();
        match hit {
            Some(_) => self.stats.node_hit.fetch_add(1, Ordering::Relaxed),
            None    => self.stats.node_miss.fetch_add(1, Ordering::Relaxed),
        };
        hit
    }

    pub fn put_node(&self, node: Node) {
        let mut g = self.lock();
        let cap = g.cap_nodes;
        if cap == 0 {
            return;
        }
        Inner::make_room(&mut g.nodes, cap);
        g.nodes.insert(node.id, node);
    }

    // ── adjacency: node_id → Arc<Vec<Edge>> (full, unfiltered) ────────────────

    pub fn get_adj_out(&self, id: &Uuid) -> Option<Arc<Vec<Edge>>> {
        let hit = self.lock().adj_out.get(id).cloned();
        match hit {
            Some(_) => self.stats.adj_hit.fetch_add(1, Ordering::Relaxed),
            None    => self.stats.adj_miss.fetch_add(1, Ordering::Relaxed),
        };
        hit
    }

    pub fn get_adj_in(&self, id: &Uuid) -> Option<Arc<Vec<Edge>>> {
        let hit = self.lock().adj_in.get(id).cloned();
        match hit {
            Some(_) => self.stats.adj_hit.fetch_add(1, Ordering::Relaxed),
            None    => self.stats.adj_miss.fetch_add(1, Ordering::Relaxed),
        };
        hit
    }

    pub fn put_adj_out(&self, id: Uuid, edges: Arc<Vec<Edge>>) {
        let mut g = self.lock();
        let cap = g.cap_adj;
        if cap == 0 {
            return;
        }
        Inner::make_room(&mut g.adj_out, cap);
        g.adj_out.insert(id, edges);
    }

    pub fn put_adj_in(&self, id: Uuid, edges: Arc<Vec<Edge>>) {
        let mut g = self.lock();
        let cap = g.cap_adj;
        if cap == 0 {
            return;
        }
        Inner::make_room(&mut g.adj_in, cap);
        g.adj_in.insert(id, edges);
    }

    // ── invalidation ──────────────────────────────────────────────────────────

    /// Drop every cached trace of one node — its metadata, both
    /// adjacency directions, and (when the natural key is known) its
    /// resolve entry.  Used by `remove_node` and metadata updates.
    pub fn invalidate_node(&self, id: &Uuid, node_ref: Option<(&str, &str)>) {
        let mut g = self.lock();
        g.nodes.remove(id);
        g.adj_out.remove(id);
        g.adj_in.remove(id);
        if let Some((t, r)) = node_ref {
            g.resolve.remove(&(t.to_owned(), r.to_owned()));
        }
    }

    /// Drop the adjacency entries of both endpoints of an edge write.
    /// Both directions are dropped for each endpoint: the cache does
    /// not track edge direction, and an undirected edge affects both
    /// `adj_out` and `adj_in` of each side.
    pub fn invalidate_edge_endpoints(&self, src: &Uuid, dst: &Uuid) {
        let mut g = self.lock();
        g.adj_out.remove(src);
        g.adj_in.remove(src);
        g.adj_out.remove(dst);
        g.adj_in.remove(dst);
    }

    /// Drop the adjacency entries for every node in `ids` (both
    /// directions).  Used after a node removal to evict the cached
    /// neighbours that referenced it.
    pub fn invalidate_adjacency_of(&self, ids: &[Uuid]) {
        let mut g = self.lock();
        for id in ids {
            g.adj_out.remove(id);
            g.adj_in.remove(id);
        }
    }

    /// Empty every layer (used by `reindex` / bulk maintenance).
    pub fn clear(&self) {
        let mut g = self.lock();
        g.resolve.clear();
        g.nodes.clear();
        g.adj_out.clear();
        g.adj_in.clear();
    }

    pub fn stats(&self) -> GraphCacheStats {
        let g = self.lock();
        GraphCacheStats {
            resolve_hits:   self.stats.resolve_hit.load(Ordering::Relaxed),
            resolve_misses: self.stats.resolve_miss.load(Ordering::Relaxed),
            node_hits:      self.stats.node_hit.load(Ordering::Relaxed),
            node_misses:    self.stats.node_miss.load(Ordering::Relaxed),
            adj_hits:       self.stats.adj_hit.load(Ordering::Relaxed),
            adj_misses:     self.stats.adj_miss.load(Ordering::Relaxed),
            resolve_len:    g.resolve.len(),
            nodes_len:      g.nodes.len(),
            adj_out_len:    g.adj_out.len(),
            adj_in_len:     g.adj_in.len(),
        }
    }
}
