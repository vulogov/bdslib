# v3/script.delete

Fully-replicated BUND script delete.  Same recipe as [`v3/doc.delete`](v3_doc_delete.md):
deletes locally, writes a tombstone, fans `v2/script_delete` out to every
Alive peer with a shared `deleted_at`.

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | string | yes | UUIDv7 of the script to delete. |

## Response

```json
{
  "id":         "019e1057-…",
  "deleted":    true,
  "deleted_at": 1778390602,
  "outcome": { "peers_attempted": 2, "peers_succeeded": 2, "hints_queued": 0 },
  "cluster_meta": { "enabled": true }
}
```

## Example

```bash
bdscmd cluster script-delete -i 019e1057-d0c9-75c3-927b-dbed1b62bdb2
```

## Error responses

Same as [`v3/doc.add`](v3_doc_add.md#error-responses).
