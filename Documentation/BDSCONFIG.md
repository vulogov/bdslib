# `bds.hjson` — Configuration Reference

`bds.hjson` is the single configuration file consumed by every
bdslib-based binary.  bdsnode reads it on startup via
`--config <path>` (or the `BDS_CONFIG` env var); bdsweb reads the
same file when its `--config` flag points at it; bdscli/bdscmd
operate on a running bdsnode and don't read it directly.

The file uses [Hjson](https://hjson.github.io/) — JSON with
comments, unquoted keys, optional commas — and is parsed by
`serde_hjson` into a flat key/value tree.  Unknown keys are
silently ignored, so the same file can be shared across versions.

This document is the canonical reference.  Topic-specific docs
cross-reference it rather than duplicating tuning advice.

---

## Table of contents

1. [How the file is loaded](#1-how-the-file-is-loaded)
2. [Top-level: process + storage](#2-top-level-process--storage)
   - 2.1 [OS resource limits](#21-os-resource-limits)
   - 2.2 [Storage layout](#22-storage-layout)
   - 2.3 [DuckDB connection pooling](#23-duckdb-connection-pooling)
   - 2.4 [Embedding model](#24-embedding-model)
   - 2.5 [Deduplication](#25-deduplication)
   - 2.6 [Drain3 log-template mining](#26-drain3-log-template-mining)
   - 2.7 [JSON record cache](#27-json-record-cache)
3. [Ingest tuning](#3-ingest-tuning)
4. [BUND VM runtime](#4-bund-vm-runtime)
   - 4.1 [BUND word sandbox](#41-bund-word-sandbox)
5. [Background tasks](#5-background-tasks)
   - 5.1 [Cron-driven script scheduler](#51-cron-driven-script-scheduler)
   - 5.2 [Periodic global sync](#52-periodic-global-sync)
   - 5.3 [Result-queue sweeper](#53-result-queue-sweeper)
   - 5.4 [BUND VM cleanup](#54-bund-vm-cleanup)
6. [`cluster:` block](#6-cluster-block)
   - 6.1 [Membership](#61-membership)
   - 6.2 [Gossip cadence](#62-gossip-cadence)
   - 6.3 [Replication](#63-replication)
   - 6.4 [Hinted handoff + anti-entropy](#64-hinted-handoff--anti-entropy)
   - 6.5 [Scheduler dedup](#65-scheduler-dedup)
   - 6.6 [Authentication](#66-authentication)
7. [`llm:` block](#7-llm-block)
   - 7.1 [Provider registration](#71-provider-registration)
   - 7.2 [`llm.cache`](#72-llmcache)
   - 7.3 [`llm.dedup`](#73-llmdedup)
   - 7.4 [`llm.runner`](#74-llmrunner)
8. [bdsweb-specific keys](#8-bdsweb-specific-keys)
   - 8.1 [`web.analyze.*` — "Analyze this!" buttons](#81-webanalyze--analyze-this-buttons)
9. [Legacy `v2/chat.ollama` keys](#9-legacy-v2chatollama-keys)
10. [Tuning matrix](#10-tuning-matrix)
11. [Required-vs-optional summary](#11-required-vs-optional-summary)
12. [Per-binary key matrix](#12-per-binary-key-matrix)

---

## 1. How the file is loaded

```
bdsnode --config bds.hjson
            │
            ▼
   fs::read_to_string
            │
            ▼
   serde_hjson::from_str → serde_hjson::Value
            │
            ├── nofile_limit_from_config         → setrlimit
            ├── n_workers_from_config            → BundWorkerPool::start
            ├── ingest_channel_capacity_*        → ingest::init
            ├── bdslib::init_db(config_path)     ← reads `dbpath`, `shard_duration`,
            │                                       `pool_size`, `similarity_threshold`,
            │                                       `drain_enabled`, `drain_load_duration`,
            │                                       `jsoncache_*`, `r2d2_thread_pool_size`,
            │                                       `max_open_shards`, `embedding_*`
            │
            ├── Cluster::init (when cluster.enabled = true)
            │       ↑ reads the `cluster:` sub-object via from_hjson_str
            │
            ├── ProviderManager + cache::init + dedup::init_settings
            │       ↑ reads the `llm:` sub-object via LlmConfig::load_from_hjson
            │
            ├── server::scheduler::Config::from_config
            ├── server::sync::Config::from_config
            ├── server::results_sweeper::Config::from_config
            ├── server::bundcleanup::Config::from_config
            └── server::llm_jobs::Config::from_config
```

Each component re-reads the file independently — there is no shared
parsed-config struct.  Missing keys fall back to per-component
defaults; unknown keys are silently ignored.  Unparseable hjson
(syntax error) aborts startup with a `context("hjson parse error")`
message.

**Required keys for bdsnode** (anything else is optional):

- `dbpath` (string) — storage root
- `shard_duration` (humantime string) — time-bucket width
- `cluster.shared_secret` + `cluster.bind_url` (only when
  `cluster.enabled = true`)

**bdsweb** reads only a small subset (the `cluster.*` auth fields +
the `*_refresh_secs` knobs).  bdscli/bdscmd don't read the file.

---

## 2. Top-level: process + storage

### 2.1 OS resource limits

```hjson
nofile_limit: 4096
```

| Field         | Type    | Default | Required |
|---------------|---------|---------|----------|
| `nofile_limit`| integer | 4096    | no       |

Soft `RLIMIT_NOFILE` requested at startup via `rlimit::setrlimit`.
bdsnode opens many DuckDB files and vecstore indexes per shard;
the OS default (256 on macOS, 1024 on Linux) runs out within hours
on real workloads.  Clamped to the process hard limit by the kernel.

**Sizing rule of thumb:**
`nofile_limit ≥ max_open_shards × ~12 + 100`

The ~12 comes from each open shard holding open: 1 DuckDB file
(`obs.db`) + multiple Tantivy index files + multiple tplstorage
files (metadata.db, blobs.db, frequency.db).  The +100 covers
the docstore + signals + scripts + users + llm stores + the
TCP listener + reqwest pool.

If `setrlimit` fails (you asked for more than the hard limit),
bdsnode logs a warning and continues with whatever the OS granted.
Check the boot log for the actual applied value.

### 2.2 Storage layout

```hjson
dbpath:          "/var/lib/bdslib"
shard_duration:  "1h"
max_open_shards: 32
```

| Field             | Type             | Default | Required |
|-------------------|------------------|---------|----------|
| `dbpath`          | string           | —       | **yes**  |
| `shard_duration`  | humantime string | —       | **yes**  |
| `max_open_shards` | integer          | 16      | no       |

- **`dbpath`** — root directory for all shards, docstore, signals,
  scripts, users, and llm stores.  Created if missing.  Survives
  process restarts; wiped only by `bdsnode --new`.

- **`shard_duration`** — width of each time-partitioned shard.
  Every document is routed to the shard whose `[start, end)`
  interval contains its `timestamp` field.  Common values:

  | Value      | Records per shard (rough) | When to pick                                |
  |------------|---------------------------|---------------------------------------------|
  | `"1h"`     | thousands                 | High-throughput telemetry, fast LRU rotation |
  | `"6h"`     | tens of thousands         | Default for app logs                        |
  | `"1day"`   | hundreds of thousands     | Production telemetry, long retention        |
  | `"7days"`  | millions                  | Cold storage, archival                      |

  **Lock-in warning.**  All existing shards under `dbpath` were
  created with the original `shard_duration`.  Changing it produces
  shards of the new width going forward, but **historical shards
  retain their original boundaries** — queries may straddle
  uneven shard widths.  Re-bucket by exporting + reingesting into
  a fresh `dbpath`.

- **`max_open_shards`** — LRU cap on simultaneously open shards.
  Each open shard holds connections + memory-mapped index pages.
  When the cap is reached, the least-recently-used shard is synced
  to disk (CHECKPOINT + Tantivy commit + VecStore flush) and
  closed before the new one is opened.

  Raise this knob when:
  - You see frequent "shard evicted" log lines during normal
    operation (eviction churn slows queries).
  - Your queries routinely span more shards than the cap.

  Lower when:
  - Memory pressure or file-descriptor count is high (each open
    shard is ~12 FDs and ~tens of MB of mapped pages).

  Relationship: `nofile_limit ≥ max_open_shards × 12 + 100`.

### 2.3 DuckDB connection pooling

```hjson
pool_size:               4
r2d2_thread_pool_size:   3
```

| Field                      | Type    | Default | Required |
|----------------------------|---------|---------|----------|
| `pool_size`                | integer | 4       | no       |
| `r2d2_thread_pool_size`    | integer | 3       | no       |

- **`pool_size`** — concurrent DuckDB connections **per shard**.
  Each open shard holds its own pool, so total connections =
  `pool_size × open_shards × N_stores` (where N_stores is the
  number of DuckDB files in the shard).

  Raise on high-concurrency ingest (lots of parallel `v2/add`
  callers); lower to reduce memory.

- **`r2d2_thread_pool_size`** — size of the process-wide r2d2
  maintenance thread pool.  One pool shared across every r2d2-
  backed connection pool in the process.  Almost never needs
  tuning; bump only if you see r2d2 reaper lag under very high
  connection churn (visible as slow `take_one` in profiles).

### 2.4 Embedding model

```hjson
embedding_model:     "AllMiniLML6V2"
embedding_cache_dir: "/var/lib/bdslib/models"
```

| Field                  | Type             | Default              | Required |
|------------------------|------------------|----------------------|----------|
| `embedding_model`      | string           | `"AllMiniLML6V2"`    | no       |
| `embedding_cache_dir`  | string           | fastembed default    | no       |

**fastembed** loads the named model variant; matches Rust's `Debug`
form of `fastembed::EmbeddingModel`, case-insensitive.

| Model                          | Dim  | Size       | Notes                                |
|--------------------------------|-----:|-----------:|--------------------------------------|
| `AllMiniLML6V2`                | 384  | ~22 MB     | Default — fast / small               |
| `AllMiniLML6V2Q`               | 384  | ~6 MB      | Quantized AllMiniLM                  |
| `BGESmallENV15`                | 384  | ~33 MB     | Higher retrieval quality             |
| `BGEBaseENV15`                 | 768  | ~110 MB    | Larger model                         |
| `BGELargeENV15`                | 1024 | ~340 MB    | Best quality, slowest                |
| `MultilingualE5Small`          | 384  | —          | Multilingual                         |
| `NomicEmbedTextV15`            | 768  | —          | Longer input context                 |
| `JinaEmbeddingsV2BaseEN`       | 768  | —          | 8K context window                    |

⚠ **Dimension lock-in.**  The HNSW vector index dimension is
fixed at the dimension of whichever model produced the first
vector insert under `dbpath`.  **Switching `embedding_model` on
an existing dbpath breaks vector search.**  To switch:

```bash
bdsnode --new --config bds.hjson
```

The active model is reported in `v2/status.embedding_model` and
on the bdsweb Dashboard so you can confirm what's loaded.

- **`embedding_cache_dir`** — overrides fastembed's default
  download cache (`~/.cache/huggingface/hub` or `$HF_HOME`).
  Useful when you want the ~30–340 MB of model weights to live
  next to the data so backups are self-contained, or when running
  in a read-only `$HOME`.

### 2.5 Deduplication

```hjson
similarity_threshold: 0.85
```

| Field                  | Type  | Default | Required |
|------------------------|-------|---------|----------|
| `similarity_threshold` | float | 0.85    | no       |

Cosine-similarity cutoff for classifying an incoming document as a
**secondary** (near-duplicate of an existing primary) vs a
**primary** (new record).  Range `[0.0, 1.0]`:

- `1.0` — only bit-exact duplicates collapse.
- `0.85` — default; tolerates whitespace, ordering, minor field
  variations.
- `0.0` — never dedup; every record is a primary.

The classification happens on every `v2/add`; the secondary is
attached to the matching primary's UUID via the redb-backed
dedup index in `ObservabilityStorage`.  See
[`Documentation/OBSERVABILITYENGINE.md`](OBSERVABILITYENGINE.md).

Higher → more primaries (less dedup, more storage, but no false
joins).  Lower → more secondaries (aggressive dedup, but risks
collapsing semantically-distinct records).

### 2.6 Drain3 log-template mining

```hjson
drain_enabled:       true
drain_load_duration: "24h"
```

| Field                  | Type             | Default | Required |
|------------------------|------------------|---------|----------|
| `drain_enabled`        | bool             | false   | no       |
| `drain_load_duration`  | humantime string | `"24h"` | no       |

When `drain_enabled = true`, every `v2/add` and `v2/add.batch`
runs the [drain3](https://github.com/IBM/Drain3) prefix-tree log
template miner over the record body, clustering similar log lines
into templates stored in the per-shard tplstorage.  Templates are
queryable via `v2/tpl.list`, `v2/tpl.search`, etc.

`drain_load_duration` is how far back the parser looks at startup
to rehydrate previously discovered templates — without this, the
miner starts from scratch on every restart and may produce
different template UUIDs for the same patterns.

Disable on hot-ingest deployments that don't need template mining
(measurable CPU cost per record).

### 2.7 JSON record cache

```hjson
jsoncache_capacity: 10000
jsoncache_ttl_secs: 300
```

| Field                 | Type    | Default | Required |
|-----------------------|---------|---------|----------|
| `jsoncache_capacity`  | integer | 10000   | no       |
| `jsoncache_ttl_secs`  | integer | 300     | no       |

Process-wide LRU cache of recent records, consulted before any
DuckDB round-trip on FTS/vector hits.  `0` capacity disables it
entirely.

Reported in `v2/status` (`jsoncache_pct`, `jsoncache_len`,
`jsoncache_capacity`) and on the bdsweb Dashboard.  TTL eviction
runs lazily on access + via a background sweeper every 60 s.

---

## 3. Ingest tuning

Three ingest paths, each with its own batch/timeout pair.  All
share `ingest_channel_capacity` for back-pressure.

```hjson
// Channel back-pressure (all three paths)
ingest_channel_capacity: 100000

// v2/add — single-record + small-batch ingest
pipe_batch_size:  500
pipe_timeout_ms:  500

// v2/add.file — newline-delimited JSON file ingest
file_batch_size:  500
file_timeout_ms:  5000

// v2/add.file.syslog — RFC 3164 syslog ingest
syslog_batch_size: 500
syslog_timeout_ms: 5000
```

| Field                      | Type    | Default  | Required |
|----------------------------|---------|----------|----------|
| `ingest_channel_capacity`  | integer | 100000   | no       |
| `pipe_batch_size`          | integer | 500      | no       |
| `pipe_timeout_ms`          | integer | 500      | no       |
| `file_batch_size`          | integer | 100      | no       |
| `file_timeout_ms`          | integer | 5000     | no       |
| `syslog_batch_size`        | integer | 100      | no       |
| `syslog_timeout_ms`        | integer | 5000     | no       |

**Common semantics** — each path's worker drains its tokio mpsc
channel, accumulating into a batch until either:
- the batch hits `*_batch_size` records → flush, OR
- the channel is silent for `*_timeout_ms` → flush whatever is
  buffered.

Tuning:

| Symptom                                | Action                                                      |
|----------------------------------------|-------------------------------------------------------------|
| `v2/add` returns `-32099 channel overloaded` | Producer is faster than the consumer.  Either:        |
|                                        | • raise `ingest_channel_capacity` (more memory)             |
|                                        | • raise `pool_size` (faster ingest)                         |
|                                        | • have the client back off + retry                          |
| High per-record ONNX embedding overhead | Raise `*_batch_size` (amortize across records)             |
| Single-record latency visibly bad       | Lower `pipe_timeout_ms` (flush sooner)                     |
| ONNX runtime memory growth              | Lower `*_batch_size` (smaller per-call working set)        |

⚠ **`ingest_channel_capacity: 0`** is the legacy unbounded
behaviour — the channel will grow until the kernel kills you with
OOM.  Set it explicitly unless you have an upstream rate limiter.

---

## 4. BUND VM runtime

```hjson
n_workers:        4
bund_ttl_secs:    300

// Optional — defaults to "nothing disabled".  See § 4.1.
bund: {
  disabled_categories: ["os_shell", "process_control"]
  disabled_words:      ["cls.script.add", "cls.script.delete"]
}
```

| Field            | Type    | Default | Required |
|------------------|---------|---------|----------|
| `n_workers`      | integer | 4       | no       |
| `bund_ttl_secs`  | integer | 300     | no       |
| `bund.disabled_categories` | string list | `[]` | no |
| `bund.disabled_words`      | string list | `[]` | no |

- **`n_workers`** — threads in the process-wide `BundWorkerPool`.
  Each worker runs an ephemeral BUND VM per job submitted via
  `v2/eval.queued`.  Floor 1.  Raise for parallel script
  execution throughput.

- **`bund_ttl_secs`** — time-to-idle for stateful BUND VM
  contexts (the long-lived ones backed by `v2/eval`'s `context`
  field).  After this much inactivity the VM is dropped; the
  next call rebuilds it from scratch.  Swept by the
  `vm_cleanup_interval_secs` background task (§ 5.4).

### 4.1 BUND word sandbox

Every BUND word that touches the host (shell, filesystem, process
lifecycle, cluster writes, …) is classified into one of seven
**risk categories**.  By default **nothing is disabled** — every
category is enabled so existing scheduled scripts and dev runs
keep working without config edits.  Operators who expose the VM
to less-trusted callers (chat snippets via `llm.chat.bund`,
public Bund playground UI, untrusted tenants) opt out of one or
more categories.

The sandbox is applied **every time a Bund VM is initialised**
(the global Adam instance, per-context VMs, ephemeral
worker-pool VMs, chat-snippet VMs).  Disabled words are still
registered in the VM, but invoking them returns:

> `BUND word disabled by bdsnode policy. Edit bund.disabled_categories / bund.disabled_words in bds.hjson (or check startup logs to see which words are currently denied).`

bdsnode logs the active sandbox at startup so operators can
audit it without running a probe:

```
[bund::policy] category os_shell DISABLED (arbitrary shell command execution): 2 word(s) blocked: system.shell, system.shell.
[bund::policy] category process_control DISABLED (kills bdsnode (`bund.exit`) or blocks workers (`sleep.seconds`)): 2 word(s) blocked: bund.exit, sleep.seconds
```

#### Category reference

| Key | Words blocked | Why dangerous |
|---|---|---|
| `os_shell` | `system.shell`, `system.shell.` | Arbitrary shell command execution as the bdsnode user → RCE. |
| `process_control` | `bund.exit`, `sleep.seconds` | `bund.exit` calls `process::exit()` and kills the entire bdsnode process. `sleep.seconds` ties up a worker thread for any duration. |
| `filesystem_write` | `file.write[.]`, `fs.cp`, `fs.mv`, `fs.rm` | Arbitrary-path filesystem modification. |
| `filesystem_read` | `file[.]`, `url[.]`, `fs.ls[.]`, `fs.ls.dir[.]`, `fs.cwd`, `fs.is_file`, `fs_is_file.`, `bund.eval-file[.]`, `filename[.]` | Filesystem layout disclosure, SSRF (`url`), eval-of-file. `filename` canonicalises and therefore touches the FS. |
| `code_eval` | `bund.eval[.]`, `compile`, `apply`, `use[.]` | Recursive code execution; `use` loads files. |
| `cluster_admin` | `cls.add[.]`, `cls.add.batch[.]`, `cls.update[.]`, `cls.delete[.]`, `cls.doc.{add,add.file,update.content,update.metadata,delete,reindex,sync}[.]`, `cls.tpl.{add,update.body,update.metadata,delete,reindex}[.]`, `cls.signal.{emit,update}[.]`, `cls.script.{add,update,delete}[.]` | Cluster-replicated writes. **`cls.script.*` installs persistent cron jobs on every peer** — by far the highest blast radius in this category. |
| `local_db_write` | `db.add[.]`, `db.sync`, `doc.{add,add.file,add.vec,delete,update.content,update.metadata,store.content.vec,store.meta.vec,reindex}[.]`, `doc.sync` | Local-only DB writes that bypass cluster replication — fine for maintenance scripts, dangerous if a chat user invokes them. |

The `.` suffix denotes the workbench variant of the same word; both forms are gated together.  The `[.]` notation in the table is shorthand for "this word and its workbench variant".

Pure-string path words (`system.path.split`, `system.path.filename`) are **NOT gated** — they don't touch the host.

#### Recommended profiles

| Scenario | Recommended `disabled_categories` |
|---|---|
| Single-operator dev box | `[]` (default) |
| Production node, only operators run scripts | `["os_shell", "process_control"]` |
| Chat snippet eval enabled (`llm.chat.bund.enabled=true`) | `["os_shell", "process_control", "filesystem_write", "code_eval", "cluster_admin", "local_db_write"]` — leave only `filesystem_read` for read-only forensics |
| Public-facing Bund playground / multi-tenant | All seven categories disabled; rely on `db.search.*` / `cls.search.*` / `cls.aggregation` for read-only queries |

The per-word `disabled_words` list is layered on top of `disabled_categories`.  Use it to keep a category mostly enabled but block specific high-risk words — for example, enable `cluster_admin` but block only the script-persistence triplet:

```hjson
bund: {
  disabled_words: ["cls.script.add", "cls.script.update", "cls.script.delete"]
}
```

Category names accept short aliases for ergonomics — `shell` → `os_shell`, `fs_write` → `filesystem_write`, `fs_read` → `filesystem_read`, `eval` → `code_eval`, `cluster_write` → `cluster_admin`, `db_write` → `local_db_write`, `process` → `process_control`. Unknown values are logged at WARN and ignored; bdsnode starts up partially-sandboxed rather than refusing to run.

---

## 5. Background tasks

Each task is spawned as a separate tokio task in `bdsnode::main`
and gets its own `Handle` so shutdown is clean.

### 5.1 Cron-driven script scheduler

```hjson
scheduler_interval_secs: 60
```

| Field                       | Type    | Default | Required |
|-----------------------------|---------|---------|----------|
| `scheduler_interval_secs`   | integer | 60      | no       |

Tick cadence for the BUND-script scheduler.  Once per tick it
walks every script in the registry (`v2/scripts`), parses its
metadata `schedule` field as a 5-field crontab via `croner`, and
submits any script whose next occurrence falls within the current
minute.

| Value | Behaviour                                                     |
|------:|----------------------------------------------------------------|
| `0`   | Scheduler disabled — stored scripts never auto-fire           |
| `60`  | Default — matches standard crontab "once per minute" semantics |
| `< 60`| `* * * * *` cron will fire multiple times per minute           |

Cluster-aware: in cluster mode, the per-node `scheduler_log` +
`v2/scheduler.last_seen` peer fan-out dedups so each scheduled
minute fires on exactly one node (see § 6.5).

### 5.2 Periodic global sync

```hjson
sync_interval_secs: 60
```

| Field                  | Type    | Default | Required |
|------------------------|---------|---------|----------|
| `sync_interval_secs`   | integer | 60      | no       |

How often `bdslib::sync_db()` runs — iterates every open shard
and runs:
- DuckDB `CHECKPOINT` (flush WAL)
- Tantivy `commit` (publish FTS writes)
- VecStore flush
- tplstorage HNSW save

Without this, sync only happens on LRU shard eviction and at
graceful shutdown.  **An unclean exit (kill -9 / OOM / hardware
fault) loses every write since the active shard's last natural
sync.**

| Value | Behaviour                                                  |
|------:|-------------------------------------------------------------|
| `0`   | Disabled — fast, but unclean exits lose hours of data       |
| `60`  | Default — sub-second per tick on warm WALs                  |
| Lower | More frequent sync, slight overhead, smaller exit-loss window |

### 5.3 Result-queue sweeper

```hjson
results_ttl_secs:    600
results_sweep_secs:   30
```

| Field                | Type    | Default | Required |
|----------------------|---------|---------|----------|
| `results_ttl_secs`   | integer | 600     | no       |
| `results_sweep_secs` | integer | 30      | no       |

Controls the per-id `ResultQueue` family exposed via
`v2/results.{len,push,pull,empty}`.  These queues back
`v2/eval.queued`'s async results and the LLM async runner's
delivery channel.

- **`results_ttl_secs`** — age (in seconds since queue creation)
  above which a queue is evicted.  `0` keeps queues forever.
- **`results_sweep_secs`** — interval between sweep passes.
  Ignored when `results_ttl_secs = 0`.

A long-running script that takes longer than `results_ttl_secs`
to be claimed will have its result silently dropped.  Match the
TTL to the longest expected poll latency for `v2/results.pull`.

### 5.4 BUND VM cleanup

```hjson
vm_cleanup_interval_secs: 60
```

| Field                       | Type    | Default | Required |
|-----------------------------|---------|---------|----------|
| `vm_cleanup_interval_secs`  | integer | 60      | no       |

Scan interval for the BUND VM idle sweeper.  On each tick,
evicts every named VM context whose idle time exceeds
`bund_ttl_secs` (§ 4).  Pair the two values — sweep interval
should be ≤ the TTL so a stale VM doesn't linger past its
deadline by more than one tick.

---

## 6. `cluster:` block

```hjson
cluster: {
  enabled:                 true
  shared_secret:           "at-least-16-chars-of-shared-secret"
  bind_url:                "http://10.0.0.5:9000"
  bootstrap:               "http://10.0.0.6:9000"

  gossip_interval:         "5s"
  suspect_timeout:         "30s"
  dead_timeout:            "120s"
  peer_rpc_timeout:        "2s"

  full_mode_threshold:     3
  replication_factor:      3
  full_replication_stores: ["docs","signals","scripts","users","llm_cache"]

  hint_replay_interval:    "10s"
  hint_max_age:            "24h"
  antientropy_interval:    "300s"
  max_fingerprints_per_peer: 100000

  floating_bootstrap:      true
  bootstrap_retry_interval: "60s"

  scheduler_dedup_window:  "300s"

  session_ttl:                "8h"
  auth_rate_limit_per_minute: 10
}
```

The block is optional.  Absent or `enabled: false` → bdsnode runs
**standalone**.  Architecture deep-dive: [`CLUSTER.md`](CLUSTER.md)
and [`CLUSTER_DETAILS.md`](CLUSTER_DETAILS.md).

### 6.1 Membership

| Field                | Type             | Default | Required when enabled |
|----------------------|------------------|---------|-----------------------|
| `enabled`            | bool             | `false` | no                    |
| `shared_secret`      | string           | —       | **yes** (≥ 16 chars)  |
| `bind_url`           | string           | —       | **yes**               |
| `bootstrap`          | string           | none    | no (bootstrap only)   |

- **`enabled`** — master switch.  When `false`, all `v3/*` and
  `v4/*` methods return `-32097 cluster mode disabled`.
- **`shared_secret`** — HMAC-SHA256 key for `v3/cluster.*` gossip,
  every `v3/*` admin RPC, every `v4/llm.*` call, and bdsweb's
  session-cookie HMAC.  **Must be ≥ 16 chars** (config validation
  refuses shorter values).  Rotation is destructive — every
  existing peer table + session cookie becomes invalid.
- **`bind_url`** — the URL **other peers** use to reach this node.
  Must match `--host`/`--port` on the bdsnode CLI.  Wrong here =
  silent gossip failures (peers think you're down).
- **`bootstrap`** — URL of one known peer to hello on startup.
  Omit on the first node (which IS the bootstrap target) or in
  setups where you want strict isolation.  See `floating_bootstrap`
  for the multi-target fallback.

### 6.2 Gossip cadence

```hjson
gossip_interval:    "5s"
suspect_timeout:    "30s"
dead_timeout:       "120s"
peer_rpc_timeout:   "2s"
```

| Field                | Type             | Default |
|----------------------|------------------|---------|
| `gossip_interval`    | humantime string | `"5s"`  |
| `suspect_timeout`    | humantime string | `"30s"` |
| `dead_timeout`       | humantime string | `"120s"`|
| `peer_rpc_timeout`   | humantime string | `"2s"`  |

- **`gossip_interval`** — how often this node pings **every**
  Alive peer (parallel fan-out).  Lower = faster failure detection
  + more network chatter.
- **`suspect_timeout`** — peer is marked Suspect when its
  `last_seen` ages past this value.  Must be `>= 2 × gossip_interval`
  to avoid flapping during transient packet loss.
- **`dead_timeout`** — Suspect → Dead transition.  Should be
  `>= 3 × suspect_timeout`.
- **`peer_rpc_timeout`** — per-call deadline for any v2/v3/v4
  peer-to-peer RPC (gossip pings, replication writes, anti-entropy
  pulls, fan-out reads).  Tight values fail fast; loose values
  tolerate slow peers but tie up tokio tasks.

**Relationship**: `peer_rpc_timeout < gossip_interval` is the
right shape — each tick must complete its ping fan-out before the
next one starts.

### 6.3 Replication

```hjson
full_mode_threshold:     3
replication_factor:      3
full_replication_stores: ["docs","signals","scripts","users","llm_cache"]
```

| Field                       | Type           | Default                                                  |
|-----------------------------|----------------|----------------------------------------------------------|
| `full_mode_threshold`       | integer        | `3`                                                      |
| `replication_factor`        | integer        | `3`                                                      |
| `full_replication_stores`   | list of string | `["docs","signals","scripts","users","llm_cache"]`       |

- **`full_mode_threshold`** — minimum Alive peer count (including
  this node) to enter `full` mode.  Below this, the cluster is in
  `partial` mode and the dashboard shows a banner.  Replication
  proceeds in both modes; this is purely an operator signal.
- **`replication_factor`** — used by **sharded** writes
  (`v3/add`, `v3/add.batch`):  local commit + (`replication_factor − 1`)
  random Alive peers.  Failures land in the hint queue.
  Fully-replicated stores (the next field) **ignore this** — they
  always write to every Alive peer.
- **`full_replication_stores`** — list of store names anti-entropy
  sweeps + that `v3/doc.*` / `v3/signal.emit` / `v3/script.*` /
  `v3/user.*` / `v4/llm.*` coordinators replicate fully.  The
  library default already includes all five canonical stores;
  setups that override this list must include **everything they
  want replicated**, or anti-entropy won't pull missing rows
  for the omitted stores.

  **Common pitfall**: test configs that override the list to
  `["docs", "signals", "scripts"]` will silently drop
  `users` + `llm_cache` from anti-entropy.

### 6.4 Hinted handoff + anti-entropy

```hjson
hint_replay_interval:     "10s"
hint_max_age:             "24h"
antientropy_interval:     "300s"
max_fingerprints_per_peer: 100000
```

| Field                       | Type             | Default  |
|-----------------------------|------------------|----------|
| `hint_replay_interval`      | humantime string | `"10s"`  |
| `hint_max_age`              | humantime string | `"24h"`  |
| `antientropy_interval`      | humantime string | `"300s"` |
| `max_fingerprints_per_peer` | integer          | `100000` |

- **`hint_replay_interval`** — how often the hint replay loop runs.
  Each pass picks up queued hints (writes that failed to fan out
  to a Dead/Suspect peer at the time) and retries them.
- **`hint_max_age`** — hints older than this are dropped.
  Defends against unbounded queue growth when a peer is gone
  forever.  Anti-entropy backfills anything missed past the hint
  window.
- **`antientropy_interval`** — how often the AE pull loop runs
  per peer.  Each pass walks every store name in
  `full_replication_stores`, fetches the peer's `v2/<store>.list_ids`,
  diffs against local, and pulls anything missing.
- **`max_fingerprints_per_peer`** — cap on the `(uuid, fingerprint)`
  pairs returned by `v2/fingerprints.recent` (the input source for
  cluster-wide k-NN / anomaly / denoise analytics).  Cap exists so
  a single peer with a huge active shard doesn't bloat the
  fan-out body.

### 6.5 Scheduler dedup

```hjson
scheduler_dedup_window: "300s"
```

| Field                    | Type             | Default  |
|--------------------------|------------------|----------|
| `scheduler_dedup_window` | humantime string | `"300s"` |

Cross-cluster dedup window for the BUND-script scheduler.  When
a node's local tick decides to fire a script, it fans
`v2/scheduler.last_seen` to every Alive peer first; if **any**
node (this one or any peer) executed the same script within
`scheduler_dedup_window`, the fire is suppressed.

**Relationship**: must be `≥ scheduler_interval_secs` (default 60s).
Setting it lower would let two nodes both fire when their tick
alignments differ by a second or two.  See
[`CLUSTER_DETAILS.md`](CLUSTER_DETAILS.md) § 5.

### 6.6 Authentication

```hjson
session_ttl:                "8h"
auth_rate_limit_per_minute: 10
```

| Field                          | Type             | Default |
|--------------------------------|------------------|---------|
| `session_ttl`                  | humantime string | `"8h"`  |
| `auth_rate_limit_per_minute`   | integer          | `10`    |

- **`session_ttl`** — lifetime of HMAC-signed session tokens
  issued by `v3/user.authenticate` and stored in bdsweb's
  `bds_session` cookie.  No server-side revocation — token leaks
  are mitigated by short TTL + password rotation.
- **`auth_rate_limit_per_minute`** — per-username sliding-window
  cap on `v3/user.authenticate` attempts.  `0` disables the
  per-user limit (bdsweb still applies a per-IP limit via
  `tower_governor` on `POST /login` based on this value).
  Cooperates with the per-IP `/login` limiter — both must pass.

---

## 7. `llm:` block

```hjson
llm: {
  default: "ollama"

  providers: {
    ollama: {
      url:           "http://127.0.0.1:11434"
      default_model: "llama3.2"
    }
    anthropic: {
      base_url:      "https://api.anthropic.com"
      api_key_env:   "ANTHROPIC_API_KEY"
      default_model: "claude-sonnet-4-5"
    }
    openai: {
      base_url:      "https://api.openai.com"
      api_key_env:   "OPENAI_API_KEY"
      default_model: "gpt-4o-mini"
    }
    deepseek: {
      base_url:      "https://api.deepseek.com"
      api_key_env:   "DEEPSEEK_API_KEY"
      // api_key:    "sk-…"            // optional hjson fallback
      default_model: "deepseek-chat"
    }
  }

  cache: { enabled: true, ttl_secs: 86400 }
  dedup: { enabled: true, window_secs: 300, wait_max_secs: 30 }
  runner:{ enabled: true, poll_interval_secs: 1, max_concurrency: 2 }
}
```

Architecture deep-dive: [`LLM.md`](LLM.md).  The block is
optional; absent → no LLM providers registered and `v4/llm.*`
returns `no providers registered`.

### 7.1 Provider registration

| Path                                      | Default                          | Required |
|-------------------------------------------|----------------------------------|----------|
| `llm.default`                             | first registered                 | no       |
| `llm.providers.ollama.url`                | `"http://localhost:11434"`       | no       |
| `llm.providers.ollama.default_model`      | `"llama3.2"`                     | no       |
| `llm.providers.anthropic.base_url`        | `"https://api.anthropic.com"`    | no       |
| `llm.providers.anthropic.api_key_env`     | `"ANTHROPIC_API_KEY"`            | no       |
| `llm.providers.anthropic.default_model`   | `"claude-sonnet-4-5"`            | no       |
| `llm.providers.openai.base_url`           | `"https://api.openai.com"`       | no       |
| `llm.providers.openai.api_key_env`        | `"OPENAI_API_KEY"`               | no       |
| `llm.providers.openai.default_model`      | `"gpt-4o-mini"`                  | no       |
| `llm.providers.deepseek.base_url`         | `"https://api.deepseek.com"`     | no       |
| `llm.providers.deepseek.api_key_env`      | `"DEEPSEEK_API_KEY"`             | no       |
| `llm.providers.deepseek.api_key`          | `""` (no fallback)               | no       |
| `llm.providers.deepseek.default_model`    | `"deepseek-chat"`                | no       |

- **`api_key_env`** names the **environment variable** holding the
  API key.  For `anthropic` / `openai` this is the *only* source —
  an unset env var causes that provider to be logged-and-skipped at
  startup, not a fatal error.  Never put the key itself in
  `bds.hjson` for those two.
- **DeepSeek** is the exception: the key is resolved as **env var
  first, then hjson `api_key` fallback**.  If `$DEEPSEEK_API_KEY`
  is set and non-empty it wins; otherwise bdsnode reads the
  plaintext `api_key` field.  Both unset → skip the provider.  This
  asymmetry exists so deployments that can't easily set env vars
  (e.g. systemd units behind operator-only access) can still ship
  the key in hjson, while operators who prefer the env-only model
  just leave `api_key` out and behaviour matches the other
  providers.  The chosen source is logged at startup:
  `[llm] registered provider 'deepseek' model=… (key from $DEEPSEEK_API_KEY)`
  or
  `(key from bds.hjson:llm.providers.deepseek.api_key)`.
- **DeepSeek capabilities**: chat completions only — no embeddings.
  Models: `deepseek-chat` (default) or `deepseek-reasoner` (chain
  of thought).  Wire format is OpenAI-compatible.
- **`llm.default`** — provider name used when a v4/llm.* request
  omits `provider`.  When unset, the first successfully registered
  provider wins.  Misconfigured default (name doesn't match any
  registered provider) silently falls back to the first
  registered.
- **`llm.default`** — provider name used when a v4/llm.* request
  omits `provider`.  When unset, the first successfully registered
  provider wins.  Misconfigured default (name doesn't match any
  registered provider) silently falls back to the first
  registered.

### 7.2 `llm.cache`

| Field                | Type    | Default | Required |
|----------------------|---------|---------|----------|
| `llm.cache.enabled`  | bool    | `true`  | no       |
| `llm.cache.ttl_secs` | integer | `86400` | no       |

Controls the replicated inference cache at
`<dbpath>/llm/cache.duckdb`.

- **`enabled: false`** — cache manager not registered; every
  `v4/llm.{complete,analyze}` call goes straight to the provider
  with `cache: "disabled"` in the response.
- **`ttl_secs`** — rows expire `ttl_secs` after creation.  `0`
  means never expires (rely on `v4/llm.cache.purge` for cleanup).

Per-call opt-out via `cache: false` in the request body.
`temperature > 0` automatically skips caching (`disabled:temperature`).

### 7.3 `llm.dedup`

| Field                     | Type    | Default | Required |
|---------------------------|---------|---------|----------|
| `llm.dedup.enabled`       | bool    | `true`  | no       |
| `llm.dedup.window_secs`   | integer | `300`   | no       |
| `llm.dedup.wait_max_secs` | integer | `30`    | no       |

Controls the cluster-wide single-execution lease via
`<dbpath>/network/inference_log.duckdb` + `v2/llm.last_executed`
fan-out.

- **`window_secs`** — how long a recent `done` / `failed` row
  keeps short-circuiting fresh requests for the same `cache_key`.
  Pair with `cache.ttl_secs` (a hit within this window means the
  cache should have the answer too).
- **`wait_max_secs`** — sync caller poll budget when a peer is
  mid-flight on the same `cache_key`.  `0` = fail-fast (don't
  wait, just run anyway).

Standalone mode (no cluster) means `dedup` falls through to
`disabled` regardless of this setting — there's no peer to dedup
against.

### 7.4 `llm.runner`

| Field                            | Type    | Default | Required |
|----------------------------------|---------|---------|----------|
| `llm.runner.enabled`             | bool    | `true`  | no       |
| `llm.runner.poll_interval_secs`  | integer | `1`     | no       |
| `llm.runner.max_concurrency`     | integer | `2`     | no       |

Controls the per-node background runner that drains the async
job queue (`<dbpath>/llm/jobs.duckdb`).

- **`enabled: false`** — `v4/llm.*_async` jobs sit in `pending`
  forever (callers can still inspect / cancel them via
  `v4/llm.jobs.*`).
- **`max_concurrency`** — simultaneous in-flight inferences.
  Bound the kind of model you're using: large local models on a
  single GPU may need `1`.
- **`poll_interval_secs`** — sleep between claim sweeps when
  idle.  Lower = tighter latency from submit to start; higher =
  fewer wakeups.

---

## 8. bdsweb-specific keys

These are read **only by bdsweb**; bdsnode ignores them.

```hjson
dashboard_refresh_secs: 30
cluster_refresh_secs:   10
```

| Field                       | Type    | Default | Floor |
|-----------------------------|---------|---------|-------|
| `dashboard_refresh_secs`    | integer | 30      | 1     |
| `cluster_refresh_secs`      | integer | 10      | 1     |

Each spawns its own background tokio task in `bdsweb::main` that
polls bdsnode at the configured cadence and parks the snapshot in
`state.{dashboard,cluster}_cache`.  The corresponding page
(`/` and `/cluster`) renders from cache; a **Reload** button on
each page forces a live fetch through `/<page>/refresh`.

Tuning: lower for tighter UI responsiveness (cost: more RPC traffic
to bdsnode); raise on RPC-saturated clusters where stale UI is
acceptable.  See [`CLUSTER_DETAILS.md`](CLUSTER_DETAILS.md) § 10.1.

### 8.1 `web.analyze.*` — "Analyze this!" buttons

bdsweb's analysis pages each have an **Analyze this!** button that
hands the current result set to the default LLM via `v4/llm.analyze`
and renders the verdict in a floating side-pane.  Each target gets
its own sub-block under `web.analyze.<target>` so future targets
(metrics, rca, …) slot in alongside `logs` without re-shuffling the
schema.

Every target accepts the same three keys:

| Field             | Type    | Default                              | Floor / Range |
|-------------------|---------|--------------------------------------|---------------|
| `timeout_secs`    | integer | 600                                  | floor 30      |
| `max_rows`        | integer | 50                                   | 1 – 500       |
| `prompt_template` | string  | per-target compiled-in default       | —             |

#### `web.analyze.logs` (Telemetry → Logs page)

```hjson
web: {
  analyze: {
    logs: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing a slice of operational log records …
        '''
    }
  }
}
```

Default prompt — 5-step "SRE reading a log slice" frame: dominant theme, recurring failures, anomalies, root cause, next step.

#### `web.analyze.metrics` (Telemetry → Metrics page)

```hjson
web: {
  analyze: {
    metrics: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing a slice of numeric telemetry records …
        '''
    }
  }
}
```

Default prompt — 6-step *numeric* frame: metric description + typical ranges, per-key min/max/median + outliers, trend direction with bracketing timestamps, cross-metric correlation, operational interpretation (healthy / capacity / failure / noise), next step.  Use this knob to tighten the analysis ("only flag CPU% above 90"), change the audience ("explain like I'm a junior on-call"), or change output format ("return strict JSON with `metric`, `range`, `verdict`").

#### `web.analyze.templates` (Telemetry → Templates page)

```hjson
web: {
  analyze: {
    templates: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing a slice of drain3-mined log templates …
        '''
    }
  }
}
```

Default prompt — 6-step *pattern* frame designed for drain3 output (each row is a recurring log-line pattern with `<*>` placeholders): system-behavior themes, failure-indicator templates with verbatim citations, benign/high-volume templates the operator can tune out, suspicious wildcards (drain3 over-collapsing different value classes), most-likely incident, next step.  Operators rewriting it should preserve the "quote bodies verbatim" instruction — that's what lets the SRE grep / drill down from the answer.  Works in both browse-recent mode (empty query, lists the last `duration` of templates) and search mode (vector search over the tpl store).

#### `web.analyze.agg_search` (Analysis → Agg. Search page)

```hjson
web: {
  analyze: {
    agg_search: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing the output of an aggregated search …
        '''
    }
  }
}
```

Default prompt — 6-step *cross-corpus* frame designed for `v?/aggregationsearch` output, which returns **two correlated corpora** in parallel: live telemetry rows and matched operational documents (runbooks, postmortems, design notes).  The prompt forces the model to cross-reference the two sets rather than producing two unrelated summaries: live-telemetry signal, document-knowledge surface, **explicit cross-reference**, coherent story, gaps in the evidence, next-step preferably authorised by a runbook.  Each row handed to the LLM carries a synthetic `_kind=telemetry` or `_kind=document` field so the model can tell which corpus the evidence came from inside the prompt; documents are clipped to ~800 chars each so one large runbook can't crowd out telemetry rows.  `max_rows` caps the *total* row count handed to the LLM; up to half the budget is reserved for documents so a query with many telemetry hits can't drop the doc rows.

#### `web.analyze.templates_summary` (Analysis → Templates Summary page)

```hjson
web: {
  analyze: {
    templates_summary: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing two complementary derived views …
        '''
    }
  }
}
```

Default prompt — 6-step *story-from-summary* frame.  This page is structurally different from the others: instead of analysing raw rows, the LLM is handed a single TextRank summary (`v?/textrank.templates`) **plus** the LDA-discovered topic keywords (`v?/topics.all`), and asked to weave both into a coherent narrative: headline, themes anchored to keywords, trouble signals quoted verbatim from the summary, healthy noise to tune out, most-likely incident citing *both* the summary AND the keywords, next investigative step.  Each payload entry carries a synthetic `_kind=textrank_summary` (always exactly one row) or `_kind=topic_keywords` (one row per log key, carrying the LDA top-N words).  `max_rows` caps only the topic rows — the summary always gets through.  Use this knob to rewrite the prompt for a different storytelling angle ("narrate as an exec summary", "focus only on the security keywords", "respond in markdown table form").

#### `web.analyze.primary_summary` (Analysis → Primary Summary page)

```hjson
web: {
  analyze: {
    primary_summary: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing a TextRank-PageRank summary …
        '''
    }
  }
}
```

Default prompt — 6-step *story-from-summary* frame tuned for primary telemetry text bodies (`v?/summary_for_recent`).  Numeric-only records are filtered out upstream, so the summary reaches the LLM as the system's text-emitted operational language — warnings, status lines, error messages, audit notes.  The prompt asks the model to **interpret** the summary, not re-summarise it: headline anchored with verbatim phrasing, thematic clusters, signals of trouble quoted verbatim, healthy chatter to tune out, most-likely incident, next step.  The supplied payload is always exactly one `_kind=primary_summary` row carrying the summary text plus the operator's TextRank knobs (`max_sentences`, `min_word_len`) — `max_rows` doesn't gate anything here, it's retained for schema parity.

#### `web.analyze.primary_query_summary` (Analysis → Primary Query Summary page)

```hjson
web: {
  analyze: {
    primary_query_summary: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing a TextRank-PageRank summary that distills …
        '''
    }
  }
}
```

Default prompt — 5-step *answer-the-question* frame.  This page is unique in being query-driven: the operator already asked a specific question (semantic vector search), the records summarised are those matching the question, and the LLM's job is to **answer the operator** using the summary as evidence — not to tell a general story.  Steps: direct answer with verbatim anchoring, supporting evidence (2–4 quoted sentences), signals of trouble within the query scope, **caveats** for thin / off-topic summaries (so operators stop chasing weak retrieval matches), next investigative step.  The supplied payload is always exactly one `_kind=primary_query_summary` row carrying the query, the summary, and the TextRank knobs; the query is also passed to `v4/llm.analyze` separately so the inference cache key is query-aware.  The prompt explicitly tells the model to *refuse to force an answer* when the summary doesn't actually speak to the question — important for keeping operators out of dead ends.

#### `web.analyze.primary_lsa_summary` (Analysis → Primary LSA Summary page)

```hjson
web: {
  analyze: {
    primary_lsa_summary: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing an LSA (Latent Semantic Analysis) summary …
        '''
    }
  }
}
```

Default prompt — 7-step *per-concept* frame.  Unlike TextRank (which picks central sentences via PageRank similarity), LSA decomposes the term-document matrix into `n_concepts` latent dimensions via SVD and picks one sentence per concept — so the summary is **deliberately diverse**, each sentence representing a different topical thread.  The prompt mirrors that structure: headline across all concepts, per-concept breakdown identifying which thread each sentence represents, trouble signals, healthy threads, **cross-concept correlation** (the unique value LSA adds over TextRank — when two concepts together imply one underlying condition), most-likely incident, next step.  The supplied payload is exactly one `_kind=primary_lsa_summary` row carrying the summary, the operator's `n_concepts` choice, and the TextRank knobs.  The prompt also tells the model to *flag noise dimensions* honestly — LSA can over-decompose sparse corpora into meaningless concepts, and pretending each one is signal is worse than ignoring the bad ones.

#### `web.analyze.primary_lsa_query_summary` (Analysis → Primary LSA Query Summary page)

```hjson
web: {
  analyze: {
    primary_lsa_query_summary: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing a query-driven LSA (Latent Semantic Analysis) summary …
        '''
    }
  }
}
```

Default prompt — 6-step *answer-the-question, per-concept* frame.  This target combines the two traits that distinguish each of its siblings:

- **Query-driven**, like `primary_query_summary` — the operator already asked a question via vector search and the records summarised are those matching it.  The LLM must *answer the operator*, not tell a general story.
- **LSA-decomposed**, like `primary_lsa_summary` — the summary has `n_concepts` sentences, each representing a different topical thread inside the query scope.

The prompt blends both: direct answer anchored verbatim, per-concept breakdown tying each thread back to the question (with explicit instructions to flag off-topic concepts honestly), trouble signals within the query scope, **cross-concept correlation** (the LSA value-add for query-driven analysis), caveats for weak retrieval / over-decomposed LSA, next step.  The supplied payload is exactly one `_kind=primary_lsa_query_summary` row carrying query, summary, `n_concepts`, and the TextRank knobs; the query is also passed to `v4/llm.analyze` separately so the inference cache key is query-aware.  Critical guardrail: *"If the summary truly doesn't answer the question, that is a valid answer — say so plainly rather than padding."*

#### `web.analyze.anomaly_recent` (Analysis → Detect anomalies page)

```hjson
web: {
  analyze: {
    anomaly_recent: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing the output of a rarity-based anomaly detector …
        '''
    }
  }
}
```

Default prompt — 6-step *outline-the-nature* frame designed for `v?/anomaly.recent` output.  Unlike the summary targets which receive a single derived blob, this one receives a row list: one synthetic `_kind=anomaly_window_stats` row carrying the population context (`n_logs`, `n_unique_ngrams`, threshold, mean rarity), plus N `_kind=anomaly` rows each with `idx`, `rarity`, the record `text`, and the `novel_ngrams` that drove the rarity score.  The prompt asks the model to **explain** the anomalies, not list them: population framing, themes across anomalies (clustering by key/source/time/n-gram family), severity ranking weighted by operational impact rather than raw rarity, **false-positive candidates** so the operator can tune them out, most-likely incident, next step.  `max_rows` caps the anomaly rows fed to the LLM; the stats row is always included on top of that so the model never loses population context.  Critical guardrail: *"If the anomaly set is dominated by noise … say so plainly rather than forcing a narrative."*

#### `web.analyze.denoise_recent` (Analysis → Denoise page)

```hjson
web: {
  analyze: {
    denoise_recent: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing the output of an n-gram commonness denoiser …
        '''
    }
  }
}
```

Default prompt — 6-step *story-from-signal + filter sanity check* frame.  Unlike `anomaly_recent` (which surfaces *rare* records), the denoiser splits a window into **two correlated corpora**: `kept` records (low commonness — the signal) and `removed` records (high commonness — boilerplate / heartbeats / templated chatter).  The LLM does both halves: it tells the story the kept set describes AND sanity-checks the filter by characterising what got removed.  Steps: population context (kept vs removed split), the signal (story from KEPT, with `[idx]` citations), the noise floor (what got REMOVED, plausibility check), **filter quality** (false positives — real signal in REMOVED; false negatives — boilerplate in KEPT — to guide threshold tuning), most-likely incident from kept, next step.  The supplied payload carries one `_kind=denoise_window_stats` row + rows tagged `_kind=denoise_kept` and `_kind=denoise_removed`.  `max_rows` caps the **total** kept + removed row count with a **60/40 split in favour of kept** (slack from either side redistributes); the stats row always passes through on top.

#### `web.analyze.knn` (Analysis → k-NN page)

```hjson
web: {
  analyze: {
    knn: {
      timeout_secs:    600
      max_rows:        50
      prompt_template:
        '''
        You are reviewing the output of a k-Nearest-Neighbour clustering analysis …
        '''
    }
  }
}
```

Default prompt — 7-step *interpret-the-clustering-structure* frame for `v?/knn` output.  k-NN returns two complementary outputs: **clusters** (groups of records bound by vector similarity, each with a `representative` and a `members` list) and **anomalies** (records whose `max_similarity` to any cluster fell below the threshold — singletons that didn't fit anywhere).  The prompt asks the model to interpret each cluster's operational meaning rather than re-listing them, rank by relevance (a 3-member error cluster usually matters more than a 200-member heartbeat cluster), call out failure clusters separately from routine ones, and assess anomalies as either novel-worth-investigating or just noisy edge cases.  The supplied payload carries one `_kind=knn_window_stats` row + N `_kind=knn_cluster` rows (each with its representative and a clipped members list — 5 verbatim members per cluster, so one 200-member cluster can't crowd out the others) + M `_kind=knn_anomaly` rows.  `max_rows` caps clusters + anomalies combined with a **60/40 split in favour of clusters** (each cluster row carries denser info per row); slack from either side redistributes.

- **`timeout_secs`** — per-request reqwest timeout for bdsweb → bdsnode
  on the analyze call only.  Default 600 s.  CPU-bound local Ollama
  on llama3.2 + 50 supplied rows + auto-bumped `num_ctx` typically
  needs 60–180 s for the first call; cached repeats return in <50 ms.
  Raise on slower hardware or larger prompts; lower to fail fast on
  wedged providers.

- **`max_rows`** — how many search hits bdsweb fetches and forwards
  to the LLM.  Default 50.  Clamped to `[1, 500]` because anything
  more usually overflows the model's context window and produces a
  mush of unrelated logs.

- **`prompt_template`** — operator-supplied instruction text prepended
  to the rows when calling `v4/llm.analyze`.  Use hjson triple-quoted
  multi-line strings (`'''…'''`) for readability.  Each target has its
  own compiled-in default (logs: SRE log frame, metrics: numeric
  frame); rewrite to change analysis style, audience, language, or
  output format.

Missing block or missing keys fall back to the per-target built-in
defaults; operators who don't care about this feature don't need to
edit anything.

Active settings are logged at bdsweb startup:

```
[INFO] web.analyze.logs:                      timeout=600s, max_rows=50, prompt_chars=621
[INFO] web.analyze.metrics:                   timeout=600s, max_rows=50, prompt_chars=863
[INFO] web.analyze.templates:                 timeout=600s, max_rows=50, prompt_chars=1110
[INFO] web.analyze.agg_search:                timeout=600s, max_rows=50, prompt_chars=1457
[INFO] web.analyze.templates_summary:         timeout=600s, max_rows=50, prompt_chars=1620
[INFO] web.analyze.primary_summary:           timeout=600s, max_rows=50, prompt_chars=1495
[INFO] web.analyze.primary_query_summary:     timeout=600s, max_rows=50, prompt_chars=1502
[INFO] web.analyze.primary_lsa_summary:       timeout=600s, max_rows=50, prompt_chars=1758
[INFO] web.analyze.primary_lsa_query_summary: timeout=600s, max_rows=50, prompt_chars=2042
[INFO] web.analyze.anomaly_recent:            timeout=600s, max_rows=50, prompt_chars=2470
[INFO] web.analyze.denoise_recent:            timeout=600s, max_rows=50, prompt_chars=3110
[INFO] web.analyze.knn:                       timeout=600s, max_rows=50, prompt_chars=2870
```

---

## 9. Legacy `v2/chat.ollama` keys

Pre-dates the v4/llm.* surface.  Still consumed by the deprecated
`v2/chat.ollama` RPC; **new deployments should configure the
`llm:` block** (§ 7) and use `v4/llm.chat` instead.

```hjson
ollama_url:           "http://localhost:11434"
ollama_model:         "llama3.2"
ollama_system_prompt: "You are an expert SRE …"
```

| Field                  | Type   | Default                       | Required |
|------------------------|--------|-------------------------------|----------|
| `ollama_url`           | string | `"http://localhost:11434"`    | no       |
| `ollama_model`         | string | `"llama3.2"`                  | no       |
| `ollama_system_prompt` | string | built-in SRE-style preamble   | no       |

⚠ The `ollama_*` keys are ignored by `v4/llm.chat` — that path
reads exclusively from the `llm.providers.ollama.*` block.  Don't
expect changing `ollama_model` to affect the bdsweb `/chat` page
(which has been on `v4/llm.chat` since Phase 6.a).

---

## 10. Tuning matrix

Quick lookup: pick a symptom, find the relevant knob.

| Symptom                                              | Knob                          | Direction          |
|------------------------------------------------------|-------------------------------|--------------------|
| "too many open files" at runtime                      | `nofile_limit`                | raise              |
| Frequent shard eviction churn in logs                 | `max_open_shards`             | raise              |
| Queries spanning many shards are slow                 | `max_open_shards`             | raise              |
| Memory pressure from open shards                      | `max_open_shards`             | lower              |
| `v2/add` returns `-32099`                             | `ingest_channel_capacity`     | raise              |
|                                                       | `pool_size`                   | raise              |
| Single-record latency too high                        | `pipe_timeout_ms`             | lower              |
| ONNX embedding CPU overhead per record                | `pipe_batch_size`             | raise              |
| Unclean exit losing recent writes                     | `sync_interval_secs`          | lower (or default) |
| Bund script in registry isn't firing                  | `scheduler_interval_secs`     | non-zero (60 OK)   |
| Same script fires on multiple cluster nodes per tick  | `scheduler_dedup_window`      | raise              |
| Peer flaps Alive ↔ Suspect during normal load         | `suspect_timeout`             | raise              |
|                                                       | `gossip_interval`             | lower              |
| Slow peer dragging out fan-outs                       | `peer_rpc_timeout`            | lower              |
| AE backlog growing on rejoin                          | `antientropy_interval`        | lower              |
| Hint queue growth unbounded                           | `hint_max_age`                | check (default 24h)|
| RAG context not reaching Ollama                       | (auto-tuned `num_ctx`)        | see [LLM.md § 12](LLM.md#12--operational-gotchas) |
| Identical LLM requests not hitting cache              | `llm.cache.enabled`           | confirm `true`     |
|                                                       | `temperature`                 | use 0 / unset      |
| Async LLM jobs sitting in pending                     | `llm.runner.enabled`          | confirm `true`     |
| bdsweb pages feel stale                               | `dashboard_refresh_secs`      | lower              |
|                                                       | `cluster_refresh_secs`        | lower              |
| bdsweb RPC traffic saturating bdsnode                 | `dashboard_refresh_secs`      | raise              |
|                                                       | `cluster_refresh_secs`        | raise              |

---

## 11. Required-vs-optional summary

| Scope                  | Required keys                                                |
|------------------------|--------------------------------------------------------------|
| **bdsnode** (always)   | `dbpath`, `shard_duration`                                   |
| **bdsnode** (cluster)  | `cluster.shared_secret` (≥ 16 chars), `cluster.bind_url`     |
| **bdsweb**             | none — every knob has a default                              |
| **bdscli / bdscmd**    | none — neither reads `bds.hjson` directly                    |

Everything else is optional with a documented default.  A
minimum-viable `bds.hjson` for a single-node ingest-only setup:

```hjson
{
  dbpath:         "/var/lib/bdslib"
  shard_duration: "1h"
}
```

---

## 12. Per-binary key matrix

Which binary reads which key.  ✓ = directly, ◐ = via shared
`init_db` / `Cluster::init`, ✗ = never.

| Key                            | bdsnode | bdsweb | bdscli | bdscmd |
|--------------------------------|:-:|:-:|:-:|:-:|
| `nofile_limit`                 | ✓ | ✗ | ✗ | ✗ |
| `dbpath`, `shard_duration`     | ◐ | ✗ | ◐ | ✗ |
| `max_open_shards`              | ◐ | ✗ | ◐ | ✗ |
| `pool_size`, `r2d2_thread_pool_size` | ◐ | ✗ | ◐ | ✗ |
| `embedding_*`                  | ◐ | ✗ | ◐ | ✗ |
| `similarity_threshold`         | ◐ | ✗ | ◐ | ✗ |
| `drain_*`                      | ◐ | ✗ | ◐ | ✗ |
| `jsoncache_*`                  | ◐ | ✗ | ◐ | ✗ |
| `n_workers`, `bund_ttl_secs`   | ✓ | ✗ | ✗ | ✗ |
| `ingest_channel_capacity`      | ✓ | ✗ | ✗ | ✗ |
| `pipe_*` / `file_*` / `syslog_*` | ✓ | ✗ | ✗ | ✗ |
| `scheduler_interval_secs`      | ✓ | ✗ | ✗ | ✗ |
| `sync_interval_secs`           | ✓ | ✗ | ✗ | ✗ |
| `results_ttl_secs`, `results_sweep_secs` | ✓ | ✗ | ✗ | ✗ |
| `vm_cleanup_interval_secs`     | ✓ | ✗ | ✗ | ✗ |
| `cluster.shared_secret`        | ◐ | ✓ | ✗ | ✗ |
| `cluster.bind_url` etc.        | ◐ | ✗ | ✗ | ✗ |
| `cluster.auth_rate_limit_per_minute` | ◐ | ✓ | ✗ | ✗ |
| `llm.providers.*`              | ◐ | ✗ | ✗ | ✗ |
| `llm.cache.*` / `llm.dedup.*`  | ◐ | ✗ | ✗ | ✗ |
| `llm.runner.*`                 | ✓ | ✗ | ✗ | ✗ |
| `dashboard_refresh_secs`       | ✗ | ✓ | ✗ | ✗ |
| `cluster_refresh_secs`         | ✗ | ✓ | ✗ | ✗ |
| `ollama_*` (legacy)            | ✓ | ✗ | ✗ | ✗ |

bdscmd uses CLI flags (`--secret`) and environment variables
(`BDSCMD_CLUSTER_SECRET`) instead of `bds.hjson`.  bdscli operates
on a DuckDB shard directory directly without the server hjson —
its only contact with `bds.hjson` is through subcommands that
call `init_db` (which reuses the same loader).

---

## See also

- [`CLUSTER.md`](CLUSTER.md) — cluster-mode architecture, on-disk
  layout, RPC quick reference.
- [`CLUSTER_DETAILS.md`](CLUSTER_DETAILS.md) — gossip protocol,
  replication wire shapes, dedup mechanisms, auth.
- [`LLM.md`](LLM.md) — provider abstraction, cache, dedup, async
  jobs, full `v4/llm.*` reference.
- [`BDSWEB.md`](BDSWEB.md) — bdsweb route catalog including the
  refresh-cadence routes.
- [`BDSCMD.md`](BDSCMD.md) — operator CLI; no hjson, but every
  subcommand maps onto one of the methods documented here.
- [`DATABASE.md`](DATABASE.md) — what each on-disk artefact
  named in `dbpath` actually contains.
