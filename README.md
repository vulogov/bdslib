![bdslib logo](logo.jpeg)

# bdslib — BUND Data Storage

A Rust library (Edition 2024) for multifunctional programmatic data storage.
bdslib combines time-series telemetry, full-text and semantic search, log
analysis, a document knowledge base, statistical trend analysis, and a
stack-based scripting runtime into a single cohesive system.

---

## Capabilities

- **Time-series storage** — DuckDB shards partitioned by time window, with R2D2 connection pooling and an LRU shard cache
- **Semantic search** — fastembed vector embeddings stored in per-shard HNSW indexes (VecStore)
- **Full-text search** — Tantivy BM25 index per shard
- **Log ingestion & analysis** — RFC 3164 syslog parser, deduplication via redb, LDA topic modelling
- **Document knowledge base** — metadata + blob storage with per-document vector indexing and similarity search
- **Statistical analysis** — trend detection, anomaly identification, breakout detection, root cause analysis
- **BUND scripting VM** — embedded stack-based language accessible locally and over the network

---

## Components

| Binary | Description |
|--------|-------------|
| `bdsnode` | JSON-RPC 2.0 server exposing the full bdslib API on port 9000 |
| `bdscli` | Local CLI operating directly on a DuckDB database file |
| `bdscmd` | Command-line JSON-RPC client — one subcommand per `v2/*` method |
| `bdsweb` | Dark-themed web UI with seven analytical pages |

---

## Build

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

---

## Quick Start

**1. Start the server**

```bash
bdsnode --config config.hjson
```

**2. Check it is running**

```bash
bdscmd status
```

**3. Ingest data**

```bash
bdscmd add --key cpu.usage --data '{"value": 0.72}'
bdscmd add-file /path/to/records.ndjson
```

**4. Search**

```bash
bdscmd search-get -q "high cpu memory pressure" --duration 1h
```

**5. Open the web UI**

```bash
bdsweb --node http://127.0.0.1:9000
# → http://127.0.0.1:8080
```

---

## Documentation

Full documentation lives in [`Documentation/`](Documentation/README.md).

| Document | Description |
|----------|-------------|
| [Documentation/README.md](Documentation/README.md) | Project overview, architecture, and full documentation index |
| [Documentation/BDSCLI.md](Documentation/BDSCLI.md) | `bdscli` — local CLI reference |
| [Documentation/BDSCMD.md](Documentation/BDSCMD.md) | `bdscmd` — JSON-RPC client reference |
| [Documentation/BDSWEB.md](Documentation/BDSWEB.md) | `bdsweb` — web interface reference |
| [Documentation/jsonrpc_api/README.md](Documentation/jsonrpc_api/README.md) | All `v2/*` JSON-RPC methods |
| [Documentation/Bund/README.md](Documentation/Bund/README.md) | BUND scripting VM overview |
| [Documentation/Bund/SYNTAX_AND_VM.md](Documentation/Bund/SYNTAX_AND_VM.md) | BUND language syntax and execution model |
| [Documentation/Bund/BASIC_LIBRARY.md](Documentation/Bund/BASIC_LIBRARY.md) | BUND built-in word reference |
| [Documentation/SCRIPTS.md](Documentation/SCRIPTS.md) | Operational shell scripts |
| [Documentation/examples/README.md](Documentation/examples/README.md) | Runnable examples |
| [Documentation/tests/README.md](Documentation/tests/README.md) | Integration test descriptions |

---

## License

See [LICENSE](LICENSE).
