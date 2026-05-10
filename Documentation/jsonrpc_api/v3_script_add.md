# v3/script.add

Fully-replicated BUND script add.  Stamps `metadata.updated_at`, writes
locally, then fans `v2/script_add` out to every Alive peer with a shared
UUIDv7.

For the full replication lifecycle see [`v3/doc.add`](v3_doc_add.md).
Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `metadata` | object | yes | Script metadata.  Must contain non-empty `name` and `schedule` (crontab string).  Coordinator stamps `updated_at`. |
| `script` | string | yes | BUND source code (UTF-8). |
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
bdscmd cluster script-add \
  -m '{"name":"hourly-cleanup","schedule":"0 * * * *"}' \
  -b "data 'cleanup' results"
```

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32600` | Local script add failed (e.g. invalid metadata: missing `name`/`schedule`) |
| `-32602` | Invalid params (malformed `id`) |
| `-32011` | Idempotent-existence check failed |
