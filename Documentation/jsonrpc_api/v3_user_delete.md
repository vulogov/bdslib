# v3/user.delete

Hard-delete a user.  HMAC-required.

Local delete + write a tombstone in the shared `tombstones.duckdb`
(scoped to store=`"users"`) + replicate `v2/user.delete` to every
Alive peer.  Anti-entropy applies remote tombstones on the next
tick so a peer that was Dead at delete time catches up via either
hinted handoff or AE — whichever recovers first.

## Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `id`     | string | yes | UUIDv7 of the user. |
| `_hmac`  | string | yes | HMAC-SHA256 over canonical params. |

## Response

```json
{
  "id":         "019e15a2-4484-7b32-954b-61d63221d32d",
  "deleted":    true,
  "deleted_at": 1778479479,
  "outcome":    { "peers_attempted": 2, "peers_succeeded": 2, "hints_queued": 0 },
  "cluster_meta": { "enabled": false }
}
```

`deleted_at` is the Unix-seconds timestamp written into the
tombstone.  All replicas record the same `deleted_at` so AE doesn't
see a tombstone-timestamp disagreement.

## Errors

| Code | Condition |
|---|---|
| `-32001` | DB not initialised |
| `-32097` | Cluster mode disabled |
| `-32098` | Missing / invalid `_hmac` |
| `-32011` | UserStorage rejection |
| `-32602` | Missing or invalid `id` |

## Example

```bash
bdscmd user --secret "$SECRET" delete -i 019e15a2-…
```
