# v3/add.batch

Replicated batch write. Same recipe as [`v3/add`](v3_add.md), but the
local write goes through `ShardsManager::add_batch` (one DuckDB write
transaction, one Tantivy commit, one HNSW upsert pass) and the fan-out is
also a single round-trip per peer (`v2/add.batch` with `sync: true`).

For high-volume ingest this is dramatically cheaper than calling `v3/add`
N times — roughly N× lower per-record latency for batches of 100+ docs.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `docs` | array of objects | yes | — | Telemetry documents. Each must contain `timestamp`, `key`, `data`. May contain `id` per-doc — preserved as the replication identity. |
| `replication_factor` | integer | no | `cluster.replication_factor` | Override the cluster's configured replication factor. |

## Response

```json
{
  "ids":                 ["019e1040-…", "019e1040-…", "019e1040-…"],
  "n":                   3,
  "replication_factor":  3,
  "replicas_dispatched": 2,
  "alive_peers":         2,
  "under_replicated":    false,
  "mode":                "full",
  "cluster_meta":        { "enabled": true }
}
```

| Field | Description |
|---|---|
| `ids` | UUIDv7s of the stored records, in input order. |
| `n` | `ids.length`. |
| `replication_factor` / `replicas_dispatched` / `alive_peers` / `under_replicated` / `mode` | See [v3/add](v3_add.md). |

## Receiver-side semantics

The fan-out uses the new `sync: true` mode of [`v2/add.batch`](v2_add_batch.md),
which calls `ShardsManager::add_batch` directly on a blocking thread
instead of pushing to the async ingest queue. This is required so the
coordinator's hinted-handoff path can distinguish success from failure on
the receiver — the legacy queued path always returns `{queued: N}` even
when the underlying write later fails.

The async-queued path remains the default for callers that want
fire-and-forget behaviour with no replication guarantees.

## Example

```bash
bdscmd cluster add-batch -f /path/to/batch.ndjson
```

Where `batch.ndjson` is one JSON document per line:

```jsonl
{"timestamp":1778000000,"key":"app.error","data":{"msg":"first"}}
{"timestamp":1778000001,"key":"app.warn","data":{"msg":"second"}}
```

## Idempotency

Same recipe as [`v3/add`](v3_add.md): each document is keyed by its `id`,
and the receiver's `(key, data_text)` dedup absorbs duplicates without
creating extra rows. Hint replay therefore converges regardless of how
many times the same batch is replayed.

## Hinted handoff

The whole batch is enqueued as a single hint per failed peer (one row in
`hints.duckdb` per failed peer per batch). The hint's `params` field holds
the entire `v2/add.batch` payload, so retry sends one round-trip even if
the original batch had 1000 records.

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32004` | Local batch write failed |
| `-32602` | Invalid params (missing `docs`, malformed per-doc `id`) |

## Notes

- **Empty batch.** Returns immediately with `{"ids":[], "replicas_dispatched":0}`.
- **Same effective rf as v3/add.** Clamped to `min(rf, alive_peers + 1)`.
- **Per-record vs per-batch hints.** A failed batch fan-out queues exactly
  one hint per peer, regardless of batch size. This means partial batch
  failures (e.g. peer accepted 500 of 1000 docs and then failed) are not
  granular — the entire batch is replayed on recovery, and the receiver's
  `(key, data_text)` dedup absorbs the duplicates.
