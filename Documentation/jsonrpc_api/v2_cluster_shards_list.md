# v2/cluster.shards.list

Peer-cheap catalog dump.  Returns `(shard_id, start_ts, end_ts)`
tuples for every shard registered in this node's
[`ShardInfoEngine`](../STORAGEENGINE.md) without opening any shard.

Designed for Phase 3 retention quorum probes: an evicting peer can
call this against every Alive sibling at the start of a sweep to
build an "(interval → number-of-peers-holding-it)" map.

Unauthenticated v2/* read — same trust boundary as
`v2/cluster.peers` / `v2/timeline`.

## Parameters

None.  Any payload is ignored.

## Response

```json
{
  "n_shards": 4,
  "shards": [
    { "shard_id": "019e2200-…", "start_ts": 1699999200, "end_ts": 1700002800 },
    { "shard_id": "019e2201-…", "start_ts": 1700002800, "end_ts": 1700006400 },
    …
  ]
}
```

| Field           | Type   | Notes |
|-----------------|--------|-------|
| `n_shards`      | int    | Length of `shards[]`. |
| `shards[].shard_id` | string (UUID) | The catalog row's UUIDv7. |
| `shards[].start_ts` | int   | Inclusive lower bound (Unix seconds). |
| `shards[].end_ts`   | int   | Exclusive upper bound (Unix seconds). |

Cost: one indexed SELECT against `shards_info.db`.  Does **not** open
any underlying shard's DuckDB / Tantivy / VecStore / tplstorage —
even on nodes with thousands of shards the call returns in
sub-millisecond time.

## Example

```bash
# Direct: peek at one node's shard catalog.
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/cluster.shards.list","params":{},"id":1}' \
  | jq '.result.n_shards, .result.shards[:3]'
```

## How retention uses it

When `retention.quorum_check_enabled = true`, the bdsnode sweeper
fans this RPC out across every Alive peer at the start of each
sweep ([`fetch_peer_shards` in
`src/bin/bdsnode/server/retention.rs`](../../src/bin/bdsnode/server/retention.rs)).
The returned shards are aggregated into a `HashMap<(start_ts, end_ts), peer_count>`,
then consulted per candidate during the sweep.

A shard is allowed to evict iff at least
`retention.quorum_min_peers` OTHER live peers report a shard for
the same exact `(start_ts, end_ts)` interval.

Caveats:

- **Exact-interval matching only.**  When peers run different
  `shard_duration` settings their catalog rows won't share keys
  and the quorum check fails closed.  Operators should keep
  `shard_duration` uniform across the cluster.
- **Per-node catalog.**  This RPC does not coordinate with
  anti-entropy or replication metadata — it's just a read of the
  local catalog.  An older replica on a peer that's lagging
  on ingest will not surface here even if it eventually receives
  the data.

## Error codes

| Code     | Trigger                                               |
|----------|-------------------------------------------------------|
| `-32000` | Task panicked on the bdsnode tokio pool.              |
| `-32001` | DB unavailable (initialisation incomplete).           |
| `-32002` | Catalog read failed.                                  |

## See also

- [`../RETENTION.md`](../RETENTION.md) § Phase 3 — Cluster-aware
  quorum — the operator-facing design + opt-in instructions.
- [`v2_shards.md`](v2_shards.md) — heavier-weight cousin that
  opens each shard to count primaries/secondaries.  Wrong tool
  for quorum probes.
- [`v3_cluster_retention.md`](v3_cluster_retention.md) —
  cluster-wide retention introspection (Phase 2).
