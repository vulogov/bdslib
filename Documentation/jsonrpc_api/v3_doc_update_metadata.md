# v3/doc.update.metadata

Fully-replicated document metadata update.  Bumps `metadata.updated_at` to
wall-clock now, writes locally, then fans `v2/doc.update.metadata` out to
every Alive peer.

For the full replication lifecycle see [`v3/doc.add`](v3_doc_add.md).
Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | string | yes | UUIDv7 of the doc to update. |
| `metadata` | object | yes | Replacement metadata. The coordinator stamps `updated_at = now()` into this object before writing. |

## Response

```json
{
  "id":      "019e1056-…",
  "updated": true,
  "outcome": { "peers_attempted": 2, "peers_succeeded": 2, "hints_queued": 0 },
  "cluster_meta": { "enabled": true }
}
```

## Conflict semantics

Phase 5 closes the Phase 4 LWW gap on this method:

- **Real-time fan-out** sets `if_newer: true` on the underlying
  `v2/doc.update.metadata` calls.  Each receiver compares the incoming
  `metadata.updated_at` against the locally-stored value and only
  applies the update if the incoming value is strictly greater.
  Concurrent partition updates therefore **don't overwrite newer state**
  with older arrival-order writes.
- **Anti-entropy** also pulls updates: when the per-store
  `v2/doc.list_ids` diff shows a UUID present on both sides but
  `remote.updated_at > local.updated_at`, the local copy is overwritten
  with the remote one (via `v2/doc.get.metadata` + `v2/doc.get.content`
  followed by local `update_metadata` + `update_content`).

Remaining edge cases:

- **Wall-clock skew.** LWW uses Unix seconds from each coordinator's
  wall clock. Skewed clocks can favour the wrong side; for typical
  NTP-synced fleets the seconds-resolution skew is negligible.
- **Same-second writes.** Two updates hitting different coordinators in
  the same Unix second are tied — the receiver applies whichever
  arrived first (`if_newer` checks `>`, not `>=`).

## Error responses

Same as [`v3/doc.add`](v3_doc_add.md#error-responses).
