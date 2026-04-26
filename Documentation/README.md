# bdslib — Documentation

Reference documentation for bdslib, bdscli, bdsnode, and the BUND scripting VM.

---

## API & engines

| Document | Description |
|---|---|
| [STORAGEENGINE.md](STORAGEENGINE.md) | `StorageEngine` — DuckDB-backed core storage with R2D2 connection pool |
| [SHARDSMANAGER.md](SHARDSMANAGER.md) | `ShardsManager` — time-partitioned shard lifecycle, ingestion, and querying |
| [SHARDSCACHE.md](SHARDSCACHE.md) | `ShardsCache` — LRU shard handle cache and open-shard pool |
| [SHARD.md](SHARD.md) | `Shard` — single time-partition: telemetry table, FTS index, vector index |
| [EMBEDDINGENGINE.md](EMBEDDINGENGINE.md) | `EmbeddingEngine` — fastembed-based vector embedding generation |
| [FTSENGINE.md](FTSENGINE.md) | `FTSEngine` — Tantivy full-text search index management |
| [VECTORENGINE.md](VECTORENGINE.md) | `VectorEngine` — HNSW vector index backed by VecStore |
| [DOCUMENTSENGINE.md](DOCUMENTSENGINE.md) | `DocumentStorage` — combined metadata, blob, and vector store with unified similarity search |
| [OBSERVABILITYENGINE.md](OBSERVABILITYENGINE.md) | `ObservabilityStorage` — redb key-value store for dedup tracking and secondaries |
| [COMMON.md](COMMON.md) | Common utilities: error handling, JSON fingerprint, math, time ranges, UUID |

## CLI

| Document | Description |
|---|---|
| [BDSCLI.md](BDSCLI.md) | `bdscli` — full command reference: init, generate, ingest, get, search, analyze |
| [BDSCMD.md](BDSCMD.md) | `bdscmd` — full JSON-RPC client reference: all 30 `v2/*` methods, eval shebang, quick reference |

## JSON-RPC API (bdsnode)

| Document | Description |
|---|---|
| [jsonrpc_api/README.md](jsonrpc_api/README.md) | Index of all `v2/*` JSON-RPC 2.0 methods with protocol notes and error codes |

Key methods:

| Method | Description |
|---|---|
| [`v2/status`](jsonrpc_api/v2_status.md) | Live process snapshot: node identity, uptime, hostname, queue depths |
| [`v2/add`](jsonrpc_api/v2_add.md) | Enqueue a single telemetry document |
| [`v2/add.batch`](jsonrpc_api/v2_add_batch.md) | Enqueue a batch of telemetry documents |
| [`v2/add.file`](jsonrpc_api/v2_add_file.md) | Submit an NDJSON file path for background ingestion |
| [`v2/add.file.syslog`](jsonrpc_api/v2_add_file_syslog.md) | Submit an RFC 3164 syslog file path for background ingestion |
| [`v2/trends`](jsonrpc_api/v2_trends.md) | Statistical trend summary with anomaly and breakout detection |
| [`v2/topics`](jsonrpc_api/v2_topics.md) | LDA topic modelling over a key's corpus |
| [`v2/rca`](jsonrpc_api/v2_rca.md) | Root cause analysis: co-occurrence clustering and causal ranking |
| [`v2/search`](jsonrpc_api/v2_search.md) | Semantic vector search |
| [`v2/fulltext`](jsonrpc_api/v2_fulltext.md) | Full-text BM25 search |

See [jsonrpc_api/README.md](jsonrpc_api/README.md) for the complete method list and the `bdscmd` client reference.

## BUND scripting VM

| Document | Description |
|---|---|
| [Bund/README.md](Bund/README.md) | BUND VM overview and integration guide |
| [Bund/SYNTAX_AND_VM.md](Bund/SYNTAX_AND_VM.md) | Language syntax, stack operations, and VM execution model |
| [Bund/BASIC_LIBRARY.md](Bund/BASIC_LIBRARY.md) | Built-in word reference: stack, arithmetic, string, list, and I/O words |

## Scripts

| Document | Description |
|---|---|
| [SCRIPTS.md](SCRIPTS.md) | Operational shell scripts: data ingestion, node submission, and end-to-end verification |

Key scripts:

| Script | Description |
|---|---|
| `send_file_to_node.sh` | Generate an NDJSON file, submit via `v2/add.file`, monitor `v2/status` until complete, then remove the file |
| `send_logs_to_node.sh` | Generate mixed + log documents in memory and submit as a single `v2/add.batch` |
| `send_syslog_to_node.sh` | Generate an RFC 3164 syslog file, submit via `v2/add.file.syslog`, monitor `v2/status`, verify with `v2/fulltext*` |
| `verify_analysis.sh` | End-to-end LDA topic analysis test against bdscli and a fresh database |
| `verify_ingestion.sh` | End-to-end ingestion test: record counts, primary/secondary split, dedup, vector index |
| `verify_logs.sh` | End-to-end log pipeline test: ingestion, deduplication, FTS, and vector search |

## Examples

| Document | Description |
|---|---|
| [examples/README.md](examples/README.md) | Index of all runnable examples: 10 BUND VM tutorials and 13 Rust API demos |

Key examples:

| Example | Description |
|---|---|
| [`01_hello_world.bund`](examples/01_hello_world.md) – [`10_full_program.bund`](examples/10_full_program.md) | Progressive BUND VM tutorial series |
| [`storage_engine_demo.rs`](examples/storage_engine_demo.md) | Low-level DuckDB SQL engine |
| [`shardsmanager_demo.rs`](examples/shardsmanager_demo.md) | Top-level config-driven API: bulk ingest, cross-shard search |
| [`rca_demo.rs`](examples/rca_demo.md) | Root cause analysis with co-occurrence clustering |
| [`telemetrytrend_demo.rs`](examples/telemetrytrend_demo.md) | Anomaly and breakout detection |

## Tests

| Document | Description |
|---|---|
| [tests/README.md](tests/README.md) | Index of all 18 test files with per-file descriptions |

Key test files:

| Test file | Description |
|---|---|
| [`storageengine_test.rs`](tests/storageengine_test.md) | SQL engine types, CRUD, 100-thread concurrency |
| [`shard_test.rs`](tests/shard_test.md) | Three-index consistency, secondary isolation, embedded secondaries |
| [`shardscache_test.rs`](tests/shardscache_test.md) | Time alignment, LRU cache, catalog persistence, span queries |
| [`logparser_test.rs`](tests/logparser_test.md) | Syslog, CLF, Apache, nginx, Python traceback parsing |
| [`rca_test.rs`](tests/rca_test.md) | Co-occurrence clustering, causal ranking, telemetry exclusion |

## Reference files

| File | Description |
|---|---|
| [COMMANDS.txt](COMMANDS.txt) | Quick-reference command cheatsheet |
| [CURL.txt](CURL.txt) | curl one-liners for common JSON-RPC calls |
