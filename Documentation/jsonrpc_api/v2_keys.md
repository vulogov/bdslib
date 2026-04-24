# v2/keys

Returns the unique, sorted list of keys present on primary telemetry records across all shards. Supports optional time window filtering.

Keys are the `key` field stored on each primary document — they identify the logical type or category of a record (e.g. `"server.cpu"`, `"http.request"`).

## Parameters

All parameters are optional. See [time window parameters](README.md#time-window-parameters) for details.

| Parameter | Type | Description |
|---|---|---|
| `duration` | string | Lookback window from now, e.g. `"1h"`, `"24h"`, `"7d"` |
| `start_ts` | integer | Range start as Unix seconds |
| `end_ts` | integer | Range end as Unix seconds |

## Response

```json
{
  "keys": [
    "http.request",
    "server.cpu",
    "server.memory"
  ]
}
```

| Field | Type | Description |
|---|---|---|
| `keys` | array of strings | Alphabetically sorted, deduplicated list of primary record keys |

## Examples

```bash
# all keys ever stored
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/keys","params":{},"id":1}' | jq

# keys seen in the last 30 minutes
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/keys","params":{"duration":"30min"},"id":1}' | jq
```

```json
{
  "jsonrpc": "2.0",
  "result": {
    "keys": ["http.request", "server.cpu", "server.memory"]
  },
  "id": 1
}
```

## Notes

- Deduplication and sorting are performed in a single pass using a `BTreeSet`, so the result is always alphabetically ordered regardless of which shards contributed entries.
- Only `is_primary = 1` records are considered; secondary records are excluded.
