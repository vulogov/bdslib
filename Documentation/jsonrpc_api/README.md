# bdsnode — JSON-RPC 2.0 API

`bdsnode` is the network-facing daemon for bdslib. It exposes a JSON-RPC 2.0 HTTP server backed by the shared `ShardsManager` singleton and the BUND VM runtime.

---

## Running bdsnode

```
bdsnode [OPTIONS]
```

### Options

| Flag | Env var | Default | Description |
|---|---|---|---|
| `-c, --config <PATH>` | `BDS_CONFIG` | — | Path to the hjson configuration file |
| `--host <HOST>` | — | `127.0.0.1` | Address to bind the JSON-RPC listener |
| `-p, --port <PORT>` | — | `9000` | TCP port for the JSON-RPC listener |

### Example

```bash
# use a config file
bdsnode --config /etc/bdslib/config.hjson --host 0.0.0.0 --port 9944

# rely on environment variable
BDS_CONFIG=/etc/bdslib/config.hjson bdsnode --port 9944
```

On startup `bdsnode`:

1. Initialises the DuckDB-backed `ShardsManager` from the config file or `BDS_CONFIG`.
2. Initialises the BUND VM runtime (`init_adam`).
3. Binds the JSON-RPC listener on `host:port`.
4. Runs until `Ctrl-C`, then checkpoints the database (`sync_db`) before exit.

---

## Protocol

All requests use **JSON-RPC 2.0** over plain HTTP `POST` to the server root (`/`).

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"<method>","params":{...},"id":1}' | jq
```

Notifications (requests without an `"id"` field) are not used; always include an `"id"`.

### Time window parameters

Several methods accept an optional time window. Exactly one of the three forms may be used; if none is provided the method queries all data.

| Parameter | Type | Description |
|---|---|---|
| `duration` | string | Lookback window from now, e.g. `"1h"`, `"30min"`, `"7d"` |
| `start_ts` | integer | Range start as Unix seconds (must be paired with `end_ts`) |
| `end_ts` | integer | Range end as Unix seconds (must be paired with `start_ts`) |

### Error codes

| Code | Meaning |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32002` | Shard index query failed |
| `-32003` | Shard open failed |
| `-32004` | Observability query failed |
| `-32005` | Relationship lookup failed |
| `-32404` | Record not found |
| `-32600` | Invalid parameter (bad UUID, bad duration string, etc.) |

---

## API Reference

| Method | Description |
|---|---|
| [`v2/add`](v2_add.md) | Enqueue a single telemetry document for async persistence |
| [`v2/add.batch`](v2_add_batch.md) | Enqueue a list of telemetry documents for async persistence |
| [`v2/timeline`](v2_timeline.md) | Earliest and latest event timestamps across all shards |
| [`v2/count`](v2_count.md) | Total number of telemetry records, optionally filtered by time window |
| [`v2/shards`](v2_shards.md) | List of shards with time boundaries, path, and primary/secondary counts |
| [`v2/keys`](v2_keys.md) | Unique sorted list of primary record keys, optionally filtered by time window |
| [`v2/primaries`](v2_primaries.md) | UUIDs of all primary records, optionally filtered by time window |
| [`v2/primary`](v2_primary.md) | Full document for a single primary record by UUID |
| [`v2/secondaries`](v2_secondaries.md) | UUIDs of secondary records associated with a primary |
| [`v2/secondary`](v2_secondary.md) | Full document for a single secondary record by UUID |
| [`v2/duplicates`](v2_duplicates.md) | Map of primary UUID → duplicate timestamps, optionally filtered by time window |
| [`v2/fulltext`](v2_fulltext.md) | Full-text search returning matching primary IDs and BM25 relevance scores |
| [`v2/fulltext.get`](v2_fulltext_get.md) | Full-text search returning complete primary documents with linked secondaries |
