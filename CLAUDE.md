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

The library exposes a layered set of storage primitives, all built on `StorageEngine`.

### Foundation — `StorageEngine` (`src/storageengine.rs`)

Wraps a `duckdb::r2d2::Pool` (configurable size). `Clone`-able and thread-safe via `Arc`.

Constructor:
```rust
StorageEngine::new(path: &str, init_sql: &str, pool_size: u32) -> Result<StorageEngine>
```
`path` is a filesystem path or `":memory:"`. `init_sql` is executed once to initialize the schema.

Core methods:
- `select_all(sql)` → `Vec<Vec<rust_dynamic::value::Value>>` — collect all rows
- `select_foreach(sql, callback)` — stream rows via callback
- `execute(sql)` — DML (INSERT/UPDATE/DELETE)
- `execute_many(stmts)` — batch inside a single `BEGIN … COMMIT`
- `sync()` — DuckDB CHECKPOINT (flush WAL to disk)

**Type bridge**: `row_to_dynamic()` maps DuckDB types to `rust_dynamic::value::Value`. Use `.cast_int()` for BIGINT, `.cast_string()` for TEXT, `.cast_bin()` for BLOB.

**Error handling**: All methods return `Result<T>` (`crate::common::error::Result`).

### Primitive stores (`src/datastorage.rs`)

- **`BlobStorage`** — keyed binary blob store (UUID or string key).
- **`JsonStorage`** — keyed JSON document store with optional logical-key deduplication.

### Frequency tracking (`src/frequencytrackingstorage.rs`)

**`FrequencyTracking`** records `(timestamp, id)` observation pairs, allowing event-rate analysis over time. Duplicate observations at the same second are stored separately.

```rust
FrequencyTracking::new(path, pool_size) -> Result<FrequencyTracking>
```

Key methods:
| Method | Description |
|---|---|
| `add(id)` | Record `id` at wall-clock now |
| `add_with_timestamp(ts, id)` | Record `id` at explicit Unix-second `ts` |
| `by_id(id)` | All timestamps (ascending) for `id` |
| `by_timestamp(ts)` | Distinct IDs observed at exact second `ts` |
| `time_range(start, end)` | Distinct IDs in inclusive `[start, end]` |
| `recent(duration)` | Distinct IDs in `[now−duration, now]`; duration is a humantime string like `"1h"` |
| `sync()` | DuckDB CHECKPOINT |

Tests: `tests/frequencytrackingstorage_test.rs` (25 tests).
Demo: `examples/frequencytracking_demo.rs`.

### Document store (`src/documentstorage.rs`)

**`DocumentStorage`** combines JSON metadata (`JsonStorage`), raw content (`BlobStorage`), and a vector index. Auto-embeds via `EmbeddingEngine`.

### Sharded telemetry (`src/shard.rs`, `src/shardscache.rs`, `src/shardsmanager.rs`)

- **`Shard`** — time-partitioned unit: observability table + FTS + vector index + `tplstorage` (template store).
- **`ShardsCache`** — manages multiple `Shard` instances keyed by `[start, end)` intervals.
- **`ShardsManager`** — high-level API; routes records by `"timestamp"` field; driven by an hjson config file.

Config keys: `dbpath`, `shard_duration`, `pool_size`, `similarity_threshold`, `drain_enabled`, `drain_load_duration`.

### Drain3 log-template mining (`src/common/drain.rs`)

**`DrainParser`** — prefix-tree log template miner. Default: `depth=3`, `sim_threshold=0.5`, `max_children=100`.

Key methods: `parse(line)` → `ParseResult<'_>`, `parse_json(doc)` → `ParseJsonResult` (global DB), `parse_json_with_callback(doc, fn)` (explicit store), `load_templates(duration)` (global DB), `from_tpl_list(entries)` (pre-fetched list), `seed_cluster(tokens)` (direct injection).

`ShardsManager::drain_parse_json(parser, doc)` and `ShardsManager::drain_load(duration)` are the instance-scoped equivalents.

### Cluster-aware Bund VM API (`src/vm/api`, `src/vm/stdlib/cluster`)

Bund scripts get a transparent cluster-aware API that uses `rust_dynamic::value::Value` everywhere and detects standalone vs cluster mode at the call site.

