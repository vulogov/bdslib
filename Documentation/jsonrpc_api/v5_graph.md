# v5/graph.* — Relationship graph API

The `v5/*` tree exposes the **relationship graph** of the global
`ShardsManager` — a directed/undirected, weighted, time-bounded
property graph over the entities bdsnode manages (telemetry items,
groups, documents, signals, …).  It is backed by the `GraphStore`
layer (`src/graphstorage.rs`): a self-contained directory under
`<dbpath>/graph/` holding a DuckDB store for nodes + edges and a
Tantivy index for node-metadata full-text search.

The graph is a **fully-replicated cluster store** — see
[§ Cluster replication](#cluster-replication) below and
[`CLUSTER.md § 11`](../CLUSTER.md).

---

## Entity model

| Concept | Notes |
|---|---|
| **Node** | A typed entity identified by a `(node_type, ref_id)` natural key. `node_type` is free-form (`telemetry`, `document`, `signal`, `group`, …). Carries a free-form JSON `attrs` blob, full-text-indexed for `v5/graph.search`. |
| **Edge** | A typed, weighted relationship `src → dst`. Directed by default; undirected edges are stored once in canonical order. Carries `attrs` and a validity window `[valid_from, valid_to)` — an open edge uses the sentinel `valid_to = 9223372036854775807`. The same `(src, dst, edge_type)` may recur as distinct time *episodes*. |
| **Group** | A first-class `group` node linked to its members — not a special construct. `v5/graph.node.group.add` is a convenience that creates the group node + membership edges in one call. |

**Deterministic ids.** Node and edge ids are UUIDv5 of their natural
key.  The *same logical entity gets the same id on every node* — this
is what lets the cluster's last-writer-wins replication converge a
graph without duplicating it.  A returned `id` is stable and
reproducible.

**Temporal edges.** Edge queries take an optional `TimeScope`:
`At(t)` (valid at instant `t`) or `Overlap(a, b)` (active during the
half-open window `[a, b)`).  Absent → the cumulative graph.

---

## Method surface

| Class | Methods |
|---|---|
| **Reads** (local) | `node.get`, `outgoing`, `incoming`, `neighbors`, `degree`, `traverse`, `reachable`, `shortest_path`, `search`, `search.typed`, `stats`, `verify`, `fingerprint` |
| **Writes** (replicated) | `node.add`, `node.remove`, `node.group.add`, `link`, `link.batch`, `unlink`, `expire`, `edge.set_weight`, `edge_type.register` |
| **Maintenance** (per-node) | `repair`, `rebuild_fts`, `sync` |

Reads are served from the local replica — full replication means a
local read covers the whole cluster, so there is no fan-out and no
`cluster_meta` block.  Writes commit locally then fan out (see below);
every write response carries an `outcome` block.  Maintenance is
per-node and local, like `v2/retention.sweep`.

---

## Common parameter shapes

A **node reference** is `{ "node_type": "...", "ref_id": "..." }`.

An **edge filter** (optional on the neighbour/traversal reads):

| Field | Type | Description |
|---|---|---|
| `edge_types` | string[] | Restrict to these edge types. |
| `min_weight` | number | Drop edges below this weight. |
| `limit` | integer | Cap the result count. |
| `at` | integer | `TimeScope::At` — edges valid at this Unix-second instant. |
| `overlap` | `[i64, i64]` | `TimeScope::Overlap` — edges active during `[a, b)`. `at` wins if both are set. |

A **traversal** carries: `direction` (`"out"` \| `"in"` \| `"both"`,
default `"out"`), `max_depth` and `max_nodes` (both **required** — every
traversal is bounded), optional `strategy` (`"bfs"` default \|
`"dijkstra"` — weighted shortest cost), and an optional `edge_filter`.

---

## Reads

### `v5/graph.node.get`

`{ node_type, ref_id }` → `{ found, node }`.  `node` is
`{ id, node_type, ref_id, attrs, created_at, updated_at }`.

### `v5/graph.outgoing` · `v5/graph.incoming`

`{ node_type, ref_id, filter? }` → `{ count, edges }`.  `edges[]` is
the full edge rows leaving (`outgoing`) or arriving at (`incoming`) the
node, with undirected edges visible from both ends.

### `v5/graph.neighbors`

`{ node_type, ref_id, direction?, filter? }` → `{ count, nodes }` —
the neighbour *nodes* in the given direction.

### `v5/graph.degree`

`{ node_type, ref_id, direction? }` → `{ degree }`.

### `v5/graph.traverse`

`{ start: {node_type, ref_id}, direction?, max_depth, max_nodes, strategy?, edge_filter? }`
→ `{ count, hits }`.  Each hit is `{ node, depth, path_cost }`.  The
canonical **blast-radius** query: traverse `out` along
dependency/affects edges from a seed.

### `v5/graph.reachable`

Same params as `traverse` → `{ count, nodes }`, where `nodes[]` is
`{ node_type, ref_id }` — just the reachable set.

### `v5/graph.shortest_path`

`{ from, to, direction?, max_depth, max_nodes, strategy?, edge_filter? }`
→ `{ found, nodes, edges, total_weight }`.  `strategy: "dijkstra"`
minimises summed edge weight; `"bfs"` minimises hop count.

### `v5/graph.search` · `v5/graph.search.typed`

Full-text search over node metadata.  `search`:
`{ query, limit? }`; `search.typed`: `{ query, types: [...], limit? }`
→ `{ count, results }` where each result is `{ node, score }`
(BM25 relevance).

### `v5/graph.stats`

`{}` → `{ node_count, edge_count, edge_type_count, fts_doc_count, cache }`
— the `cache` block reports resolve/node/adjacency cache hit+miss
counters.

### `v5/graph.verify`

`{}` → a read-only integrity report:
`{ healthy, node_count, edge_count, dangling_edges, invalid_temporal_edges, fts_doc_count, fts_drift }`.
`healthy` is `true` iff every defect counter is zero.

### `v5/graph.fingerprint`

`{}` → `{ node_count, edge_count, nodes_hash, edges_hash }` — a cheap,
order-independent whole-store digest.  Two replicas with identical
content produce identical fingerprints; this is what the anti-entropy
sweep compares before doing a full diff.

---

## Writes

Every write follows the fully-replicated pattern: **commit locally,
then fan out to every Alive peer's `v2/graph.apply.batch` receiver**
(hint-on-failure).  The response is `{ <result>, "outcome": {…} }` where
`outcome` is `{ peers_attempted, peers_succeeded, hints_queued }`
(all `0` in standalone mode).

| Method | Params | Result |
|---|---|---|
| `v5/graph.node.add` | `{ node_type, ref_id, attrs? }` | `{ id, outcome }` — upsert; `attrs` updated if the node exists. |
| `v5/graph.node.remove` | `{ node_type, ref_id }` | `{ removed, outcome }` — cascades the node's edges; writes a `graph_nodes` tombstone. |
| `v5/graph.node.group.add` | `{ group: {node_type, ref_id}, group_attrs?, members: [{node_type, ref_id}], member_edge }` | `{ id, outcome }` — creates the group node + `member_edge` edges group→member. |
| `v5/graph.link` | `{ from, to, edge_type, weight?, directed?, attrs?, valid_from?, valid_to? }` | `{ id, outcome }` — auto-vivifies missing endpoint nodes; upserts the edge episode. |
| `v5/graph.link.batch` | `{ links: [ {from, to, edge_type, …} ] }` | `{ count, outcome }` — one transaction, one fan-out. |
| `v5/graph.unlink` | `{ from, to, edge_type }` | `{ removed: [edge_id…], outcome }` — deletes every episode of the edge; writes `graph_edges` tombstones. |
| `v5/graph.expire` | `{ from, to, edge_type, at }` | `{ updated: [edge_id…], outcome }` — closes the validity window (`valid_to = at`); history is preserved, not deleted. |
| `v5/graph.edge.set_weight` | `{ edge_id, weight }` | `{ updated, outcome }`. |
| `v5/graph.edge_type.register` | `{ name, default_weight?, default_directed?, attrs? }` | `{ ok, outcome }` — registers defaults for a "configured" edge type. |

---

## Maintenance (per-node, local)

| Method | Params | Result |
|---|---|---|
| `v5/graph.repair` | `{ prune_dangling?, prune_invalid?, fix_fts_drift?, dry_run? }` (all default `true`, `dry_run` default `false`) | `{ dry_run, dangling_pruned, invalid_pruned, fts_rebuilt, fts_docs_after, before }` — detects + repairs store-internal inconsistency. |
| `v5/graph.rebuild_fts` | `{}` | `{ reindexed }` — wipe + rebuild the Tantivy index from the DuckDB `nodes` table (garbage-collects orphan FTS docs). |
| `v5/graph.sync` | `{}` | `{ ok }` — DuckDB `CHECKPOINT` + Tantivy commit. |

These act only on the calling node — like a retention sweep — they are
not replicated.

---

## Examples

```bash
# add a node
curl -s -X POST http://127.0.0.1:9000 -H 'Content-Type: application/json' -d '{
  "jsonrpc":"2.0","id":1,"method":"v5/graph.node.add",
  "params":{"node_type":"telemetry","ref_id":"checkout-svc","attrs":{"service":"checkout"}}
}' | jq

# link two services with a weighted dependency edge
curl -s -X POST http://127.0.0.1:9000 -H 'Content-Type: application/json' -d '{
  "jsonrpc":"2.0","id":1,"method":"v5/graph.link",
  "params":{"from":{"node_type":"telemetry","ref_id":"checkout-svc"},
            "to":{"node_type":"telemetry","ref_id":"payments-svc"},
            "edge_type":"depends_on","weight":2.0}
}' | jq

# blast radius — everything reachable downstream of checkout-svc
curl -s -X POST http://127.0.0.1:9000 -H 'Content-Type: application/json' -d '{
  "jsonrpc":"2.0","id":1,"method":"v5/graph.traverse",
  "params":{"start":{"node_type":"telemetry","ref_id":"checkout-svc"},
            "direction":"out","max_depth":10,"max_nodes":1000}
}' | jq

# blast radius "as of" an incident window
curl -s -X POST http://127.0.0.1:9000 -H 'Content-Type: application/json' -d '{
  "jsonrpc":"2.0","id":1,"method":"v5/graph.traverse",
  "params":{"start":{"node_type":"telemetry","ref_id":"checkout-svc"},
            "direction":"out","max_depth":10,"max_nodes":1000,
            "edge_filter":{"overlap":[1778790000,1778793600]}}
}' | jq
```

---

## Cluster replication

The graph is one of the `cluster.full_replication_stores` (see
[`CLUSTER.md § 11`](../CLUSTER.md)).  Replication works on two paths:

### Real-time fan-out

A `v5/graph.*` write commits locally, reads back every affected node +
edge, and fans **one** `v2/graph.apply.batch` call out to every Alive
peer.  The receiver applies it **locally only** (no re-fan-out) through
last-writer-wins upsert primitives that keep the Tantivy index and the
in-memory caches coherent.  A peer that fails the call gets the batch
enqueued as a hint for replay.

Because entity ids are deterministic, the *same logical write applied
on two nodes converges to one row* — there is no id-divergence to
reconcile.

### Anti-entropy

The cluster background loop (`cluster.antientropy_interval`, default
5 min) runs a graph-specific sync against a random Alive peer:

1. **Fingerprint pre-check** — compare `v2/graph.fingerprint` both
   ways; identical → already converged, skip the diff entirely.
2. **Enumerate** — pull the peer's `v2/graph.list_ids` (node + edge
   summaries keyed by natural key + LWW `updated_at`, plus tombstones)
   and the local equivalent.
3. **Apply tombstones** the peer has and we don't (delete + record).
4. **Pull** every node, then every edge, that is missing locally or
   older than the peer's copy — applied through the same LWW
   primitives.  Nodes before edges keeps referential integrity.

This recovers a node that was down during a fan-out (and missed the
hint-replay window).  Node deletes tombstone under `graph_nodes`, edge
deletes under `graph_edges`, so a delete is never resurrected by a
later anti-entropy round.

The inter-node receiver + enumeration surface is documented in
[`v2_graph.md`](v2_graph.md).

---

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic. |
| `-32001` | Database / `ShardsManager` unavailable. |
| `-32011` | Graph operation failed (DuckDB / Tantivy error). |
| `-32602` | Invalid params (missing required field, bad UUID, bad `direction`/`strategy`). |

## See also

- [`v2_graph.md`](v2_graph.md) — the `v2/graph.*` inter-node receiver + anti-entropy surface.
- [`CLUSTER.md`](../CLUSTER.md) — cluster mode, fully-replicated stores, anti-entropy.
- `src/graphstorage.rs` — the `GraphStore` layer: schema, caching, traversal, and self-healing primitives (`probe` / `verify` / `repair` / `fingerprint` / `apply_*_lww`).
