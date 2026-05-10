# v2/signal.get

Fetch a single signal's metadata by UUID.

Phase 5 addition.  The original signal API only exposed lookup by
recency (`v2/signals`) or semantic query (`v2/signals_query`).
Anti-entropy needs a deterministic by-UUID fetch so it can pull a
specific missing signal it learned about via `v2/signal.list_ids`.

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | string | yes | UUIDv7 of the signal. |
| `session` | string | no | UUIDv7 transaction id (echoed only). |

## Response

```json
{
  "id":       "019e1019-3c00-7000-9000-3c0000003c00",
  "metadata": {
    "name":      "disk-full",
    "severity":  "critical",
    "timestamp": 1778000000
  }
}
```

`metadata` is `null` when no signal with that UUID exists locally.

## Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/signal.get","id":1,"params":{"id":"019e1019-3c00-7000-9000-3c0000003c00"}}' \
  | jq
```

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32011` | Signal store read failed |
| `-32602` | Invalid `id` |
