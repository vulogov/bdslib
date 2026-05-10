# v2/doc.list_ids · v2/signal.list_ids · v2/script.list_ids

Cheap UUID + `updated_at` enumeration plus a tombstone list, used by the
[anti-entropy](../CLUSTER.md#7-rpc-surface) pull-sync to compute what each
peer is missing.

All three methods share the same shape — only the underlying store differs:

| Method | Store |
|---|---|
| `v2/doc.list_ids` | docstore (`{dbpath}/docs`) |
| `v2/signal.list_ids` | signal store (`{dbpath}/signals`) |
| `v2/script.list_ids` | BUND script store (`{dbpath}/scripts`) |

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

None. All three methods ignore any params supplied.

## Response

```json
{
  "store":        "docs",
  "n_live":       1,
  "n_tombstones": 0,
  "live": [
    { "id": "019e1056-…", "updated_at": 1778390585 }
  ],
  "tombstones": [
    { "id": "019e1057-…", "deleted_at": 1778390602 }
  ]
}
```

| Field | Description |
|---|---|
| `store` | `"docs"`, `"signals"`, or `"scripts"`. |
| `n_live` | Length of `live[]`. |
| `n_tombstones` | Length of `tombstones[]`. |
| `live[].id` | Stable UUIDv7 of the live record. |
| `live[].updated_at` | Unix seconds extracted from `metadata.updated_at`, falling back to `metadata.timestamp`, then `0` for legacy records that have neither. Used by anti-entropy for last-write-wins comparison. |
| `tombstones[].id` | UUIDv7 of a deleted record. |
| `tombstones[].deleted_at` | Unix seconds the deletion was issued. |

When cluster mode is disabled (`cluster.enabled = false`), the
`tombstones[]` array is always empty — without the cluster layer there's
no tombstone storage. The `live[]` walk still works.

## Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/doc.list_ids","id":1,"params":{}}' \
  | jq '.result | {n_live, n_tombstones}'
```

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32004` | Underlying store walk failed |

## Notes

- **No pagination.** Both arrays return the full set in one response. For
  very large stores (>10⁶ entries) consider the future Phase 5 streaming
  variant; today the cost is bounded by the payload size, which scales
  linearly with the entry count.
- **Order.** `live[]` is in `list_metadata()` natural order (typically
  insertion / UUIDv7 order); `tombstones[]` is sorted by `deleted_at`
  descending (most-recent-first).
- **Anti-entropy diff.** The peer that's running anti-entropy computes
  the set difference locally (in-memory hash sets) and pulls the
  delta via `v2/doc.get` / `v2/script` etc.  See
  [CLUSTER.md § 7](../CLUSTER.md#7-rpc-surface).
