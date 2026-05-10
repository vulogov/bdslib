# v3/script.update

Fully-replicated BUND script update.  Bumps `metadata.updated_at`,
writes both metadata and body locally, then fans `v2/script_update` out
to every Alive peer.

For the full replication lifecycle see [`v3/doc.add`](v3_doc_add.md).
Conflict semantics: same caveat as [`v3/doc.update.metadata`](v3_doc_update_metadata.md#conflict-semantics--known-limitation).

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | string | yes | UUIDv7 of the script to update. |
| `metadata` | object | yes | Replacement metadata (must contain `name` and `schedule`). |
| `script` | string | yes | Replacement BUND source code. |

## Response

```json
{
  "id":      "019e1057-…",
  "updated": true,
  "outcome": { "peers_attempted": 2, "peers_succeeded": 2, "hints_queued": 0 },
  "cluster_meta": { "enabled": true }
}
```

## Error responses

Same as [`v3/script.add`](v3_script_add.md#error-responses).
