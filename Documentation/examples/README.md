# bdslib — Examples

Reference documentation for the files in the `examples/` directory. Each example is self-contained and runnable with `cargo run --example <name>`.

---

## BUND VM examples

Ten progressive tutorials for the BUND stack-based scripting VM. Run with:

```bash
bdscli bund examples/<name>.bund
```

| Example | File | Description |
|---|---|---|
| [Hello World](01_hello_world.md) | `01_hello_world.bund` | Push literals, print with `println` |
| [Arithmetic](02_arithmetic.md) | `02_arithmetic.bund` | Postfix math, `float.sqrt`, `float.Pi`, `*+` bulk sum |
| [Named Functions](03_named_functions.md) | `03_named_functions.bund` | `:name { body } register`, `alias`, recursion |
| [Conditionals](04_conditionals.md) | `04_conditionals.bund` | `if`, `if.false`, `ifthenelse`, boolean combinators |
| [Loops](05_loops.md) | `05_loops.bund` | `times`, `do`, `map`, `while`, `for`, fibonacci |
| [Lists](06_lists.md) | `06_lists.bund` | `car`/`cdr`, `head`/`tail`, `at`, `len`, `push`, `map`, recursive sum |
| [Strings](07_strings.md) | `07_strings.bund` | Case conversion, `wildmatch`, `regex`, `tokenize` |
| [Maps and Types](08_maps_and_types.md) | `08_maps_and_types.bund` | `set`/`get`/`has_key`, `type`, `convert.*` |
| [Stack and Workbench](09_stack_and_workbench.md) | `09_stack_and_workbench.bund` | Workbench (`.`), named stacks (`@name`), function pointers |
| [Full Program](10_full_program.md) | `10_full_program.bund` | Statistics tool combining all BUND features |

---

## Rust API examples

Run with:

```bash
cargo run --example <name>
```

### Storage layer

| Example | File | Description |
|---|---|---|
| [StorageEngine](storage_engine_demo.md) | `storage_engine_demo.rs` | Low-level DuckDB SQL engine with R2D2 pool and rust_dynamic type bridge |
| [DataStorage](datastorage_demo.md) | `datastorage_demo.rs` | `BlobStorage` and `JsonStorage` with key-based deduplication |
| [ObservabilityStorage](observability_demo.md) | `observability_demo.rs` | redb-backed dedup, primary/secondary classification, time-range queries |

### Search engines

| Example | File | Description |
|---|---|---|
| [EmbeddingEngine](embedding_engine_demo.md) | `embedding_engine_demo.rs` | fastembed vector embeddings, cosine similarity, nearest-neighbour |
| [FTSEngine](fts_engine_demo.md) | `fts_engine_demo.rs` | Tantivy BM25 full-text search: add, query, drop, sync |
| [VectorEngine](vectorengine_demo.md) | `vectorengine_demo.rs` | HNSW vector storage, reranking (MMR, custom), JSON fingerprinting |

### Shard management

| Example | File | Description |
|---|---|---|
| [Shard](shard_demo.md) | `shard_demo.rs` | Single time-partition: telemetry table, FTS, vector search, delete |
| [ShardsCache](shardscache_demo.md) | `shardscache_demo.rs` | LRU shard cache, time-aligned buckets, cross-shard span queries |
| [ShardsManager](shardsmanager_demo.md) | `shardsmanager_demo.rs` | Config-driven top-level API: bulk ingest, cross-shard FTS and vector |

### Analytics

| Example | File | Description |
|---|---|---|
| [TelemetryTrend](telemetrytrend_demo.md) | `telemetrytrend_demo.rs` | Statistics, S-H-ESD anomaly detection, breakout detection |
| [RCA](rca_demo.md) | `rca_demo.rs` | Co-occurrence clustering and causal ranking for root cause analysis |

### Data generation and globals

| Example | File | Description |
|---|---|---|
| [Generator](generator_demo.md) | `generator_demo.rs` | Synthetic telemetry, logs, mixed, and template-driven documents |
| [Globals](globals_demo.md) | `globals_demo.md` | Process-wide `ShardsManager` singleton: `init_db`, `get_db`, `sync_db` |
