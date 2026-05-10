# v3/signal.emit

Fully-replicated signal emit.  Writes locally, then fans
`v2/signal.emit` out to every Alive peer with a shared UUIDv7.

Signals are append-only events — there is no `v3/signal.update` or
`v3/signal.delete`.  Anti-entropy pulls missing signal entries the same
way it does for docs.

For the full replication lifecycle see [`v3/doc.add`](v3_doc_add.md).
Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Signal identifier. |
| `severity` | string | yes | E.g. `"info"`, `"warning"`, `"critical"`. |
| `timestamp` | integer | yes | Unix seconds the signal occurred. |
| `metadata` | object | no | Extra fields merged into stored metadata. `name`, `severity`, `timestamp` always take precedence. |
| `id` | string | no | Caller-supplied UUIDv7 (idempotent retries). |

## Response

```json
{
  "id":      "019e1057-…",
  "outcome": { "peers_attempted": 2, "peers_succeeded": 2, "hints_queued": 0 },
  "cluster_meta": { "enabled": true }
}
```

## Example

```bash
bdscmd cluster signal-emit -n disk-full -S critical
```

## Error responses

Same as [`v3/doc.add`](v3_doc_add.md#error-responses).
