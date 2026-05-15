# v2/graph.* — Graph replication receiver + anti-entropy surface

These are **inter-node** methods, not the client API.  The client API
is [`v5/graph.*`](v5_graph.md); `v2/graph.*` is what the `v5/graph.*`
coordinator fans out to and what the anti-entropy sweep
(`server/cluster.rs::sync_graph`) pulls from.  Operators normally
never call these directly.

The graph is a fully-replicated cluster store — see
[`CLUSTER.md § 11`](../CLUSTER.md).

---

## `v2/graph.apply.batch` — replication receiver

Applies a replicated write batch **locally only** (no re-fan-out, so
no replication loop) through last-writer-wins upsert primitives that
keep the Tantivy index and the in-memory caches coherent.  Idempotent:
re-arrivals (hint replays, anti-entropy) converge.

### Parameters

| Field | Type | Description |
|---|---|---|
| `nodes` | object[] | Full node rows to upsert (`{id, node_type, ref_id, attrs, created_at, updated_at}`). LWW on `updated_at`. |
| `edges` | object[] | Full edge rows to upsert (`{id, src, dst, edge_type, weight, directed, attrs, valid_from, valid_to, created_at, updated_at}`). LWW on `updated_at`. |
| `removed_nodes` | object[] | `{node_type, ref_id}` keys to delete (cascades the node's edges; tombstoned under `graph_nodes`). |
| `removed_edges` | object[] | `{from: {node_type, ref_id}, to: {…}, edge_type}` keys to delete (tombstoned under `graph_edges`). |
| `edge_types` | object[] | `{name, default_weight?, default_directed?, attrs?}` registry entries to upsert. |

All fields are optional; a batch carries only what the originating
write produced.  Nodes are applied before edges so endpoint
references resolve.

### Response

```json
{ "nodes_applied": 2, "edges_applied": 1, "nodes_removed": 0, "edges_removed": 0 }
```

Counts reflect rows that actually changed (an LWW no-op — local copy
already as fresh — is not counted).

---

## `v2/graph.list_ids` — anti-entropy enumeration

Cheap enumeration of every node and edge by **natural key** + LWW
`updated_at`, plus the tombstone lists.  The input source for the
graph anti-entropy diff.

### Response

```json
{
  "n_nodes": 4,
  "n_edges": 2,
  "nodes": [ { "id": "...", "node_type": "telemetry", "ref_id": "svc-a", "updated_at": 1778790000 } ],
  "edges": [ { "id": "...", "src": "...", "dst": "...", "edge_type": "depends_on", "valid_from": 0, "updated_at": 1778790000 } ],
  "node_tombstones": [ { "id": "...", "deleted_at": 1778790100 } ],
  "edge_tombstones": [ { "id": "...", "deleted_at": 1778790100 } ]
}
```

The two id-spaces (nodes, edges) are returned together — the graph is
the one replicated store with more than one id-space, which is why it
gets its own `sync_graph` path rather than the generic `sync_store`.
Tombstone arrays are empty when cluster mode is off.

---

## `v2/graph.node.get` · `v2/graph.edge.get` — full-entity getters

`{ id }` (a UUID string) → `{ found, node }` / `{ found, edge }` with
the full entity row.  Used by the anti-entropy pull path to fetch an
entity the diff flagged as missing or stale.

---

## `v2/graph.fingerprint` — cheap divergence probe

`{}` → `{ node_count, edge_count, nodes_hash, edges_hash }` — an
order-independent whole-store digest.  The anti-entropy sweep compares
this against its own fingerprint first; identical hashes mean the two
replicas have already converged, so the full id-set diff is skipped.

Identical to [`v5/graph.fingerprint`](v5_graph.md) — exposed under
`v2/*` as well so the inter-node anti-entropy client can reach it.

---

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic. |
| `-32001` | Database / `ShardsManager` unavailable. |
| `-32011` | Graph operation failed. |
| `-32602` | Invalid params (bad UUID, malformed batch). |

## See also

- [`v5_graph.md`](v5_graph.md) — the client-facing `v5/graph.*` API.
- [`CLUSTER.md`](../CLUSTER.md) — fully-replicated stores + anti-entropy.
