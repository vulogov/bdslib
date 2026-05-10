# v3/doc.update.content

Fully-replicated document content update.  Writes the new content body
locally, then fans `v2/doc.update.content` out to every Alive peer.

Note: this method **does not** stamp `metadata.updated_at` — only the
content blob is touched.  If you want anti-entropy to consider this
update as "newer" for LWW comparison, follow up with
[`v3/doc.update.metadata`](v3_doc_update_metadata.md) to bump the
metadata's `updated_at`.

For the full replication lifecycle see [`v3/doc.add`](v3_doc_add.md).
Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | string | yes | UUIDv7 of the doc to update. |
| `content` | string | yes | Replacement UTF-8 content body. |

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

Same as [`v3/doc.update.metadata`](v3_doc_update_metadata.md#conflict-semantics--known-limitation).

## Error responses

Same as [`v3/doc.add`](v3_doc_add.md#error-responses).
