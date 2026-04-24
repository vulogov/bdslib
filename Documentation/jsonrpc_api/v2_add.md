# v2/add

Enqueues a single JSON telemetry document into the `"ingest"` crossbeam channel for asynchronous persistence by the batch-ingestion thread.

The call returns as soon as the document is accepted by the channel — persistence happens in the background. The document is committed to the shard store when either the batch is full (`pipe_batch_size`) or the idle timeout (`pipe_timeout_ms`) elapses.

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `doc` | object | yes | The JSON telemetry document to ingest. Must contain a `"timestamp"` field (Unix seconds) so the shard router can place it in the correct shard. |

## Response

```json
{ "queued": 1 }
```

| Field | Type | Description |
|---|---|---|
| `queued` | integer | Always `1` — confirms the document was accepted by the channel. |

## Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "method": "v2/add",
    "params": {
      "doc": {
        "timestamp": 1745042000,
        "key": "server.cpu",
        "host": "web-01",
        "value": 87.3
      }
    },
    "id": 1
  }' | jq
```

```json
{
  "jsonrpc": "2.0",
  "result": { "queued": 1 },
  "id": 1
}
```

## Error responses

| Code | Condition |
|---|---|
| `-32001` | The `"ingest"` channel is unavailable (pipe registry not initialized). |

## Notes

- The `"ingest"` channel is unbounded so this call never blocks waiting for the consumer.
- Use [`v2/add.batch`](v2_add_batch.md) to enqueue multiple documents in a single request.
- Persistence is asynchronous — a successful response means the document is queued, not yet written to disk.
