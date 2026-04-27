# bdslib — Test Suite

Reference documentation for the files in the `tests/` directory. Each document describes the test functions, scenarios covered, and key invariants verified by that file.

Run all tests:

```bash
make test
# or
cargo test -- --show-output
```

Run a single test:

```bash
cargo test <test_function_name> -- --show-output
```

---

## Common utilities

| Document | File | Description |
|---|---|---|
| [common_timerange_test](common_timerange_test.md) | `tests/common_timerange_test.rs` | Time range alignment: `minute_range`, `hour_range`, `day_range` — boundary alignment, nesting, contiguity |
| [common_uuid_test](common_uuid_test.md) | `tests/common_uuid_test.rs` | UUIDv7 generation: monotonicity, uniqueness, timestamp round-trip, ordering |

## Storage layer

| Document | File | Description |
|---|---|---|
| [storageengine_test](storageengine_test.md) | `tests/storageengine_test.rs` | Low-level DuckDB SQL engine: CRUD, all DuckDB types, sync, 100-thread concurrency |
| [datastorage_test](datastorage_test.md) | `tests/datastorage_test.rs` | `BlobStorage` and `JsonStorage`: CRUD, key-based dedup, nested paths, SQL safety |
| [documentstorage_test](documentstorage_test.md) | `tests/documentstorage_test.rs` | `DocumentStorage`: combined metadata/blob/vector store — add, get, update, delete, unified vector search, `results_to_strings`, persistence |
| [observability_test](observability_test.md) | `tests/observability_test.rs` | `ObservabilityStorage`: dedup, primary/secondary split, time-range queries, metadata |

## Search engines

| Document | File | Description |
|---|---|---|
| [embedding_test](embedding_test.md) | `tests/embedding_test.rs` | `EmbeddingEngine`: cosine similarity math, 384-dimensional AllMiniLML6V2 output, semantic consistency, thread safety |
| [fts_test](fts_test.md) | `tests/fts_test.rs` | `FTSEngine`: Tantivy add/search/drop/sync, BM25 ranking, limit, concurrency |
| [vectorengine_test](vectorengine_test.md) | `tests/vectorengine_test.rs` | `VectorEngine`: HNSW store/search, reranking (MMR/custom), JSON fingerprinting, concurrency |

## Shard management

| Document | File | Description |
|---|---|---|
| [shardsinfo_test](shardsinfo_test.md) | `tests/shardsinfo_test.rs` | `ShardInfoEngine` catalog: add/query by timestamp, half-open interval semantics, concurrent writes |
| [shard_test](shard_test.md) | `tests/shard_test.rs` | `Shard`: FTS + vector + observability consistency, secondary isolation, embedded secondaries in results |
| [shardscache_test](shardscache_test.md) | `tests/shardscache_test.rs` | `ShardsCache`: auto-creation, interval alignment, LRU cache, catalog persistence, span queries |
| [shardsmanager_test](shardsmanager_test.md) | `tests/shardsmanager_test.rs` | `ShardsManager`: config loading, timestamp routing, cross-shard FTS/vector, cross-shard update |
| [shardsmanager_aggregationsearch_test](shardsmanager_aggregationsearch_test.md) | `tests/shardsmanager_aggregationsearch_test.rs` | `ShardsManager::aggregationsearch`: result structure, empty store, telemetry hits with `_score`, document hits with metadata/content, combined population, duration error propagation |

## Data generation and parsing

| Document | File | Description |
|---|---|---|
| [generator_test](generator_test.md) | `tests/generator_test.rs` | `Generator`: telemetry, log, mixed, and templated document generation; placeholder types; time window |
| [logparser_test](logparser_test.md) | `tests/logparser_test.rs` | Log parsing: syslog, CLF, Apache, nginx, Python tracebacks; validation; grok; file ingestion |

## Analytics

| Document | File | Description |
|---|---|---|
| [telemetrytrend_test](telemetrytrend_test.md) | `tests/telemetrytrend_test.rs` | `TelemetryTrend`: statistics, S-H-ESD anomaly detection, breakout detection, generator integration |
| [lda_test](lda_test.md) | `tests/lda_test.rs` | LDA topic modelling: corpus analysis, keyword invariants, k clamping, empty/numeric corpora |
| [rca_test](rca_test.md) | `tests/rca_test.rs` | RCA: co-occurrence clustering, causal ranking, telemetry exclusion, threshold and bucket effects |

## Global singleton

| Document | File | Description |
|---|---|---|
| [globals_test](globals_test.md) | `tests/globals_test.rs` | `init_db` / `get_db` / `sync_db`: initialization lifecycle, double-init guard, config resolution |

---

## Notes on singleton tests

Several tests (`globals_test`, `lda_test`, `rca_test`, `telemetrytrend_test`) wrap all sub-scenarios in a **single `#[test]` function**. This is intentional: the process-wide `ShardsManager` `OnceLock` cannot be reset between tests, so sequential execution within one function is required to prevent initialization races when tests run in parallel.
