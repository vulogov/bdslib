# v2/add.batch

Enqueues a list of JSON telemetry documents into the `"ingest"` crossbeam channel for asynchronous persistence by the batch-ingestion thread.

Each document is pushed individually so the consumer can interleave them with documents from other sources. The call returns as soon as all documents are accepted by the channel.

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `docs` | array of objects | yes | The JSON telemetry documents to ingest. Each must contain a `"timestamp"` field (Unix seconds). |

## Response

```json
{ "queued": 42 }
```

| Field | Type | Description |
|---|---|---|
| `queued` | integer | Number of documents accepted by the channel (equals `len(docs)`). |

Returns `{ "queued": 0 }` for an empty `docs` array.

## Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "method": "v2/add.batch",
    "params": {
      "docs": [
        {"timestamp": 1745042000, "key": "server.cpu",    "host": "web-01", "value": 87.3},
        {"timestamp": 1745042001, "key": "server.memory", "host": "web-01", "value": 4096},
        {"timestamp": 1745042002, "key": "server.cpu",    "host": "web-02", "value": 23.1}
      ]
    },
    "id": 1
  }' | jq
```

```json
{
  "jsonrpc": "2.0",
  "result": { "queued": 3 },
  "id": 1
}
```

## Error responses

| Code | Condition |
|---|---|
| `-32001` | The `"ingest"` channel is unavailable (pipe registry not initialized). Sending stops on the first failure; already-queued documents in the same request are not rolled back. |

## Notes

- The `"ingest"` channel is unbounded so this call never blocks.
- Documents are forwarded to the batch thread one at a time; the thread may combine them with records from concurrent `v2/add` calls into the same DuckDB batch.
- Persistence is asynchronous — a successful response means all documents are queued, not yet written to disk.
- For single-document ingestion use [`v2/add`](v2_add.md).
