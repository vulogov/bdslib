# bdslib — Documentation

Reference documentation for the bdslib library, the bdsnode server, and the
three CLI tools that communicate with it.

---

## Table of Contents

1. [System Overview](#1-system-overview)
2. [Library — API & Engines](#2-library--api--engines)
3. [Tools](#3-tools)
4. [JSON-RPC API (bdsnode)](#4-json-rpc-api-bdsnode)
5. [BUND Scripting VM](#5-bund-scripting-vm)
6. [Scripts](#6-scripts)
7. [Examples](#7-examples)
8. [Tests](#8-tests)
9. [Reference Files](#9-reference-files)

---

## 1. System Overview

```
┌─────────────────────────────────────────────┐
│                 Applications                │
│  bdscli        bdscmd        bdsweb         │
│  (local CLI)   (RPC client)  (web UI)       │
└────────────────────┬────────────────────────┘
                     │ JSON-RPC 2.0
┌────────────────────▼────────────────────────┐
│                   bdsnode                   │
│         JSON-RPC server (port 9000)         │
└────────────────────┬────────────────────────┘
                     │ Rust API
┌────────────────────▼────────────────────────┐
│                   bdslib                    │
│  ShardsManager → Shard (DuckDB + FTS + Vec) │
│  DocumentStorage · ObservabilityStorage     │
│  EmbeddingEngine · BUND VM                  │
└─────────────────────────────────────────────┘
```

**bdslib** is the core Rust library. It is embedded directly into **bdsnode**,
which exposes the full API over JSON-RPC 2.0. Three tools talk to bdsnode:

| Tool | Purpose |
|------|---------|
| `bdscli` | Local CLI operating directly on a database file |
| `bdscmd` | JSON-RPC client — one subcommand per API method |
| `bdsweb` | Dark-themed web UI with seven analytical pages |

---

## 2. Library — API & Engines

Internal library modules, each documented independently.

### Storage

| Document | Component | Description |
|----------|-----------|-------------|
| [STORAGEENGINE.md](STORAGEENGINE.md) | `StorageEngine` | DuckDB-backed core SQL engine with R2D2 connection pool |
| [SHARD.md](SHARD.md) | `Shard` | Single time-partition: telemetry table, FTS index, vector index |
| [SHARDSCACHE.md](SHARDSCACHE.md) | `ShardsCache` | LRU shard handle cache and open-shard pool |
| [SHARDSMANAGER.md](SHARDSMANAGER.md) | `ShardsManager` | Time-partitioned shard lifecycle, ingestion queues, and cross-shard querying |
| [OBSERVABILITYENGINE.md](OBSERVABILITYENGINE.md) | `ObservabilityStorage` | redb key-value store for dedup tracking and secondary records |
| [DOCUMENTSENGINE.md](DOCUMENTSENGINE.md) | `DocumentStorage` | Combined metadata, blob, and vector store with unified similarity search |

### Search & Analysis

| Document | Component | Description |
|----------|-----------|-------------|
| [EMBEDDINGENGINE.md](EMBEDDINGENGINE.md) | `EmbeddingEngine` | fastembed-based vector embedding generation |
| [FTSENGINE.md](FTSENGINE.md) | `FTSEngine` | Tantivy full-text search index management |
| [VECTORENGINE.md](VECTORENGINE.md) | `VectorEngine` | HNSW vector index backed by VecStore |

### Utilities

| Document | Component | Description |
|----------|-----------|-------------|
| [COMMON.md](COMMON.md) | Common | Error handling, JSON fingerprint, math helpers, time ranges, UUID utilities |

---

## 3. Tools

### bdscli — Local CLI

[BDSCLI.md](BDSCLI.md)

Operates directly on a DuckDB database file without a running server.
Commands: `init`, `generate`, `ingest`, `get`, `search`, `analyze`.
Use for local data exploration, testing, and scripting.

### bdscmd — JSON-RPC Client

[BDSCMD.md](BDSCMD.md)

Full-featured command-line client for every `v2/*` method exposed by bdsnode.
One subcommand per API method. Results printed as pretty JSON; `--raw` for
compact output suitable for piping into `jq`. Supports shebang-based BUND
script execution.

### bdsweb — Web Interface

[BDSWEB.md](BDSWEB.md)

Dark-themed web UI served by `bdsweb`, connecting to bdsnode over JSON-RPC.
Seven pages covering system health, telemetry search, log search with LDA
topic analysis, document retrieval, aggregated search, trend analysis, and
an interactive BUND scripting workbench.

| Page | Path | Description |
|------|------|-------------|
| Dashboard | `/` | System health: uptime, record counts, shard chart |
| Telemetry | `/telemetry` | Semantic search over telemetry records |
| Logs | `/logs` | Semantic search over logs + LDA topic cloud |
| Documents | `/docs` | Knowledge-base document retrieval |
| Agg. Search | `/search` | Combined telemetry + document search |
| Trends | `/trends` | Statistical analysis and time-series chart |
| Bund | `/bund` | Interactive BUND scripting workbench |

---

## 4. JSON-RPC API (bdsnode)

Full index: [jsonrpc_api/README.md](jsonrpc_api/README.md)

All methods use JSON-RPC 2.0 over HTTP POST to `/` on the bdsnode port
(default 9000). Every request requires a `session` string parameter.

### Ingestion

| Method | Document | Description |
|--------|----------|-------------|
| `v2/add` | [v2_add.md](jsonrpc_api/v2_add.md) | Enqueue a single telemetry document |
| `v2/add.batch` | [v2_add_batch.md](jsonrpc_api/v2_add_batch.md) | Enqueue a batch of telemetry documents |
| `v2/add.file` | [v2_add_file.md](jsonrpc_api/v2_add_file.md) | Submit an NDJSON file path for background ingestion |
| `v2/add.file.syslog` | [v2_add_file_syslog.md](jsonrpc_api/v2_add_file_syslog.md) | Submit an RFC 3164 syslog file for background ingestion |

### Inventory

| Method | Document | Description |
|--------|----------|-------------|
| `v2/status` | [v2_status.md](jsonrpc_api/v2_status.md) | Node identity, uptime, hostname, ingest queue depths |
| `v2/count` | [v2_count.md](jsonrpc_api/v2_count.md) | Total record count across all shards |
| `v2/timeline` | [v2_timeline.md](jsonrpc_api/v2_timeline.md) | Oldest and newest event timestamps |
| `v2/shards` | [v2_shards.md](jsonrpc_api/v2_shards.md) | Per-shard record counts and metadata |

### Keys

| Method | Document | Description |
|--------|----------|-------------|
| `v2/keys` | [v2_keys.md](jsonrpc_api/v2_keys.md) | Known keys in the active shard |
| `v2/keys.all` | [v2_keys_all.md](jsonrpc_api/v2_keys_all.md) | Known keys across all shards for a duration |
| `v2/keys.get` | [v2_keys_get.md](jsonrpc_api/v2_keys_get.md) | Records for a specific key |

### Records

| Method | Document | Description |
|--------|----------|-------------|
| `v2/primaries` | [v2_primaries.md](jsonrpc_api/v2_primaries.md) | Primary records from the active shard |
| `v2/primaries.get` | [v2_primaries_get.md](jsonrpc_api/v2_primaries_get.md) | Primary records by key and duration |
| `v2/primaries.get.telemetry` | [v2_primaries_get_telemetry.md](jsonrpc_api/v2_primaries_get_telemetry.md) | Telemetry time-series data for a key |
| `v2/primaries.explore` | [v2_primaries_explore.md](jsonrpc_api/v2_primaries_explore.md) | Explore primary records with filters |
| `v2/primaries.explore.telemetry` | [v2_primaries_explore_telemetry.md](jsonrpc_api/v2_primaries_explore_telemetry.md) | Explore telemetry primaries with filters |
| `v2/primary` | [v2_primary.md](jsonrpc_api/v2_primary.md) | Fetch a single primary record by ID |
| `v2/secondaries` | [v2_secondaries.md](jsonrpc_api/v2_secondaries.md) | Secondary records from the active shard |
| `v2/secondary` | [v2_secondary.md](jsonrpc_api/v2_secondary.md) | Fetch a single secondary record by ID |
| `v2/duplicates` | [v2_duplicates.md](jsonrpc_api/v2_duplicates.md) | Deduplicated record report |

### Search

| Method | Document | Description |
|--------|----------|-------------|
| `v2/search` | [v2_search.md](jsonrpc_api/v2_search.md) | Semantic vector search (current shard) |
| `v2/search.get` | [v2_search_get.md](jsonrpc_api/v2_search_get.md) | Semantic vector search across all shards |
| `v2/fulltext` | [v2_fulltext.md](jsonrpc_api/v2_fulltext.md) | Full-text BM25 search (current shard) |
| `v2/fulltext.get` | [v2_fulltext_get.md](jsonrpc_api/v2_fulltext_get.md) | Full-text BM25 search across all shards |
| `v2/fulltext.recent` | [v2_fulltext_recent.md](jsonrpc_api/v2_fulltext_recent.md) | Full-text search limited to recent records |
| `v2/aggregationsearch` | [v2_aggregationsearch.md](jsonrpc_api/v2_aggregationsearch.md) | Combined telemetry + document semantic search |

### Analysis

| Method | Document | Description |
|--------|----------|-------------|
| `v2/trends` | [v2_trends.md](jsonrpc_api/v2_trends.md) | Statistical trend summary with anomaly and breakout detection |
| `v2/topics` | [v2_topics.md](jsonrpc_api/v2_topics.md) | LDA topic modelling over a key's corpus |
| `v2/topics.all` | [v2_topics_all.md](jsonrpc_api/v2_topics_all.md) | LDA topics across all shards for a duration |
| `v2/rca` | [v2_rca.md](jsonrpc_api/v2_rca.md) | Root cause analysis: co-occurrence clustering and causal ranking |

### Documents

| Method | Document | Description |
|--------|----------|-------------|
| `v2/doc.add` | [v2_doc_add.md](jsonrpc_api/v2_doc_add.md) | Add a document with metadata and content |
| `v2/doc.add.file` | [v2_doc_add_file.md](jsonrpc_api/v2_doc_add_file.md) | Add a document from a local file path |
| `v2/doc.get` | [v2_doc_get.md](jsonrpc_api/v2_doc_get.md) | Fetch a document (metadata + content) by ID |
| `v2/doc.get.metadata` | [v2_doc_get_metadata.md](jsonrpc_api/v2_doc_get_metadata.md) | Fetch document metadata by ID |
| `v2/doc.get.content` | [v2_doc_get_content.md](jsonrpc_api/v2_doc_get_content.md) | Fetch document content by ID |
| `v2/doc.update.metadata` | [v2_doc_update_metadata.md](jsonrpc_api/v2_doc_update_metadata.md) | Update document metadata |
| `v2/doc.update.content` | [v2_doc_update_content.md](jsonrpc_api/v2_doc_update_content.md) | Replace document content |
| `v2/doc.delete` | [v2_doc_delete.md](jsonrpc_api/v2_doc_delete.md) | Delete a document by ID |
| `v2/doc.search` | [v2_doc_search.md](jsonrpc_api/v2_doc_search.md) | Semantic search over documents |
| `v2/doc.search.strings` | [v2_doc_search_strings.md](jsonrpc_api/v2_doc_search_strings.md) | String similarity search over documents |
| `v2/doc.search.json` | [v2_doc_search_json.md](jsonrpc_api/v2_doc_search_json.md) | Structured JSON field search over documents |
| `v2/doc.reindex` | [v2_doc_reindex.md](jsonrpc_api/v2_doc_reindex.md) | Rebuild the HNSW vector index from all stored documents |

### BUND VM

| Method | Document | Description |
|--------|----------|-------------|
| `v2/eval` | [v2_eval.md](jsonrpc_api/v2_eval.md) | Evaluate a BUND script; returns the last workbench value |

---

## 5. BUND Scripting VM

BUND is a stack-based scripting language embedded in bdsnode and accessible
interactively via `bdsweb` or by script via `bdscmd` and `v2/eval`.

| Document | Description |
|----------|-------------|
| [Bund/README.md](Bund/README.md) | VM overview, integration guide, and context lifecycle |
| [Bund/SYNTAX_AND_VM.md](Bund/SYNTAX_AND_VM.md) | Language syntax, stack operations, and execution model |
| [Bund/BASIC_LIBRARY.md](Bund/BASIC_LIBRARY.md) | Built-in word reference: stack, arithmetic, string, list, map, I/O |

**Key concepts:**

- Values live on a stack. Operations consume and produce stack entries.
- `.` (dot) pops the top-of-stack and pushes it to the **workbench** — the
  result collection returned by `v2/eval`.
- Named contexts (`v2/eval` `context` parameter) persist VM state (defined
  words, stack contents) between calls. A fresh name gives a clean VM.
- Contexts are evicted after a configurable idle timeout (default 300 s).

---

## 6. Scripts

[SCRIPTS.md](SCRIPTS.md)

Operational shell scripts for data ingestion, node submission, and
end-to-end verification.

| Script | Description |
|--------|-------------|
| `fill-store.sh` | Populate a fresh node with telemetry, logs, and documents; rebuild vector indexes |
| `send_file_to_node.sh` | Generate NDJSON, submit via `v2/add.file`, monitor until complete |
| `send_logs_to_node.sh` | Generate mixed and log documents, submit as a single `v2/add.batch` |
| `send_syslog_to_node.sh` | Generate RFC 3164 syslog, submit via `v2/add.file.syslog`, verify with FTS |
| `verify_analysis.sh` | End-to-end LDA topic analysis test against bdscli |
| `verify_ingestion.sh` | End-to-end ingestion: record counts, primary/secondary split, dedup, vector index |
| `verify_logs.sh` | End-to-end log pipeline: ingestion, deduplication, FTS, and vector search |

---

## 7. Examples

[examples/README.md](examples/README.md)

Runnable examples covering the BUND VM tutorial series and the Rust API.

### BUND VM tutorial series

| Example | Description |
|---------|-------------|
| [01_hello_world](examples/01_hello_world.md) | Stack basics and `println` |
| [02_arithmetic](examples/02_arithmetic.md) | Numeric operations and operator precedence |
| [03_named_functions](examples/03_named_functions.md) | `register` / `alias` word definitions |
| [04_conditionals](examples/04_conditionals.md) | `if`, `ifthenelse`, `if.false` |
| [05_loops](examples/05_loops.md) | `while`, `times`, `for`, `map` |
| [06_lists](examples/06_lists.md) | List construction, `push`, `car`, `cdr`, sorting |
| [07_strings](examples/07_strings.md) | String builtins: case, regex, grok, tokenize |
| [08_maps_and_types](examples/08_maps_and_types.md) | Maps, type inspection, JSON encode/decode |
| [09_stack_and_workbench](examples/09_stack_and_workbench.md) | Stack manipulation and the `.` workbench operator |
| [10_full_program](examples/10_full_program.md) | Complete program combining all concepts |

### Rust API demos

| Example | Description |
|---------|-------------|
| [storage_engine_demo](examples/storage_engine_demo.md) | Low-level DuckDB SQL engine |
| [shard_demo](examples/shard_demo.md) | Single-shard three-index operations |
| [shardscache_demo](examples/shardscache_demo.md) | LRU shard cache and span queries |
| [shardsmanager_demo](examples/shardsmanager_demo.md) | Top-level API: bulk ingest, cross-shard search |
| [shardsmanager_documentstore](examples/shardsmanager_documentstore.md) | Document store via ShardsManager |
| [datastorage_demo](examples/datastorage_demo.md) | JSON key-value storage layer |
| [documentstorage_demo](examples/documentstorage_demo.md) | DocumentStorage: add, search, update |
| [embedding_engine_demo](examples/embedding_engine_demo.md) | fastembed vector generation |
| [fts_engine_demo](examples/fts_engine_demo.md) | Tantivy full-text indexing and BM25 search |
| [vectorengine_demo](examples/vectorengine_demo.md) | HNSW vector index operations |
| [observability_demo](examples/observability_demo.md) | ObservabilityStorage: dedup and secondaries |
| [generator_demo](examples/generator_demo.md) | Synthetic data generation |
| [globals_demo](examples/globals_demo.md) | Process-wide DB singleton (`init_db` / `get_db`) |
| [large_document_demo](examples/large_document_demo.md) | Chunked ingestion of large documents |
| [rca_demo](examples/rca_demo.md) | Root cause analysis with co-occurrence clustering |
| [telemetrytrend_demo](examples/telemetrytrend_demo.md) | Anomaly and breakout detection |
| [aggregationsearch_demo](examples/aggregationsearch_demo.md) | Combined telemetry + document search |

---

## 8. Tests

[tests/README.md](tests/README.md)

Integration test suite. Each test creates its own isolated database instance
(`:memory:` or `tempfile`) so tests run independently in parallel.

| Test file | Document | Description |
|-----------|----------|-------------|
| `storageengine_test.rs` | [storageengine_test.md](tests/storageengine_test.md) | SQL engine types, CRUD, 100-thread concurrency |
| `datastorage_test.rs` | [datastorage_test.md](tests/datastorage_test.md) | JSON key-value store operations |
| `documentstorage_test.rs` | [documentstorage_test.md](tests/documentstorage_test.md) | Document add, search, update, delete |
| `shard_test.rs` | [shard_test.md](tests/shard_test.md) | Three-index consistency, secondary isolation |
| `shardscache_test.rs` | [shardscache_test.md](tests/shardscache_test.md) | Time alignment, LRU cache, catalog persistence, span queries |
| `shardsinfo_test.rs` | [shardsinfo_test.md](tests/shardsinfo_test.md) | Shard metadata and info queries |
| `shardsmanager_test.rs` | [shardsmanager_test.md](tests/shardsmanager_test.md) | End-to-end ShardsManager: ingest, search, cross-shard |
| `shardsmanager_aggregationsearch_test.rs` | [shardsmanager_aggregationsearch_test.md](tests/shardsmanager_aggregationsearch_test.md) | Aggregated search across telemetry and documents |
| `vectorengine_test.rs` | [vectorengine_test.md](tests/vectorengine_test.md) | HNSW index: store, search, sync |
| `fts_test.rs` | [fts_test.md](tests/fts_test.md) | Tantivy index: ingest, BM25 queries, multi-shard |
| `embedding_test.rs` | [embedding_test.md](tests/embedding_test.md) | fastembed model loading and embedding generation |
| `observability_test.rs` | [observability_test.md](tests/observability_test.md) | redb dedup store and secondary tracking |
| `logparser_test.rs` | [logparser_test.md](tests/logparser_test.md) | Syslog, CLF, Apache, nginx, Python traceback parsing |
| `lda_test.rs` | [lda_test.md](tests/lda_test.md) | LDA topic analysis on log corpora |
| `rca_test.rs` | [rca_test.md](tests/rca_test.md) | Co-occurrence clustering and causal ranking |
| `telemetrytrend_test.rs` | [telemetrytrend_test.md](tests/telemetrytrend_test.md) | Anomaly detection and breakout analysis |
| `generator_test.rs` | [generator_test.md](tests/generator_test.md) | Synthetic data generator output validation |
| `globals_test.rs` | [globals_test.md](tests/globals_test.md) | Process-wide DB singleton initialization |
| `common_timerange_test.rs` | [common_timerange_test.md](tests/common_timerange_test.md) | Time range parsing and boundary logic |
| `common_uuid_test.rs` | [common_uuid_test.md](tests/common_uuid_test.md) | UUID generation and formatting utilities |

---

## 9. Reference Files

| File | Description |
|------|-------------|
| [COMMANDS.txt](COMMANDS.txt) | Quick-reference command cheat sheet |
| [CURL.txt](CURL.txt) | `curl` one-liners for common JSON-RPC calls |
