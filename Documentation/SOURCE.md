# `source` — the data-origin axis

Every observability record stored through any of bdslib's ingest paths
(`v2/add`, `v2/add.batch`, `v2/add.file`, `v2/add.file.syslog`, the
Rust `ShardsManager::add*` API, or replicated v3 fan-outs) carries a
canonical `source` tag.  `source` answers *where did this record come
from* — a hostname, container id, application name, syslog shipper,
or logical pipeline — and is the join key the signal layer, the graph
layer, and the LLM analyze surfaces use to align records with the
broader system they describe.

This document covers:

1. [What `source` is](#1-what-source-is)
2. [The resolution chain](#2-the-resolution-chain)
3. [Storage layout](#3-storage-layout)
4. [Configuration](#4-configuration)
5. [API surface — Rust, RPC, CLI](#5-api-surface--rust-rpc-cli)
6. [Graph alignment](#6-graph-alignment)
7. [LLM alignment](#7-llm-alignment)
8. [Backwards compatibility](#8-backwards-compatibility)
9. [Edge cases & validation](#9-edge-cases--validation)
10. [Source code map](#10-source-code-map)

---

## 1. What `source` is

> `source` is a non-empty UTF-8 string (≤ 256 bytes by default)
> identifying *where a record originated*.  Default `"global"` when
> no signal is available.

Every primary record stored through bdslib gets exactly one source
value resolved at ingest time and parked in the `metadata.source`
field of the stored record.  Sources are:

- **First-class for queries** — operators can filter every read by
  source.
- **First-class for the graph** — the first observation of a new
  source automatically creates a `Source:<name>` graph node with a
  deterministic UUIDv5 id (so every cluster peer converges on the
  same node ids without coordination).
- **First-class for LLM analysis** — the analyze pipeline passes the
  full doc through to `v4/llm.analyze`, so `metadata.source` lands in
  the prompt context for free.  The standard analyze prompts are
  expected to grow source-aware sections as the surface matures.
- **Immutable** — once stored, a record's source does not change.
  Operators wanting a different tagging axis should add a separate
  metadata field rather than rewriting `source`.

---

## 2. The resolution chain

Every record passes through one resolution call before it lands in
storage.  Priority order, highest wins:

1. **Explicit API parameter** — `add_with_source(doc, Some("…"))`,
   `--source <name>` on `bdscmd`, the `source` JSON-RPC param.
2. **Walked `source_keys` in priority order, each looked up
   top-level then `data.*`**:
   - top-level `source` → `data.source`
   - top-level `origin` → `data.origin`
   - top-level `host` → `data.host`
3. **Deployment default** — `data.default_source` in `bds.hjson`
   (`"global"` out of the box).

Resolution is implemented in `bdslib::common::source::resolve`:

```rust
pub fn resolve(explicit: Option<&str>, doc: &JsonValue, cfg: &SourceConfig) -> String {
    if let Some(s) = explicit { return validate(s, cfg); }
    for key in &cfg.source_keys {
        if let Some(s) = doc.get(key).and_then(|v| v.as_str()) {
            let t = s.trim();
            if !t.is_empty() { return validate(t, cfg); }
        }
        if let Some(s) = doc.get("data").and_then(|d| d.get(key)).and_then(|v| v.as_str()) {
            let t = s.trim();
            if !t.is_empty() { return validate(t, cfg); }
        }
    }
    cfg.default_source.clone()
}
```

Why the priority order: source-of-any-flavour beats origin-of-any-
flavour beats host-of-any-flavour.  Within one key, top-level beats
`data.*` (operator-explicit tagging is more specific than
parser-extracted metadata).  The syslog parser writes `host` into
`data.host`, which means a vanilla `v2/add.file.syslog` call without
`--source` produces records tagged by the parsed RFC 3164 hostname —
exactly the auto-grouping behaviour operators expect for syslog.

After resolution, the value is **injected** as a top-level `"source"`
field on the doc.  The existing
`ObservabilityStorage::build_metadata` then folds every non-canonical
top-level field into the `metadata` JSON column, so the canonical
storage location ends up being `metadata.source`.

### Worked examples

| Input (explicit / doc) | Resolved | Why |
|---|---|---|
| `--source api`, doc `{host: "worker-01"}` | `api` | Explicit wins. |
| `--source api`, doc `{metadata: {source: "x"}}` | `api` | Explicit beats pre-tag. |
| no `--source`, doc `{source: "api", data: {host: "w1"}}` | `api` | Top-level `source` beats `data.host`. |
| no `--source`, doc `{data: {source: "api", host: "w1"}}` | `api` | `data.source` beats `data.host` (priority order). |
| no `--source`, syslog line → doc `{key: "sshd", data: {host: "w1", message: "..."}}` | `w1` | `data.host` picked up via `source_keys`. |
| no `--source`, doc `{data: {v: 1}}` | `global` | Nothing matched; fall through to default. |
| explicit empty / whitespace | `global` | Validate rejects empty; falls through to default. |

---

## 3. Storage layout

`source` lives in the existing `metadata` JSON column on every shard's
`telemetry` table — **no schema migration is required**.  The flow:

```
caller
  │   doc = {timestamp, key, data, ...}
  ▼
ShardsManager::add_with_source
  │   resolve → "worker-01"
  │   doc["source"] = "worker-01"     ← top-level injection
  ▼
ObservabilityStorage::add
  │   build_metadata → metadata = {source: "worker-01", ...}
  │   INSERT INTO telemetry (id, ts, key, data, metadata, …)
  ▼
DuckDB
  │   metadata column stores JSON: {"source": "worker-01", ...}
  ▼
Read path (e.g. v2/primary)
  │   row_to_doc spreads metadata back to top level
  ▼
caller sees
  {"id": "...", "key": "...", "data": {...}, "source": "worker-01", ...}
```

So while `source` *stores* as `metadata.source`, every read path that
goes through `row_to_doc` *projects* it back to a top-level `"source"`
field on the response — equivalent to a real column for query
ergonomics, no migration overhead.

Why metadata-as-JSON instead of a dedicated column:

- **Zero schema migration.**  Existing shards work as-is; pre-existing
  records simply read back without a `source` field (callers treat
  absent as `"global"`).
- **LLM-prompt friendly.**  The analyze pipeline already passes the
  whole document to `v4/llm.analyze`, and `metadata.*` ends up in the
  prompt context without any extra plumbing.
- **Forward flexibility.**  A future move to a real column is a clean
  one-way migration (read both, prefer the column) if query-by-source
  performance ever becomes the bottleneck.

---

## 4. Configuration

The optional `data:` block in `bds.hjson`:

```hjson
data: {
  default_source:                   "global"
  source_keys:                      ["source", "origin", "host"]
  source_max_length:                256
  auto_create_source_graph_node:    true
}
```

Every key is optional; missing block / missing key / invalid value all
fall back to the library defaults shown above.

| Key | Default | Effect |
|---|---|---|
| `default_source` | `"global"` | Returned when no other resolution step yields a value.  Set to your deployment's umbrella tag (e.g. `"prod"`) if you want untagged records to roll up under it. |
| `source_keys` | `["source","origin","host"]` | Priority list walked at each resolution step.  Override to e.g. `["app","host"]` if your log shippers use different names.  Order matters; the first match wins. |
| `source_max_length` | `256` | Hard byte-cap on stored values.  Longer strings are truncated to a UTF-8-safe boundary and a `log::debug!` line is emitted. |
| `auto_create_source_graph_node` | `true` | When `true`, the data path auto-creates a `Source:<name>` graph node the first time this process observes each value (idempotent + memoised; the first replicate is one graph write, subsequent records for the same source are zero-cost).  Set `false` for deployments that manage their graph hygiene manually. |

Process-wide tuning is held in `bdslib::common::source::SourceConfig`,
installed once at bdsnode startup via `bdslib::common::source::configure`.
Library callers that never invoke `configure` get the documented
defaults — same backwards-compat shape as the dev_data / retention /
rebalancer configs.

---

## 5. API surface — Rust, RPC, CLI

### Rust library

```rust
impl ShardsManager {
    /// Existing entry point — unchanged signature; threads through
    /// the resolution chain with `None` for explicit.
    pub fn add(&self, doc: JsonValue) -> Result<Uuid>;

    /// New: explicit source override beats every other step.
    pub fn add_with_source(&self, doc: JsonValue, source: Option<&str>) -> Result<Uuid>;

    /// Same pattern for batched ingest.
    pub fn add_batch(&self, docs: Vec<JsonValue>) -> Result<Vec<Uuid>>;
    pub fn add_batch_with_source(
        &self,
        docs:   Vec<JsonValue>,
        source: Option<&str>,
    ) -> Result<Vec<Uuid>>;
}
```

Calling `add()` (or `add_batch()`) keeps the historic auto-extraction
behaviour — the resolution chain runs with no explicit override.  Use
`_with_source` when an upstream context (a CLI flag, a syslog file
ingest, etc.) needs to override every other signal.

### JSON-RPC

All four ingest RPCs grow an optional `source` parameter:

| RPC | New param | Semantics |
|---|---|---|
| [`v2/add`](jsonrpc_api/v2_add.md) | `source: string` | Applied to the single record. Works in both `queued` and `sync` modes. |
| [`v2/add.batch`](jsonrpc_api/v2_add_batch.md) | `source: string` | Applied uniformly to every record in the batch. Absent → per-record resolution. |
| [`v2/add.file`](jsonrpc_api/v2_add_file.md) | `source: string` | Applied to every record parsed from the NDJSON file. |
| [`v2/add.file.syslog`](jsonrpc_api/v2_add_file_syslog.md) | `source: string` | Applied to every syslog record parsed from the file. Absent → per-record fallback to the parsed RFC 3164 host. |

`v3/add` and `v3/add.batch` accept the same param and forward it
verbatim to peer `v2/add*` receivers — source resolves once on the
coordinator, and every replica stores the same value.

### bdscmd

| Subcommand | New flag |
|---|---|
| [`bdscmd add`](BDSCMD.md#62-add) | `--source <name>` |
| [`bdscmd add-batch`](BDSCMD.md#63-add-batch) | `--source <name>` (positional arg renamed to `input` to avoid the collision) |
| [`bdscmd add-file`](BDSCMD.md#64-add-file) | `--source <name>` |
| [`bdscmd add-file-syslog`](BDSCMD.md#65-add-file-syslog) | `--source <name>` |

Examples:

```bash
# Single record with explicit source — beats anything in the doc
bdscmd add --sync --source pipeline-a '{"timestamp": 1700000000, "key": "test", "data": {"v": 1}}'

# Batch — every record in the file gets the same source
bdscmd add-batch --source backfill events.ndjson

# Syslog with explicit override — beats the per-line host auto-promote
bdscmd add-file-syslog /var/log/syslog --source rsyslog-shipper

# Syslog WITHOUT --source — per-line resolution picks up RFC 3164 host
bdscmd add-file-syslog /var/log/syslog
# → every line gets `source = <parsed_host>` (e.g. worker-01, db-01, …)
```

---

## 6. Graph alignment

When `data.auto_create_source_graph_node = true` (the default), the
data path observes the first record from each distinct source value
and creates a corresponding graph node:

```
NodeRef { node_type: "Source", ref_id: <source name> }
node_id = UUIDv5("Source", ref_id)   ← deterministic across the cluster
```

Operators can then attach arbitrary edges from a `Source` node to any
other graph entity (`Service:foo`, `Environment:prod`, `Owner:team-a`,
…) and the data records become joinable through that path:

```bund
"Source"      "worker-01"     ?cls.graph.get_node     # node_id of the source
"Service"     "auth-api"      ?cls.graph.get_node     # node_id of a service
?cls.graph.add_edge.directed                          # link source → service
```

The first-observation logic is memoised in
`bdslib::common::source::try_mark_seen` so high-rate ingest doesn't
re-fire the graph write per record — under load the cost is one
graph mutation per new source per process lifetime, not per record.
Across a cluster, each node independently observes each source once;
the deterministic node id makes the resulting redundant writes
idempotent.

---

## 7. LLM alignment

The standard analyze pipeline (`v4/llm.analyze` + the per-target
prompts wired through bdsweb's `Analysis → *` pages) hands the full
record to the model as one of its `rows` — `metadata.source` lands
in the prompt context for free, no extra plumbing.

You can opt into source-aware analysis on existing pages by editing
their `web.analyze.<target>.prompt_template` keys in `bds.hjson` to
ask the model to group / correlate / route by source.  Worked
extension for `web.analyze.logs`:

```hjson
web.analyze.logs.prompt_template: """
  You are reviewing a slice of operational log records...

  Each record carries a `metadata.source` tag — the host, container, or
  pipeline it originated from.  When two or more records share a source,
  group them in your analysis; when sources diverge on the same event,
  call it out as a potential cross-system correlation.

  ...
"""
```

The Markov projection surface (`Analysis → Project events`) is the
natural next consumer — once each projected state carries its
source, operators can ask "what's likely to happen next, by source"
rather than the current corpus-wide view.

---

## 8. Backwards compatibility

| Surface | Behaviour after upgrade |
|---|---|
| Existing records on disk | Read back without a `source` field. Treat absent as `"global"` in query / dashboard code. No schema migration needed. |
| Existing Rust callers (`add()` / `add_batch()`) | Keep working unchanged. Documents that carry a `host` / `origin` / `source` top-level field pick up the auto-extraction; everything else lands `"global"`. |
| Existing RPC callers without `source` | Same — fallback to per-doc resolution. Responses now include a `"source"` field on success; pre-existing parsers that ignore unknown fields keep working. |
| Replicated writes (`v3/add`, `v3/add.batch`) | Source resolves once on the coordinator; peers receive an already-tagged doc and store it verbatim. Hint replay + anti-entropy preserve the value. |
| Dedup, drain3, FTS, vector | Untouched. Source is orthogonal to fingerprinting; two records with the same `(key, data)` but different sources still dedup. |
| Existing reads (`v2/timeline`, `v2/search`, `v2/primaries.*`, etc.) | Unchanged; they don't filter on source. Add the filter when you need it. |

---

## 9. Edge cases & validation

`bdslib::common::source::validate(s, cfg)`:

- Trims whitespace.
- Returns `cfg.default_source` for empty or whitespace-only input.
- Truncates strings longer than `cfg.source_max_length` to the next
  UTF-8 character boundary (`is_char_boundary`).  Emits one
  `log::debug!` line per truncation; never panics on multi-byte
  splits.
- A truncation that backs off to byte 0 (the first character itself
  is wider than the budget) falls back to the default — never returns
  empty.

The full validation matrix lives in
`src/common/source.rs` unit tests:

| Test | What it checks |
|---|---|
| `explicit_param_wins` | Priority slot 1 beats every doc-level field. |
| `top_level_source_wins_over_data_origin` | Source-of-any-flavour beats origin-of-any-flavour. |
| `data_host_picked_up_for_syslog` | Syslog auto-promote via `data.host`. |
| `falls_back_to_default_when_nothing_present` | Final fallback to `"global"`. |
| `empty_or_whitespace_treated_as_absent` | Blank strings don't block lower-priority fields. |
| `truncates_to_max_length` | Hard cap enforcement. |
| `truncation_respects_utf8_boundary` | Multi-byte split is never produced. |
| `custom_source_keys_order` | Priority list is operator-tunable. |
| `try_mark_seen_first_call_returns_true_then_false` | Graph-node memoisation. |

---

## 10. Source code map

```
src/
├── common/
│   └── source.rs            ← SourceConfig, resolve, validate, inject, try_mark_seen
├── shardsmanager.rs         ← add_with_source / add_batch_with_source +
│                              ensure_source_graph_node
└── bin/bdsnode/
    ├── main.rs              ← apply_data_config (parses `data:` hjson block)
    ├── jsonrpc/
    │   ├── add.rs           ← v2/add  source param
    │   ├── add_batch.rs     ← v2/add.batch  source param
    │   ├── add_file.rs      ← v2/add.file  source param
    │   └── add_file_syslog.rs ← v2/add.file.syslog source param
    └── server/
        ├── add_file.rs      ← {path, source} wire format on `ingest_file` pipe
        └── add_file_syslog.rs ← same on `ingest_file_syslog` pipe
```

CLI:

```
src/bin/bdscmd/cmd/
├── add.rs           ← --source
├── add_batch.rs     ← --source (and positional renamed from `source` to `input`)
├── add_file.rs      ← --source
└── add_file_syslog.rs ← --source
```

---

## See also

- [`Documentation/jsonrpc_api/v2_add.md`](jsonrpc_api/v2_add.md) /
  [`v2_add_batch.md`](jsonrpc_api/v2_add_batch.md) /
  [`v2_add_file.md`](jsonrpc_api/v2_add_file.md) /
  [`v2_add_file_syslog.md`](jsonrpc_api/v2_add_file_syslog.md) — RPC
  reference.
- [`Documentation/BDSCMD.md`](BDSCMD.md) §6.2–6.5 — CLI reference.
- [`Documentation/BDSCONFIG.md`](BDSCONFIG.md) §2 — the `data:` hjson
  block.
- [`Documentation/CLUSTER.md`](CLUSTER.md) — how source values
  replicate across peers (they ride with the record body through
  `v3/add` and the docstore anti-entropy paths).
