# v3/doc.delete

Fully-replicated document delete.  Deletes locally, writes a tombstone
(so anti-entropy can't resurrect the doc from a peer that hasn't yet
applied the delete), then fans `v2/doc.delete` out to every Alive peer
with a shared `deleted_at` timestamp.

The receiver-side `v2/doc.delete` (extended in Phase 4) also writes a
tombstone when cluster mode is on, so every replica's tombstone log
stays in sync.

For the full replication lifecycle and hinted-handoff semantics see
[`v3/doc.add`](v3_doc_add.md). Architectural overview:
[`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | string | yes | UUIDv7 of the doc to delete. |
| `session` | string | no | UUIDv7 transaction id (echoed only). |

## Response

```json
{
  "id":         "019e1056-d0c9-75c3-927b-dbed1b62bdb2",
  "deleted":    true,
  "deleted_at": 1778390602,
  "outcome": {
    "peers_attempted": 2,
    "peers_succeeded": 2,
    "hints_queued":    0
  },
  "cluster_meta": { "enabled": true }
}
```

| Field | Description |
|---|---|
| `deleted_at` | Unix seconds the coordinator chose for this deletion. The same value is sent to every peer so tombstones agree across replicas. |
| `outcome.*` | See [v3/doc.add](v3_doc_add.md#response). |

## Tombstone lifetime

Tombstones live in `<dbpath>/network/tombstones.duckdb` for
`cluster.hint_max_age * 2` (default 48h), then get GC'd by the
[anti-entropy task](../CLUSTER.md#7-rpc-surface).  Long enough to give
every peer that was down at delete time a chance to learn about the
deletion via either hint replay or anti-entropy diff.

## Example

```bash
bdscmd cluster doc-delete -i 019e1056-d0c9-75c3-927b-dbed1b62bdb2
```

## Error responses

Same as [`v3/doc.add`](v3_doc_add.md#error-responses).
