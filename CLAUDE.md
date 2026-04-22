# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**bdslib** is a Rust library (Edition 2024) providing multifunctional programmatic data storage. It wraps DuckDB with a connection pool and a dynamic type layer, with a large dependency set spanning analytics, full-text search, vector embeddings, NLP, time series forecasting, and media processing.

## Commands

```bash
make all        # cargo build
make rebuild    # clean + build
make test       # cargo test -- --show-output
make clean      # clean artifacts and update deps
```

Run a single test:
```bash
cargo test test_storage_engine_full_lifecycle -- --show-output
```

## Architecture

The entire public API is a single struct: `StorageEngine` in `src/storageengine.rs`, re-exported from `src/lib.rs`.

**`StorageEngine`** wraps a `duckdb::r2d2::Pool` (max 16 connections). It is `Clone`-able and thread-safe via `Arc`.

Constructor:
```rust
StorageEngine::new(path: &str, init_sql: &str) -> EngineResult<StorageEngine>
```
`path` is a filesystem path or `":memory:"`. `init_sql` is executed once to initialize the schema.

Core methods:
- `select_all(sql)` → `Vec<Vec<rust_dynamic::value::Value>>` — collect all rows
- `select_foreach(sql, callback)` — stream rows via callback (avoids allocating full result set)
- `execute(sql)` — DML (INSERT/UPDATE/DELETE)
- `sync()` — DuckDB CHECKPOINT (flush WAL to disk)

**Type bridge**: `row_to_dynamic()` converts DuckDB column types to `rust_dynamic::value::Value` (Boolean, Int, BigInt, Float, Double, Text, Blob, Null). All query results are returned as this dynamic value type.

**Error handling**: All methods return `EngineResult<T>` which is `Result<T, easy_error::Error>`.

## Integration Tests

Tests live in `tests/storageengine_test.rs`. Each test creates its own DuckDB instance (`:memory:` or `tempfile`):
- `test_storage_engine_full_lifecycle` — basic CRUD
- `test_concurrent_access` — 100-thread Rayon parallel stress test
- `test_type_conversions` — BLOB/binary handling

## Key Dependencies

| Crate | Purpose |
|---|---|
| `duckdb` | SQL engine with R2D2 pooling |
| `rust_dynamic` | Polymorphic value type used throughout |
| `redb` | Embedded key-value store |
| `tantivy` | Full-text search |
| `vecstore` | Vector storage |
| `fastembed` | Vector embeddings |
| `augurs` | Time series (ETS, MSTL, outlier detection, DTW, clustering) |
| `rayon` | Data parallelism |
| `ndarray` | Numerical arrays |
| `serde` + `bincode`/`serde_json`/`serde_cbor`/`rmp-serde` | Multi-format serialization |
