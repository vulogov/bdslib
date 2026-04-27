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
| `--new` | — | false | Delete the existing data store and start with a fresh database before binding the listener |

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

## Client

`bdscmd` is the dedicated command-line client for this API. It wraps every
method listed below as its own subcommand, handles the pre-flight server check,
and pretty-prints results. See [../BDSCMD.md](../BDSCMD.md) for the full
reference.

```bash
bdscmd status
bdscmd fulltext -q "kernel panic" -d 1h
bdscmd eval my_script.bund
```

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
| [`v2/status`](v2_status.md) | Live process snapshot: node identity, uptime, timestamp, hostname, and ingest queue depths |
| [`v2/add`](v2_add.md) | Enqueue a single telemetry document for async persistence |
| [`v2/add.batch`](v2_add_batch.md) | Enqueue a list of telemetry documents for async persistence |
| [`v2/add.file`](v2_add_file.md) | Validate and enqueue a file of newline-delimited JSON telemetry documents for async background ingestion |
| [`v2/add.file.syslog`](v2_add_file_syslog.md) | Validate and enqueue an RFC 3164 syslog file for async background ingestion; each line is parsed and converted to a structured telemetry document |
| [`v2/timeline`](v2_timeline.md) | Earliest and latest event timestamps across all shards |
| [`v2/count`](v2_count.md) | Total number of telemetry records, optionally filtered by time window |
| [`v2/shards`](v2_shards.md) | List of shards with time boundaries, path, and primary/secondary counts |
| [`v2/keys`](v2_keys.md) | Unique sorted list of primary record keys within a duration window |
| [`v2/keys.all`](v2_keys_all.md) | Unique sorted list of primary record keys within a duration window, filtered by an optional shell-glob pattern (default `*`) |
| [`v2/keys.get`](v2_keys_get.md) | Primary record IDs and secondary ID lists for keys matching a shell-glob pattern within a duration window |
| [`v2/primaries`](v2_primaries.md) | UUIDs of all primary records, optionally filtered by time window |
| [`v2/primaries.explore`](v2_primaries_explore.md) | Keys with more than one primary record in a duration window, with counts and UUIDs |
| [`v2/primaries.explore.telemetry`](v2_primaries_explore_telemetry.md) | Keys with more than one numeric-data primary in a duration window — suitable for `v2/trends` |
| [`v2/primaries.get`](v2_primaries_get.md) | `data` payloads and timestamps for all primary records matching an exact key within a duration window |
| [`v2/primaries.get.telemetry`](v2_primaries_get_telemetry.md) | Extracted numeric values (`data` or `data["value"]`) for primary records matching an exact key within a duration window |
| [`v2/primary`](v2_primary.md) | Full document for a single primary record by UUID |
| [`v2/secondaries`](v2_secondaries.md) | UUIDs of secondary records associated with a primary |
| [`v2/secondary`](v2_secondary.md) | Full document for a single secondary record by UUID |
| [`v2/duplicates`](v2_duplicates.md) | Map of primary UUID → duplicate timestamps, optionally filtered by time window |
| [`v2/fulltext`](v2_fulltext.md) | Full-text search returning matching primary IDs and BM25 relevance scores |
| [`v2/fulltext.get`](v2_fulltext_get.md) | Full-text search returning complete primary documents with linked secondaries |
| [`v2/fulltext.recent`](v2_fulltext_recent.md) | Full-text search returning IDs, timestamps, and scores sorted by most recent first |
| [`v2/search`](v2_search.md) | Semantic vector search returning primary IDs, timestamps, and similarity scores sorted by score |
| [`v2/search.get`](v2_search_get.md) | Semantic vector search returning complete primary documents sorted by timestamp |
| [`v2/trends`](v2_trends.md) | Statistical trend summary for a single key: min, max, mean, median, std-dev, anomalies, and breakouts |
| [`v2/topics`](v2_topics.md) | LDA topic modelling over a single key's telemetry corpus within a lookback window, returning a keyword summary |
| [`v2/topics.all`](v2_topics_all.md) | LDA topic modelling over every distinct key in the window, returning one keyword summary per key |
| [`v2/rca`](v2_rca.md) | Root cause analysis: cluster non-telemetry events by co-occurrence and rank probable causes of a named failure key |
| [`v2/eval`](v2_eval.md) | Compile and evaluate a BUND VM script in a named context, returning the workbench stack as JSON |
| [`v2/doc.add`](v2_doc_add.md) | Store a document with JSON metadata and text content; auto-embeds both slots in the HNSW index |
| [`v2/doc.add.file`](v2_doc_add_file.md) | Load a text file, split into overlapping chunks, and store each chunk as an independently searchable record |
| [`v2/doc.get`](v2_doc_get.md) | Retrieve both metadata and content text for a document by UUID |
| [`v2/doc.get.metadata`](v2_doc_get_metadata.md) | Retrieve only the JSON metadata for a document by UUID |
| [`v2/doc.get.content`](v2_doc_get_content.md) | Retrieve only the content text for a document by UUID |
| [`v2/doc.update.metadata`](v2_doc_update_metadata.md) | Replace the metadata of a document in-place (vector index not updated automatically) |
| [`v2/doc.update.content`](v2_doc_update_content.md) | Replace the content text of a document in-place (vector index not updated automatically) |
| [`v2/doc.delete`](v2_doc_delete.md) | Remove a document from all three sub-stores (metadata, blob, HNSW); idempotent |
| [`v2/doc.search`](v2_doc_search.md) | Semantic search by plain-text query; returns ranked documents with score, metadata, and content |
| [`v2/doc.search.json`](v2_doc_search_json.md) | Semantic search by JSON query object via json_fingerprint embedding |
| [`v2/doc.search.strings`](v2_doc_search_strings.md) | Semantic search returning results as flat json_fingerprint strings |
