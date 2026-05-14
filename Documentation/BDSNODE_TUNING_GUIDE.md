# bdsnode Tuning Guide

A goal-oriented, one-stop guide to tuning bdsnode for **performance**,
**reliability**, and **functionality**.

This guide is organised by *what you want to achieve*.  For the
exhaustive per-key reference — every config key with type, default,
and clamp range — see [`BDSCONFIG.md`](BDSCONFIG.md).  This document
explains *why* the defaults are what they are, *when* to deviate, and
*how* each choice ripples through bdsnode's operation.

> **The short version.** Every default in `bds.hjson` is chosen to be
> safe and sensible on a real workload.  A fresh node with only
> `dbpath` and `shard_duration` set will run correctly.  You tune when
> a measurement tells you to — not preemptively.

---

## Table of contents

1. [Tune by measurement, not by guess](#1-tune-by-measurement-not-by-guess)
2. [Performance tuning](#2-performance-tuning)
3. [Reliability tuning](#3-reliability-tuning)
4. [Functionality tuning](#4-functionality-tuning)
5. [How tuning choices interact](#5-how-tuning-choices-interact)
6. [Recommended profiles](#6-recommended-profiles)
7. [Full annotated `bds.hjson`](#7-full-annotated-bdshjson)

---

## 1. Tune by measurement, not by guess

bdsnode exposes everything you need to decide whether a knob needs
turning.  Always look before you tune:

| Tool | What it tells you |
|------|-------------------|
| `bdscmd status` | Queue depths (`logs_queue`), `jsoncache_pct`, `ingest_flushers`, `pool.checkout_timeouts`, `self_healing`, `health` verdict |
| `bdscmd perf` | p50/p95/p99 for every hot path — `ingest.flush`, `ingest.lag`, `embed.hit`/`embed.miss`, `shard.*`, `fanout.*`, `replicate.*` |
| `bdscmd perf-slow` | Single-event outliers that p95 hides |
| `bdscmd health` | Aggregate self-healing verdict + per-source heartbeats |

The pattern is always: **read a metric → identify the bottleneck →
turn exactly one knob → re-measure.**  Turning several knobs at once
makes it impossible to know which one helped.

The metric-to-knob map is the spine of the rest of this guide.

---

## 2. Performance tuning

### 2.1 Ingest throughput

The ingest path: `v2/add` → bounded crossbeam channel → flusher
thread → `ShardsManager::add_batch` → (embed → DuckDB insert →
Tantivy commit → HNSW upsert), parallelised across shards via Rayon.

| Knob | Default | Raise when | Lower when |
|------|--------:|------------|------------|
| `pipe_batch_size` | `500` | per-record ONNX embed overhead is high — bigger batches amortise the embed/commit/transaction costs | ONNX runtime memory growth; you need lower single-record latency |
| `pipe_timeout_ms` | `500` | — | single-record latency is visibly bad (flush partial batches sooner) |
| `pipe_flushers` | `1` | `v2/perf` shows `ingest.lag.p95_us` high **while** `ingest.flush.p95_us` is low (batch *accumulation* is the bottleneck) — rare | almost never |
| `ingest_channel_capacity` | `100000` | `v2/add` returns `-32099 channel overloaded` and you have memory headroom | memory pressure (each queued record is a `serde_json::Value` in RAM) |
| `pool_size` | `4` | high-concurrency ingest — many parallel `v2/add` callers contending for DuckDB connections | memory pressure |

**Why `pipe_flushers` defaults to 1.** The flush itself is
internally serialised — DuckDB, Tantivy, and HNSW are not safe under
concurrent same-shard writers — so extra flushers do **not**
parallelise flushing.  Within a single `add_batch` call the work is
*already* parallelised across shards by Rayon, so one flusher
saturates the engines.  Extra flushers only let one thread accumulate
a new batch while another flushes — a niche win for very bursty
traffic.  The flusher *supervisor* keeps every flusher alive
regardless of the count (see [`NODE_RELIABILITY.md §2`](NODE_RELIABILITY.md)).

**The biggest ingest lever is `pipe_batch_size`.** Each flush pays a
fixed cost (ONNX batch call, DuckDB transaction, Tantivy commit) that
amortises over the batch.  At `500` that fixed cost is spread thin.
Going to `1000–2000` helps a dense firehose; going to `50–100` cuts
latency for a sparse trickle at the cost of throughput.

⚠ **Never set `ingest_channel_capacity: 0`** — that is the legacy
*unbounded* mode: the channel grows until the kernel OOM-kills the
process.  Always set a finite bound; `100000` records is a few
hundred MB worst-case and gives the flusher room to catch up.

### 2.2 Query latency

| Knob | Default | Effect |
|------|--------:|--------|
| `max_open_shards` | `16` | LRU cap on open shards.  A query spanning more shards than the cap pays repeated open/evict churn.  Raise when you see frequent "shard evicted" log lines or queries routinely span > 16 shards.  Each open shard costs ~12 file descriptors + tens of MB of mapped pages. |
| `jsoncache_capacity` | `10000` | Process-wide LRU of recent records, consulted before any DuckDB round-trip on FTS/vector hits.  Watch `jsoncache_pct` in `v2/status` — if it sits at 100 %, raising the capacity buys hit-rate. |
| `embedding_cache_size` | `256` | Caches query-string → embedding.  Repeated identical queries (dashboard polls) skip ONNX entirely.  Watch the `embed.hit` / `embed.miss` ratio in `v2/perf`; a low hit ratio with a dashboard workload means queries vary more than the cache holds — raise it (memory cost ≈ `size × dim × 4 B`, ~400 KiB at the default). |
| `shard_duration` | — (required) | Narrower shards (`"1h"`) → more shards, finer LRU rotation, more catalog rows; wider shards (`"1day"`) → fewer, larger shards.  High-throughput telemetry favours `"1h"`; archival favours `"7days"`.  **This is a lock-in choice** — historical shards keep their original width. |

The `shard.*` perf series tell you where a slow search spends its
time: `shard.vector_scored_precomputed` is HNSW + MMR rerank,
`shard.fts_scored` is Tantivy BM25.  A spike there usually means a
cold shard just got opened — raise `max_open_shards` if it is chronic.

### 2.3 Cluster read/write latency (cluster mode)

| Knob | Default | Effect |
|------|--------:|--------|
| `cluster.peer_rpc_timeout` | `"2s"` | Hard per-peer RPC deadline ceiling.  Keep it `< gossip_interval`. |
| `cluster.adaptive_peer_timeout_enabled` | `true` | The read fan-out derives a *per-peer* deadline from that peer's observed p95 — a slow peer no longer makes every cluster read pay the worst-case timeout.  Leave on unless WAN latency spikes are expected and you prefer "wait the full deadline". |
| `cluster.adaptive_peer_timeout_multiplier` | `3.0` | Lower → more aggressive fast-fail on a slow peer; higher → more tolerant. |
| `cluster.replication_factor` | `3` | Higher → more durable + more read coverage, but every write fans out to more peers.  See §3 for the durability angle. |

`fanout.peer.<id>` and `fanout.method.<m>` in `v2/perf` show
per-peer and per-RPC fan-out RTT — that is how you spot one slow
peer dragging down cluster reads.

---

## 3. Reliability tuning

Most of bdsnode's reliability is **automatic and unconfigurable** —
hot-path panic elimination, per-record batch fallback, background-task
panic supervision, bounded pool checkout, the flusher supervisor.  See
[`NODE_RELIABILITY.md`](NODE_RELIABILITY.md) and
[`NODE_SELF_HEALING.md`](NODE_SELF_HEALING.md) for the full picture.
The knobs that *do* matter for reliability:

### 3.1 Durability — the single most important reliability knob

```hjson
sync_interval_secs: 60
```

`bdslib::sync_db()` flushes every open shard's DuckDB WAL
(`CHECKPOINT`), Tantivy commit, VecStore, and tplstorage to disk.
Between syncs, writes live in WALs and in-memory index buffers.

**An unclean exit (`kill -9`, OOM, hardware fault) loses every write
since the active shard's last sync.** With the default `60`, that
window is at most 60 seconds.  Setting `sync_interval_secs: 0`
disables periodic sync entirely — fast, but an unclean exit then
loses *everything since the last LRU shard eviction or graceful
shutdown*, which can be hours.

- **Keep `60`** for almost everyone — a sync tick is sub-second on a
  warm WAL.
- **Lower to `10–30`** when the loss of even a minute of data is
  unacceptable and you can afford slightly more I/O.
- **`0` only** on a node ingesting truly disposable data, or where an
  upstream system is the source of truth and replay is cheap.

### 3.2 Resource bounds — preventing OOM and FD exhaustion

| Knob | Default | Reliability role |
|------|--------:|------------------|
| `nofile_limit` | `4096` | bdsnode opens ~12 files per shard.  The rule of thumb is `nofile_limit ≥ max_open_shards × 12 + 100`.  Too low → `EMFILE` errors mid-operation.  The kernel clamps it to the process hard limit; check the boot log for what was actually applied. |
| `max_open_shards` | `16` | Caps simultaneously-open shards → caps FD count and mapped memory.  Raising it for query performance (§2.2) means raising `nofile_limit` to match. |
| `ingest_channel_capacity` | `100000` | The hard ceiling on in-flight records.  This is the OOM guard rail — see §2.1.  `0` removes the guard rail. |
| `pool_size` | `4` | DuckDB connections per shard.  The pool checkout is bounded at 10 s (fixed in code) — a non-zero `v2/status.pool.checkout_timeouts` means a pool ran dry under load; raise `pool_size` or shed load. |

### 3.3 Cluster durability

```hjson
cluster: {
  replication_factor: 3
}
```

`replication_factor` is how many nodes hold a copy of each sharded
record.  For data safety:

- **`replication_factor ≥ 2`** is the floor for any cluster you care
  about — `1` means no redundancy.
- If you run **retention** (`retention.enabled = true`), you *must*
  have `replication_factor ≥ 2` — otherwise retention evicts the only
  copy of data.  bdsnode emits a startup WARN if you do this.
- The **rebalancer** (`rebalancer.enabled = true`) is what *restores*
  replication factor over time when a node was down during writes —
  enable it on any cluster where nodes restart.

### 3.4 Self-healing — automated recovery

The self-healing layer (quarantine + the three-tier rebuild healer +
consistency sweep + circuit breaker) is **on by default** and only
ever touches shards the quarantine layer has already isolated — leave
it on.  The one opt-in, *destructive* knob:

```hjson
self_healing: {
  recreate_failed_shards:      false   // Tier-3 — destroys + recreates an unrepairable shard
  failed_shard_recreate_after: "1h"
}
```

Enable `recreate_failed_shards` **only** in a cluster with the
rebalancer on — it destroys a shard whose DuckDB is unrepairable and
recreates it empty for peers to repopulate.  On a standalone node the
healer refuses (it would be permanent data loss).  See
[`NODE_SELF_HEALING.md §3`](NODE_SELF_HEALING.md).

---

## 4. Functionality tuning

These knobs turn *features* on and off rather than tuning a
performance/reliability trade-off.

### 4.1 Ingest-time features

| Knob | Default | Turn on when |
|------|--------:|--------------|
| `drain_enabled` | `false` | You want drain3 log-template mining (`v2/tpl.*`).  Costs measurable CPU per record — leave off on a pure-metrics firehose. |
| `drain_load_duration` | `"24h"` | (only relevant when drain is on) how far back to rehydrate templates at startup so UUIDs stay stable across restarts. |
| `similarity_threshold` | `0.85` | Lower → more aggressive dedup (more secondaries, less storage, but risks collapsing distinct records).  Higher → more primaries.  `1.0` = only bit-exact dedup; `0.0` = never dedup. |
| `embedding_model` | `"AllMiniLML6V2"` | You need higher retrieval quality (`BGESmallENV15`, `BGELargeENV15`) or multilingual support.  ⚠ **Dimension lock-in** — changing it on an existing `dbpath` breaks vector search; you must `bdsnode --new`. |

### 4.2 Background feature tasks (all opt-in or default-on)

| Block | Default | What it does |
|-------|---------|--------------|
| `retention:` | `enabled: false` | Time-based shard eviction.  Opt-in; needs `replication_factor ≥ 2`.  See [`RETENTION.md`](RETENTION.md). |
| `rebalancer:` | `enabled: false` | Restores replication factor by pushing under-replicated records to peers.  Cluster-only; opt-in. |
| `self_healing:` | `enabled: true` | Shard quarantine + rebuild healer + consistency sweep.  Default on; see §3.4 and [`NODE_SELF_HEALING.md`](NODE_SELF_HEALING.md). |
| `generate_realistic_data:` | `enabled: false` | **Dev/demo only** — synthetic-telemetry generator.  Never enable in production; the bdsweb dashboard shows a red banner when it is on. |
| `scheduler_interval_secs` | `60` | Cron-driven BUND script scheduler.  `0` disables it. |
| `perf.slow_query_threshold_ms` | `500` | Slow-query log threshold.  `0` disables the slow-query ring (the percentile registry stays populated). |

### 4.3 Cluster mode

The `cluster:` block is the master switch for everything in
[`CLUSTER.md`](CLUSTER.md) / [`CLUSTER_DETAILS.md`](CLUSTER_DETAILS.md)
— gossip, replication, fan-out reads, hinted handoff, anti-entropy,
the rebalancer, `v3/user.*` auth.  Absent or `enabled: false` → the
node runs standalone.  The two required keys when enabled are
`shared_secret` (≥ 16 chars) and `bind_url`.

### 4.4 BUND VM sandbox

`bund.disabled_categories` / `bund.disabled_words` gate which
host-touching BUND words are callable.  Default is "nothing disabled"
— fine for a single-operator box.  **Disable `os_shell` +
`process_control` on any production node**, and disable far more if
you expose the VM to less-trusted callers (chat snippet eval, public
Bund playground).  See [`BDSCONFIG.md §4.1`](BDSCONFIG.md) for the
recommended profiles.

### 4.5 LLM surface

The `llm:` block registers providers (Ollama / DeepSeek / Anthropic /
OpenAI) and configures the inference cache, dedup, async runner, and
the English→Bund translator.  Entirely optional — absent → no
`v4/llm.*` surface.  See [`LLM.md`](LLM.md).

---

## 5. How tuning choices interact

The knobs are not independent.  The relationships that bite people:

| If you change… | …you probably also need to change | Why |
|----------------|-----------------------------------|-----|
| `max_open_shards` ↑ | `nofile_limit` ↑ | Each open shard is ~12 FDs; `nofile ≥ max_open_shards × 12 + 100`. |
| `pipe_batch_size` ↑ | watch ONNX memory | Bigger batches = bigger per-call embedding working set. |
| `ingest_channel_capacity` ↑ | watch process RAM | Every queued record is a `serde_json::Value` held in memory. |
| `retention.enabled = true` | `cluster.replication_factor ≥ 2` | Retention evicts shards per-node; with `rf = 1` it deletes the only copy. |
| `self_healing.recreate_failed_shards = true` | `rebalancer.enabled = true` + cluster mode | Tier-3 recreates an *empty* shard; only the rebalancer + peers can refill it.  The healer refuses on a standalone node. |
| `sync_interval_secs = 0` | accept hours of unclean-exit data loss | Sync then only happens on LRU eviction + graceful shutdown. |
| `bund_ttl_secs` | `vm_cleanup_interval_secs ≤ bund_ttl_secs` | The sweep interval must be ≤ the TTL or stale VMs linger a full tick past their deadline. |
| `cluster.gossip_interval` ↑ | `cluster.suspect_timeout ≥ 2× gossip`, `dead_timeout ≥ 3× suspect` | Otherwise peers flap between states on transient packet loss. |
| `cluster.peer_rpc_timeout` | keep `< gossip_interval` | Each gossip tick must finish its ping fan-out before the next starts. |
| `embedding_model` change | `bdsnode --new` (fresh `dbpath`) | The HNSW dimension is locked at the first vector insert. |
| `pool_size` ↓ | watch `v2/status.pool.checkout_timeouts` | Too few connections → 10 s checkout stalls under concurrent load. |

---

## 6. Recommended profiles

Three starting points.  Start from the closest one, then tune by
measurement (§1).

### Small dev / single-operator box

Defaults are already right.  The minimum viable config:

```hjson
{
  dbpath:         "/var/lib/bdslib"
  shard_duration: "1h"
}
```

Everything else defaults sensibly.  Optionally enable `drain_enabled`
if you want template mining.

### Single-node production

```hjson
{
  dbpath:          "/var/lib/bdslib"
  shard_duration:  "6h"
  max_open_shards: 32
  nofile_limit:    8192        // 32 × 12 + headroom

  sync_interval_secs: 60       // bounded unclean-exit loss window

  bund: { disabled_categories: ["os_shell", "process_control"] }

  retention: {
    enabled:  true
    duration: "30days"
  }
}
```

Single-node + retention is acceptable *if* the data is reproducible
upstream — there is no peer copy.  Otherwise run a cluster.

### Cluster node (3-node, full automated recovery)

```hjson
{
  dbpath:          "/var/lib/bdslib"
  shard_duration:  "1day"
  max_open_shards: 48
  nofile_limit:    12288

  sync_interval_secs: 30

  bund: { disabled_categories: ["os_shell", "process_control"] }

  cluster: {
    enabled:            true
    shared_secret:      "<32+ random chars — same on every node>"
    bind_url:           "http://10.0.0.11:9000"
    bootstrap:          "http://10.0.0.10:9000"   // omit on the first node
    replication_factor: 3
    full_mode_threshold: 3
  }

  retention: {
    enabled:  true
    duration: "90days"          // safe — rf=3 means peers hold copies
  }

  rebalancer: {
    enabled:  true
    interval: "10m"
  }

  self_healing: {
    enabled:                     true
    recreate_failed_shards:      true   // safe: cluster + rebalancer on
    failed_shard_recreate_after: "30m"
  }
}
```

---

## 7. Full annotated `bds.hjson`

Every key bdsnode reads, with its default and a one-line rationale.
Copy this, delete what you do not need, and change only what a
measurement tells you to.  Keys marked **REQUIRED** have no default.

```hjson
{
  // ════════════════════════════════════════════════════════════════
  //  PROCESS + STORAGE
  // ════════════════════════════════════════════════════════════════

  // REQUIRED. Root directory for every shard, docstore, signals,
  // scripts, users, and llm store. Created if missing. Survives
  // restarts; wiped only by `bdsnode --new`.
  dbpath: "/var/lib/bdslib"

  // REQUIRED. Width of each time-partitioned shard. "1h" for a
  // high-throughput firehose, "6h"/"1day" for app logs, "7days" for
  // archival. LOCK-IN: historical shards keep their original width.
  shard_duration: "1h"

  // LRU cap on simultaneously-open shards. Each open shard ≈ 12 FDs
  // + tens of MB mapped. Raise if queries span more shards than this
  // (watch for "shard evicted" log churn); lower under memory/FD
  // pressure. Default 16.
  max_open_shards: 16

  // Soft RLIMIT_NOFILE requested at startup. Rule of thumb:
  // max_open_shards × 12 + 100. Kernel clamps to the hard limit.
  // Default 4096.
  nofile_limit: 4096

  // DuckDB connections per shard. Raise for high-concurrency ingest;
  // lower for memory. Checkout is hard-bounded at 10s — a non-zero
  // v2/status.pool.checkout_timeouts means this is too low. Default 4.
  pool_size: 4

  // Process-wide r2d2 maintenance thread pool. Almost never tuned.
  // Default 3.
  r2d2_thread_pool_size: 3

  // ── Embedding model ──────────────────────────────────────────────
  // fastembed model variant. DIMENSION LOCK-IN: changing this on an
  // existing dbpath breaks vector search — use `bdsnode --new`.
  // Default "AllMiniLML6V2" (384-dim, fast/small).
  embedding_model: "AllMiniLML6V2"

  // Optional override for the fastembed model download cache.
  // Default: fastembed's own (~/.cache/huggingface/hub).
  // embedding_cache_dir: "/var/lib/bdslib/models"

  // In-process query-embedding cache capacity. Repeated query
  // strings skip ONNX. Memory ≈ size × dim × 4B. Watch the
  // embed.hit/embed.miss ratio in v2/perf. 0 disables. Default 256.
  embedding_cache_size: 256

  // Cosine-similarity cutoff for primary-vs-secondary dedup.
  // 1.0 = bit-exact only; 0.85 = tolerant; 0.0 = never dedup.
  // Default 0.85.
  similarity_threshold: 0.85

  // drain3 log-template mining on every v2/add. Measurable CPU cost
  // per record — leave off for a pure-metrics firehose. Default false.
  drain_enabled: false
  // How far back to rehydrate templates at startup (drain only).
  drain_load_duration: "24h"

  // Process-wide LRU of recent records, checked before any DuckDB
  // round-trip. Watch jsoncache_pct in v2/status. 0 disables.
  jsoncache_capacity: 10000
  jsoncache_ttl_secs: 300

  // ════════════════════════════════════════════════════════════════
  //  INGEST TUNING
  // ════════════════════════════════════════════════════════════════

  // Hard ceiling on in-flight records (the OOM guard rail).
  // NEVER set to 0 — that is unbounded mode. Raise if v2/add returns
  // -32099 channel overloaded AND you have RAM headroom. Default 100000.
  ingest_channel_capacity: 100000

  // v2/add — single-record + small-batch ingest.
  // batch_size is the biggest throughput lever (amortises the fixed
  // embed/commit/transaction cost). timeout flushes a partial batch
  // after silence. flushers: keep 1 — the flush is internally
  // serialised; >1 only helps very bursty accumulation.
  pipe_batch_size: 500
  pipe_timeout_ms: 500
  pipe_flushers:   1

  // v2/add.file — newline-delimited JSON file ingest.
  file_batch_size: 500
  file_timeout_ms: 5000

  // v2/add.file.syslog — RFC 3164 syslog ingest.
  syslog_batch_size: 500
  syslog_timeout_ms: 5000

  // ════════════════════════════════════════════════════════════════
  //  BUND VM RUNTIME
  // ════════════════════════════════════════════════════════════════

  // Threads in the BundWorkerPool (v2/eval.queued). Floor 1.
  n_workers: 4

  // Time-to-idle for stateful named BUND VM contexts. Default 300.
  bund_ttl_secs: 300

  // BUND word sandbox. Default: nothing disabled. On ANY production
  // node disable at least os_shell + process_control. Disable far
  // more if untrusted callers reach the VM (chat eval, public REPL).
  bund: {
    disabled_categories: ["os_shell", "process_control"]
    // disabled_words: ["cls.script.add", "cls.script.update", "cls.script.delete"]
  }

  // Slow-query log threshold. Calls slower than this land in the
  // 100-entry v2/perf.slow_queries ring. 0 disables the ring (the
  // percentile registry stays populated). Default 500.
  perf: {
    slow_query_threshold_ms: 500
  }

  // ════════════════════════════════════════════════════════════════
  //  BACKGROUND TASKS
  // ════════════════════════════════════════════════════════════════

  // Cron-driven BUND script scheduler tick. 0 disables. Default 60
  // (matches standard crontab once-per-minute semantics).
  scheduler_interval_secs: 60

  // Periodic global sync (DuckDB CHECKPOINT + Tantivy commit + flush).
  // THE durability knob: an unclean exit loses everything since the
  // last sync. 0 disables (loses hours on a crash). Default 60.
  sync_interval_secs: 60

  // Result-queue sweeper (v2/results.* / v2/eval.queued delivery).
  // ttl 0 keeps queues forever. Defaults: ttl 600, sweep 30.
  results_ttl_secs:   600
  results_sweep_secs: 30

  // BUND VM idle sweeper. Keep ≤ bund_ttl_secs. Default 60.
  vm_cleanup_interval_secs: 60

  // ── Shard retention (opt-in; time-based eviction) ────────────────
  // REQUIRES replication_factor ≥ 2 for safety — retention is
  // per-node and will evict the only copy with rf=1.
  retention: {
    enabled:                  false
    duration:                 "30days"   // shards older than this are evictable
    interval_secs:            300        // sweep cadence, clamped [60, 86400]; 0 = manual only
    max_evictions_per_run:    50         // per-sweep cap; 0 = unbounded
    dry_run:                  false      // log what would evict, don't act
    reload_drain_after_evict: true
    quorum_check_enabled:     false      // cluster safety net: only evict if peers hold a copy
    quorum_min_peers:         1
  }

  // ── Data rebalancer (opt-in; cluster-only) ───────────────────────
  // Restores replication factor by pushing under-replicated records
  // to peers. Enable on any cluster where nodes restart.
  rebalancer: {
    enabled:                    false
    interval:                   "10m"
    batch_size:                 50       // IDs per has_records probe; clamp [1, 1000]
    max_per_run:                500      // records examined per tick
    min_replication_factor:     null     // null = inherit cluster.replication_factor
    pause_if_ingest_lag_p95_ms: 1000     // skip the tick when ingest is backed up
  }

  // ── Self-healing (default ON; only touches isolated shards) ──────
  self_healing: {
    enabled:                     true
    interval:                    "60s"   // heal-sweep cadence; clamp [10s, 1h]
    consistency_interval:        "10m"   // cross-engine drift check; "0s" disables
    // Tier-3: DESTRUCTIVE. Destroys an unrepairable shard + recreates
    // it empty for peers to refill. Only enable in a cluster with the
    // rebalancer on — the healer refuses on a standalone node anyway.
    recreate_failed_shards:      false
    failed_shard_recreate_after: "1h"
  }

  // ── Synthetic data generator (DEV/DEMO ONLY) ─────────────────────
  // Never enable in production — bdsweb shows a red banner when on.
  // generate_realistic_data: { enabled: false }

  // ════════════════════════════════════════════════════════════════
  //  CLUSTER (omit entirely for a standalone node)
  // ════════════════════════════════════════════════════════════════
  cluster: {
    enabled:       false
    // REQUIRED when enabled — ≥ 16 chars, identical on every node.
    shared_secret: "change-me-to-32-or-more-random-chars"
    // REQUIRED when enabled — where peers reach this node.
    bind_url:      "http://10.0.0.11:9000"
    // A peer to join through. Omit on the first/bootstrap node.
    // bootstrap:  "http://10.0.0.10:9000"

    // Gossip cadence. Keep suspect ≥ 2× gossip, dead ≥ 3× suspect,
    // peer_rpc_timeout < gossip_interval.
    gossip_interval:  "5s"
    suspect_timeout:  "30s"
    dead_timeout:     "120s"
    peer_rpc_timeout: "2s"

    // Adaptive per-peer RPC timeout: derive each peer's deadline from
    // its observed p95 so one slow peer doesn't drag every read.
    adaptive_peer_timeout_enabled:    true
    adaptive_peer_timeout_multiplier: 3.0

    // Replicas per sharded record. ≥ 2 for any cluster you care
    // about; required ≥ 2 if retention is on. Default 3.
    replication_factor:  3
    // Peer count at which the cluster enters full-replication mode.
    full_mode_threshold: 3
    // Fully-replicated stores (anti-entropy keeps them converged).
    full_replication_stores: ["docs", "signals", "scripts", "users", "llm_cache"]

    // Hinted handoff + anti-entropy cadences.
    antientropy_interval: "300s"
    hint_replay_interval: "10s"
    hint_max_age:         "86400s"

    // Bootstrap recovery.
    floating_bootstrap:       true
    bootstrap_retry_interval: "60s"

    // Scheduler dedup window — suppress a script fire if any peer ran
    // it within this window. Set well above inter-node clock skew.
    scheduler_dedup_window: "300s"

    // Authentication (bdsweb sessions + v3/user.*).
    session_ttl:                "8h"
    auth_rate_limit_per_minute: 10
  }

  // ════════════════════════════════════════════════════════════════
  //  LLM SURFACE (omit entirely to disable v4/llm.*)
  // ════════════════════════════════════════════════════════════════
  // llm: {
  //   default: "ollama"
  //   providers: {
  //     ollama: { url: "http://127.0.0.1:11434", default_model: "llama3.2" }
  //   }
  //   cache:   { enabled: true, ttl_secs: 86400 }
  //   dedup:   { enabled: true, window_secs: 300, wait_max_secs: 30 }
  //   runner:  { enabled: true, max_concurrency: 2, poll_interval_secs: 1 }
  //   to_bund: { enabled: true, timeout_secs: 120, max_retries: 2 }
  // }

  // ════════════════════════════════════════════════════════════════
  //  BDSWEB-SPECIFIC (read by bdsweb, ignored by bdsnode)
  // ════════════════════════════════════════════════════════════════
  // dashboard_refresh_secs: 30
  // cluster_refresh_secs:   10
  // web: { analyze: { logs: { timeout_secs: 600, max_rows: 50 } } }
}
```

---

## See also

- [`BDSCONFIG.md`](BDSCONFIG.md) — the exhaustive per-key reference
  (every clamp range, every cross-key relationship, the per-binary
  matrix).
- [`NODE_RELIABILITY.md`](NODE_RELIABILITY.md) — the fault-tolerance
  layer (mostly automatic; the knobs that matter are cross-referenced
  in §3 here).
- [`NODE_SELF_HEALING.md`](NODE_SELF_HEALING.md) — the self-healing
  layer (`self_healing:` block, the rebuild healer, Tier-3).
- [`RETENTION.md`](RETENTION.md) — the retention design + operator
  playbook.
- [`CLUSTER.md`](CLUSTER.md) / [`CLUSTER_DETAILS.md`](CLUSTER_DETAILS.md)
  — cluster configuration and protocol-level reference.
- [`BDSCMD.md`](BDSCMD.md) — the `bdscmd` client used for every
  measurement in §1.
