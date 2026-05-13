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
| `-32099` | Ingest channel overloaded — back off and retry (only from `v2/add`, `v2/add.batch`, `v2/add.file`, `v2/add.file.syslog`) |
| `-32404` | Record not found |
| `-32600` | Invalid parameter (bad UUID, bad duration string, etc.) |

---

## API Reference

| Method | Description |
|---|---|
| [`v2/status`](v2_status.md) | Live process snapshot: node identity, uptime, timestamp, hostname, and ingest queue depths |
| [`v2/cluster.peers`](v2_cluster_peers.md) | Unauthenticated read of the local peer table (mode, peer states, replication knobs). Returns `{enabled: false, peers: []}` when cluster mode is off. Phase 1 of the [cluster layer](../CLUSTER.md). |
| [`v3/cluster.hello`](v3_cluster_hello.md) | HMAC-authenticated handshake: caller sends identity, receiver registers it and echoes back its own identity + peer view. |
| [`v3/cluster.peers`](v3_cluster_peers.md) | HMAC-authenticated read of the local peer table (used by the gossip loop every 3rd tick to converge membership). |
| [`v3/cluster.ping`](v3_cluster_ping.md) | HMAC-authenticated lightweight liveness probe; receiver returns its `node_id` + wall-clock `ts`. |
| [`v3/cluster.status`](v3_cluster_status.md) | HMAC-authenticated compact summary: mode, peer counts, replication factor, embedding model. |
| [`v2/fingerprints.recent`](v2_fingerprints_recent.md) | Raw `(uuid, fingerprint)` pairs for every primary in the lookback window; input source for the v3 distributed-analytics endpoints. |
| [`v3/timeline`](v3_timeline.md) | Cluster-wide earliest+latest timestamps; min/max merge across local + every Alive peer's `v2/timeline`. Phase 2 of the [cluster layer](../CLUSTER.md). |
| [`v3/count`](v3_count.md) | Cluster-wide record count; sum across local + every Alive peer's `v2/count` (with documented caveat for Phase 3 replication). |
| [`v3/search`](v3_search.md) | Cluster-wide semantic vector search; per-peer `v2/search` results merged + UUID-deduped + score-sorted + truncated. |
| [`v3/knn`](v3_knn.md) | Cluster-wide k-NN; per-peer `v2/fingerprints.recent` deduped by UUID; analysis runs once on the union. |
| [`v3/anomaly.recent`](v3_anomaly_recent.md) | Cluster-wide n-gram anomaly detection; same fan-out recipe as v3/knn. |
| [`v3/denoise.recent`](v3_denoise_recent.md) | Cluster-wide n-gram noise removal; same fan-out recipe as v3/knn. |
| [`v3/add`](v3_add.md) | Replicated single-document write: local sync + fire-and-forget fan-out to RF-1 random Alive peers, with hinted-handoff retry on failure. Phase 3 of the [cluster layer](../CLUSTER.md). |
| [`v3/add.batch`](v3_add_batch.md) | Replicated batch write: same recipe as v3/add but uses `v2/add.batch` with `sync: true` for one round-trip per peer. |
| [`v2/doc.list_ids`](v2_doc_list_ids.md) · `v2/signal.list_ids` · `v2/script.list_ids` | Cheap UUID + `updated_at` enumeration plus a tombstone list — input source for the Phase 4 anti-entropy pull-sync. |
| [`v3/doc.add`](v3_doc_add.md) | Fully-replicated docstore add: local sync + fan-out to **every** Alive peer with shared UUID. Phase 4 canonical pattern. |
| [`v3/doc.update.metadata`](v3_doc_update_metadata.md) | Fully-replicated docstore metadata update; bumps `metadata.updated_at` for LWW. |
| [`v3/doc.update.content`](v3_doc_update_content.md) | Fully-replicated docstore content update. |
| [`v3/doc.delete`](v3_doc_delete.md) | Fully-replicated docstore delete with shared tombstone (so anti-entropy doesn't resurrect). |
| [`v3/signal.emit`](v3_signal_emit.md) | Fully-replicated signal emit (signals are append-only — no update/delete). |
| [`v3/script.add`](v3_script_add.md) | Fully-replicated BUND script add. |
| [`v3/script.update`](v3_script_update.md) | Fully-replicated BUND script update. |
| [`v3/script.delete`](v3_script_delete.md) | Fully-replicated BUND script delete with tombstone. |
| [`v3/cluster.sync`](v3_cluster_sync.md) | HMAC-authenticated admin RPC: force an immediate hint replay + anti-entropy tick. Phase 5 of the [cluster layer](../CLUSTER.md). |
| [`v2/signal.get`](v2_signal_get.md) | Fetch a single signal's metadata by UUID. Used by anti-entropy to pull a missing signal. |
| `v3/add.file` | Replicated NDJSON ingest: coordinator parses the file, then submits the records through the `v3/add.batch` path. Phase 6 of the [cluster layer](../CLUSTER.md). |
| `v3/add.file.syslog` | Replicated RFC 3164 syslog ingest: same recipe as `v3/add.file` but uses the syslog parser. |
| `v3/fulltext`, `v3/fulltext.get`, `v3/fulltext.recent` | Cluster-wide BM25 full-text search; UUID dedup, score average / first-seen / newest-first per method. |
| `v3/keys`, `v3/keys.all`, `v3/keys.get` | Cluster-wide key enumeration; sorted string union plus per-key UUID-set merge. |
| `v3/primaries`, `v3/primaries.explore`(`.telemetry`), `v3/primaries.get`(`.telemetry`) | Cluster-wide primary record listings; UUID dedup or per-key count + UUID-set merge. |
| `v3/topics`, `v3/topics.all` | Cluster-wide LDA topic analysis; pick the largest-corpus result per key (LDA isn't directly mergeable). |
| `v3/rca`, `v3/rca.templates` | Cluster-wide root-cause analysis; pick the largest-corpus result (same strategy as v3/topics — RCA is corpus-relative, not directly mergeable). |
| `v3/signals`, `v3/signals_query` | Cluster-wide signal queries; UUID dedup with score average for semantic search. |
| `v3/tpl.list`, `v3/tpl.search`, `v3/tpl.get`, `v3/tpl.template_by_id`, `v3/tpl.templates_recent`, `v3/tpl.templates_by_timestamp` | Cluster-wide template-store reads; UUID dedup with first-non-null-peer-wins for single-record fetches. |
| `v3/search.get` | Cluster-wide semantic vector search returning full documents. UUID dedup + score average. |
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
| [`v2/rca.templates`](v2_rca_templates.md) | Root cause analysis on drain3 template observations: cluster template bodies by co-occurrence and rank probable causes of a named failure template |
| [`v2/textrank.templates`](v2_textrank.templates.md) | Extractive TextRank summary of every drain3 template observed in a lookback window — fingerprints each template and returns the highest-ranked ones joined as a single string |
| [`v2/summary_for_recent`](v2_summary_for_recent.md) | Extractive TextRank summary of text-bearing primary records observed in a lookback window — skips numeric measurements, extracts bodies from `data["value"]` or `data["raw"]` |
| [`v2/summary_for_query`](v2_summary_for_query.md) | Extractive TextRank summary of primary records matching a vector query — same body-extraction rule as `v2/summary_for_recent`; default lookback is 365 days |
| [`v2/summary_lsa_for_recent`](v2_summary_lsa_for_recent.md) | Extractive LSA summary of text-bearing primary records observed in a lookback window — same body-extraction rule as `v2/summary_for_recent`; uses SVD-based Steinberger-Ježek scoring |
| [`v2/summary_lsa_for_query`](v2_summary_lsa_for_query.md) | Extractive LSA summary of primary records matching a vector query — same body-extraction and lookup as `v2/summary_for_query`; LSA backend |
| [`v2/anomaly.recent`](v2_anomaly_recent.md) | N-gram anomaly detection over recent primary records — fingerprints each record (key + `json_fingerprint(data)`) and feeds the strings to `bdslib::analysis::ngram::ngram_anomaly_with`; returns its JSON verbatim |
| [`v2/denoise.recent`](v2_denoise_recent.md) | N-gram noise removal over recent primary records — same fingerprinting as `v2/anomaly.recent`, fed to `bdslib::analysis::ngram::ngram_remove_noise_with`; splits the corpus into `kept` (signal) and `removed` (noise) |
| [`v2/knn`](v2_knn.md) | k-NN intelligence over recent primary records — same fingerprinting as `v2/anomaly.recent`, fed to `bdslib::analysis::knn::knn_summary_with`; returns clusters, density-ranked representatives, and isolated outliers as one structured JSON document |
| [`v2/tpl.add`](v2_tpl_add.md) | Manually store a template (name, body, tags, description) in the per-shard tplstorage |
| [`v2/tpl.get`](v2_tpl_get.md) | Fetch a template's metadata and body by UUID |
| [`v2/tpl.list`](v2_tpl_list.md) | List every template (manual + drain3) stored in shards overlapping a humantime window, metadata only |
| [`v2/tpl.search`](v2_tpl_search.md) | Semantic vector search over templates within a humantime window, ranked by cosine similarity |
| [`v2/tpl.update`](v2_tpl_update.md) | Update one or more fields (name, body, tags, description) of a template by UUID — partial merge |
| [`v2/tpl.delete`](v2_tpl_delete.md) | Remove a template (metadata + body + vector entry) by UUID; idempotent |
| [`v2/tpl.reindex`](v2_tpl_reindex.md) | Rebuild the tplstorage HNSW index for every shard overlapping a humantime window |
| [`v2/tpl.template_by_id`](v2_tpl_template_by_id.md) | Fetch a single drain3 template document by UUID, scanning all shards |
| [`v2/tpl.templates_by_timestamp`](v2_tpl_templates_by_timestamp.md) | List drain3 template documents whose FrequencyTracking observation falls within an explicit Unix-second range |
| [`v2/tpl.templates_recent`](v2_tpl_templates_recent.md) | List drain3 template documents whose FrequencyTracking observation falls within a humantime lookback window |
| [`v2/signal.emit`](v2_signal_emit.md) | Emit a signal — name + severity + timestamp + arbitrary metadata — into the per-shard signal store |
| [`v2/signal.update`](v2_signal_update.md) | Replace a signal's metadata in-place by UUID (full overwrite, not merge) |
| [`v2/signals`](v2_signals.md) | List signals observed within a humantime window, with full metadata resolved per signal |
| [`v2/signals_query`](v2_signals_query.md) | Semantic search over the signal store by plain-text query, ranked by cosine similarity |
| [`v2/chat.ollama`](v2_chat_ollama.md) | Send a question to a local Ollama model with retrieval-augmented context drawn from observability + document stores; supports stateful sessions via `chat_id` |
| [`v2/eval`](v2_eval.md) | Compile and evaluate a BUND VM script in a named context.  Returns `{result, cluster_meta}` — `cluster_meta` carries the most-recent `cls.*` helper's fan-out/replication summary or `null`. |
| [`v2/eval.queued`](v2_eval_queued.md) | Submit a BUND script to the worker pool for async execution; returns a result-queue id immediately |
| [`v2/scheduler.last_seen`](v2_scheduler_last_seen.md) | Read **this node's** most-recent local execution timestamp for a stored script.  Used by the cluster-aware Scheduler dedup; surfaced via `bdscmd scheduler-last-seen` |
| [`v3/user.add`](v3_user_add.md) | Create a new user (HMAC-protected, first-user bootstrap exception); local + replicate to every Alive peer |
| [`v3/user.modify`](v3_user_modify.md) | Partial update with LWW; HMAC-protected; local + replicate |
| [`v3/user.delete`](v3_user_delete.md) | Hard delete + tombstone; HMAC-protected; local + replicate |
| [`v3/user.authenticate`](v3_user_authenticate.md) | Public login path (NOT HMAC).  Local verify + peer fan-out fallback; issues stateless HMAC-signed session token |
| [`v3/user.list`](v3_user_list.md) | Hash-free admin listing (HMAC-protected) |
| [`v2/user.*`](v2_user.md) | Receiver methods for cluster replication: `add`, `modify`, `delete`, `get_by_username`, `get_by_id`, `list_ids` |
| [`v2/aggregationsearch`](v2_aggregationsearch.md) | Parallel vector search over time-scoped telemetry shards + semantic document store search; returns `"observability"` and `"documents"` |
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
| [`v2/doc.reindex`](v2_doc_reindex.md) | Rebuild the HNSW vector index from persisted metadata and blobs; use after unclean shutdown or bulk content updates |
| [`v2/results.len`](v2_results_len.md) | Number of result queues currently tracked, with their UUIDs |
| [`v2/results.push`](v2_results_push.md) | Push a JSON value onto the back of the result queue identified by `id`; auto-creates the queue with a fresh creation timestamp |
| [`v2/results.pull`](v2_results_pull.md) | Pop the front value from the result queue identified by `id`; returns the value as JSON plus `remaining` count |
| [`v2/results.empty`](v2_results_empty.md) | Number of elements in the result queue identified by `id`, with `empty` boolean |
| [`v2/script_add`](v2_script_add.md) | Store a new BUND script — metadata must contain `name` and `schedule` (crontab-style); returns the assigned UUIDv7 |
| [`v2/scripts`](v2_scripts.md) | List every stored BUND script with `id`, `name`, `schedule`, and the full metadata document |
| [`v2/script`](v2_script.md) | Fetch a single BUND script body and metadata by UUIDv7 |
| [`v2/script_update`](v2_script_update.md) | Replace metadata and body of an existing script (full overwrite, not merge) |
| [`v2/script_delete`](v2_script_delete.md) | Remove a script from all sub-stores; idempotent |
| [`v4/llm.complete`](v4_llm_complete.md) | Single-shot text completion (HMAC-signed).  Cluster-aware via the cache (Phase 3) + dedup (Phase 4) layers.  See [`../LLM.md`](../LLM.md). |
| [`v4/llm.chat`](v4_llm_chat.md) | Stateful chat turn — history persisted in the docstore; optional inline RAG via `duration` |
| [`v4/llm.analyze`](v4_llm_analyze.md) | RAG over a `ContextSource` kind (`aggregation`/`knn`/`rca`/`anomaly`/`templates`/`telemetry`/`documents`/`supplied`) then one completion |
| [`v4/llm.embed`](v4_llm_embed.md) | Vector embeddings (batch) |
| [`v4/llm.providers.list`](v4_llm_providers_list.md) | Registered providers + capabilities + the default |
| [`v4/llm.complete_async`](v4_llm_complete_async.md) | Enqueue a completion for the background runner; returns `{job_id, result_id}` for `v2/results.pull` |
| [`v4/llm.analyze_async`](v4_llm_analyze_async.md) | Enqueue an analyze job; same result-delivery channel |
| [`v4/llm.jobs.list`](v4_llm_jobs_list.md) | List queued / in-flight / terminal jobs |
| [`v4/llm.jobs.status`](v4_llm_jobs_status.md) | Inspect a single job by id |
| [`v4/llm.jobs.cancel`](v4_llm_jobs_cancel.md) | Cancel a pending or running job (idempotent on terminal states) |
| [`v4/llm.cache.stats`](v4_llm_cache_stats.md) | Inference-cache totals: rows / total hits / response bytes / TTL |
| [`v4/llm.cache.purge`](v4_llm_cache_purge.md) | Drop matching cache rows (provider / kind / older-than filters; empty = drop all) |
| [`v2/llm.cache.{get,get.by_id,put,list_ids,delete}`](v2_llm_cache.md) | Unauthenticated receivers used by `replicate_to_all`, anti-entropy, and (future) cluster-wide cache reads |
| [`v2/llm.last_executed`](v2_llm_last_executed.md) | Most-recent inference-log row for a `cache_key` — peer of `v2/scheduler.last_seen`; used by the dedup fan-out |
| [`v2/to.bund`](v2_to_bund.md) | LLM-based English → Bund translator.  Parses + dry-runs the returned ```` ```bund```` block; retries on validation failure up to `llm.to_bund.max_retries`.  Companion `v2/to.bund.settings` echoes the effective config + active sandbox blocklist.  See [`../LLM.md`](../LLM.md). |
| [`v3/help`](v3_help.md) | Docstore-backed LLM Q&A.  Retrieves top-`limit` matching documents from the fully-replicated cluster docstore (optional `internal_only` filter for the `metadata.internal_doc == true` corpus loaded by `scripts/load_internal_documentation.sh`), then asks the default LLM to answer using those documents as RAG context.  Returns `{answer, sources[], n_docs, …}`.  Companion `v3/help.settings`. |
| [`v2/retention.sweep`](v2_retention.md) · `v2/retention.settings` | Operator-triggered shard eviction sweep + read-only echo of the active retention config.  Reuses the same code path the background tokio task drives.  See [`../RETENTION.md`](../RETENTION.md). |
| [`v3/cluster.retention.status`](v3_cluster_retention.md) | Cluster-wide retention introspection.  Read-only fan-out of `v2/retention.settings` across every Alive peer plus this node; adds a `summary` block that surfaces policy drift (`consistent`, `distinct_durations`, aggregated lifetime counts). |
| [`v2/cluster.shards.list`](v2_cluster_shards_list.md) | Peer-cheap shard catalog dump.  Phase 3 retention quorum probes call this against every Alive sibling to build a `(interval → peer-count)` map before evicting. |