**Helpers** — `src/vm/api/` — one fn per logical operation (`add`, `search_vector`, `anomaly_recent`, `signal_emit`, `doc_add`, …) under area modules (`add`, `search`, `analysis`, `signals`, `documents`, `templates`, `scripts`, `keys`, `primaries`).  Every helper:
1. Looks up the global DB via `bdslib::get_db()`.
2. Converts Value inputs to JSON via `vm::helpers::eval::dynamic_to_json`.
3. Routes through `vm::api::dispatch::{read, write_replicated, write_sharded, write_local}`:
   - Standalone → local DB call only.
   - Cluster reads → local call + `cluster::fanout::fan_out_v2` to every Alive peer, merged via `cluster::merge` (the same module bdsnode's `v3_*.rs` handlers use).
   - Cluster writes → local commit + replication via `cluster::replication::{replicate_to_all, pick_random_alive}` with hint-on-failure.
4. Stashes the per-call `cluster_meta` on a per-thread cell (`vm::api::meta::set`) so Bund scripts can introspect via `?cluster.meta`.
5. Returns the result as a `rust_dynamic::value::Value`.

Async-from-sync bridge: `vm::api::runtime::block_on` uses `tokio::task::block_in_place` + `Handle::current().block_on` when an ambient tokio runtime exists (bdsnode/bdsweb), and falls back to a `OnceLock<Runtime>` for bdscmd.

**Bund words** — `src/vm/stdlib/cluster/` — ~70 helpers wired as `cls.*` words, each with stack and workbench variants:

| Family            | Words (selection)                                                     |
|-------------------|-----------------------------------------------------------------------|
| `cls.add`         | `cls.add`, `cls.add.batch`, `cls.update`, `cls.delete`, `cls.count`, `cls.duplicates`, `cls.fingerprints.recent` |
| `cls.search`      | `cls.search`, `cls.search.get`, `cls.search.fts`, `cls.aggregation`, `cls.fulltext`, `cls.fulltext.recent`, `cls.fulltext.get` |
| `cls.analysis`    | `cls.anomaly.recent`, `cls.denoise.recent`, `cls.knn`, `cls.rca`, `cls.rca.templates`, `cls.topics`, `cls.topics.all`, `cls.trends`, `cls.summary.{recent,query,lsa.recent,lsa.query}`, `cls.textrank.templates`, `cls.timeline` |
| `cls.signal`      | `cls.signal.{emit,update,get}`, `cls.signals.{recent,query}`          |
| `cls.doc`         | `cls.doc.{add,add.file,update.metadata,update.content,delete,get.metadata,get.content,search,search.strings,search.json,search.json.strings,reindex,sync}` |
| `cls.tpl`         | `cls.tpl.{add,update.metadata,update.body,delete,reindex,get,list,search,template.by.id,templates.recent,templates.by.timestamp}` |
| `cls.script`      | `cls.script.{add,update,delete,get}`, `cls.scripts.list`              |
| `cls.keys`        | `cls.keys`, `cls.keys.all`, `cls.keys.get`                            |
| `cls.primaries`   | `cls.primaries`, `cls.primaries.{explore,explore.telemetry,get,get.telemetry}`, `cls.secondaries`, `cls.primary`, `cls.secondary` |
| meta              | `?cluster.meta` (stack) and `?cluster.meta.` (workbench)              |

The existing `db.*` / `doc.*` words are untouched — scripts opt into cluster awareness by using `cls.*` instead.  See `examples/cluster/` for a tour.

### Authentication (`src/cluster/{user_store,credential,session}`, `src/bin/bdsweb/auth.rs`)

A 4th fully-replicated cluster store backs bdsweb's session-cookie auth.

**Library**:
- `cluster::user_store::UserStorage` — DuckDB at `<dbpath>/users/users.duckdb`. Columns: `id, username, credential_hash, auth_method, metadata, created_at, updated_at, disabled`. `UserSummary` projection drops `credential_hash` so admin listings can't leak hashes.
- `cluster::credential::{AuthMethod, CredentialVerifier, PasswordVerifier, VerifierRegistry}` — pluggable verifier dispatch. `PasswordVerifier` uses argon2id (`m=19 MiB, t=2, p=1`). OAuth/LDAP register a new `CredentialVerifier` impl at startup with no schema change.
- `cluster::session::{issue_session_token, verify_session_token}` — stateless tokens of form `<user_id>.<expires_at>.<hex_hmac_sha256>` signed with `cluster.shared_secret`. Single algorithm hard-coded (HMAC-SHA256) — no JWT `alg=none` confusion. `SessionError` enum with distinct variants for precise logging.

**JSON-RPC**:
- `v3/user.{add, modify, delete, authenticate, list}` — coordinator surface. All HMAC-protected EXCEPT `authenticate` (public login) and `add` on an empty store (first-user bootstrap).
- `v2/user.{add, modify, delete, get_by_username, get_by_id, list_ids}` — receivers; called by `replicate_to_all`, hint replay, and anti-entropy.
- Anti-entropy (`bdsnode/server/cluster.rs`) sweeps `users` alongside `docs/signals/scripts`. `pull_one` uses `UserStorage::add_with_hash` to copy a peer's exact hash without re-hashing (so two nodes can converge even if their argon2 setups differ).

**bdsweb**:
- `auth::require_session` axum middleware. Three short-circuits before cookie verification: open-access mode (no secret), public allow-list (`/login`, `/logout`, `/version`), first-user bootstrap window (cached `v3/user.list` empty for 30 s).
- `routes::login` handles `/login` (GET+POST) and `/logout`. POST `/login` exchanges username+password for a `bds_session` cookie (`HttpOnly; SameSite=Lax; Max-Age=session_ttl`).
- `routes::admin_users` powers the Administration → User management page at `/admin/users`. Add / reset password / disable / enable / delete forms, all HMAC-signed via `admin::signed_rpc`.

**bdscmd**: `bdscmd user` subcommand group with `add`/`modify`/`delete`/`list`/`authenticate`/`whoami` (the last is fully offline — verifies the session token's HMAC locally).

### English → Bund translator (`src/llm/to_bund.rs`, `src/llm/to_bund_prompt.rs`)

`v2/to.bund` — LLM-based natural-language → Bund translator built
on the same `Provider` abstraction the `v4/llm.*` surface uses,
but stripped down: no HMAC, no cluster fan-out, no inference cache.

**Library**:
- `llm::to_bund::translate(message, req_extra)` returns `Translation { script, valid, parse_attempts, parse_error, provider, model, ms, tokens_in?, tokens_out? }`.  Loops `0..=max_retries`: extract `bund` fence → `bund_parse` → undefined-word dry-run against `vm::registered_word_names()`.  On any failure, appends the bad assistant turn + a user turn carrying the validation error, then re-prompts.
- `llm::to_bund_prompt::assemble_system_prompt_with_policy(extra, disabled_groups)` joins ROLE / LANG_PRIMER / TYPE_SYSTEM / STDLIB_CATALOGUE / OUTPUT_CONTRACT / DISABLED_WORDS (when policy is non-empty) / operator extras / FEW_SHOT_EXAMPLES.  `baked_prompt_len()` for telemetry.
- `ToBundSettings { enabled, timeout_secs, max_retries, provider, model, extra_system_prompt }` lives in a process-wide `OnceLock`; initialised by bdsnode main from the `llm.to_bund.*` hjson block (`manager::ToBundConfig`).

**Sandbox-policy bridge**:
- `vm::policy::effective_disabled_by_category()` groups the active policy's blocked words by category for the prompt splice.
- `vm::policy::effective_disabled_words()` is the flat enumeration.
- The disabled words are still registered in Adam (as denied stubs) so the undefined-word dry-run lets them through; the prompt is the only line of defence at translation time.

**Adam introspection**:
- `vm::registered_word_names() -> BTreeSet<String>` snapshots every key in Adam's `inline_fun`/`command_fun`/`methods_fun`/`lambdas`/`classes`/`name_mapping` maps, stripping the internal `_inline` suffix.  Returns an empty set when `init_adam` hasn't run — the dry-run degrades to a no-op in that case.

**Consumers**:
- JSON-RPC: `v2/to.bund` (translate) + `v2/to.bund.settings` (echo active config + `disabled_groups`).  Both in `src/bin/bdsnode/jsonrpc/v2_to_bund.rs`.
- CLI: `bdscmd to-bund` (`src/bin/bdscmd/cmd/to_bund.rs`).  Default returns the full Translation JSON; `--script-only` prints just the script to stdout (one-line summary on stderr) and exits non-zero on `valid=false` — designed for piping into `bdscmd eval -`.
- Web: bdsweb `/bund` page has a collapsible *Translate from English* panel.  HTMX form posts to `/bund/translate` (`src/bin/bdsweb/routes/bund.rs::translate`), partial template at `templates/partials/bund_translate.html`.  *Use as script* button drops the result into the CodeMirror editor via a hidden `#translate-script-payload` textarea + `window.useTranslatedScript()`.

**Tests**:
- 11 unit tests in `src/llm/to_bund.rs` (extract / parse_options / translation_to_json / undefined_words / format_unknown_words_error).
- 4 unit tests in `src/llm/to_bund_prompt.rs` (length sanity, extra splicing, disabled-groups rendering, essentials present).
- 6 integration tests in `tests/llm_to_bund_test.rs` using a `ScriptedProvider` impl (no HTTP) — round-trip success, parse-error retry, retries exhausted, per-call provider override, undefined-word retry, undefined-word retries exhausted.

## Integration Tests

Tests live in `tests/storageengine_test.rs`. Each test creates its own DuckDB instance (`:memory:` or `tempfile`):
- `test_storage_engine_full_lifecycle` — basic CRUD
- `test_concurrent_access` — 100-thread Rayon parallel stress test
- `test_type_conversions` — BLOB/binary handling

Cluster-aware standalone smoke at `tests/vm_api_smoke.rs` exercises `vm::api::*` end-to-end against a freshly initialised global DB.

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
