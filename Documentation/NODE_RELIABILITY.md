# Node Reliability

This document describes the **fault-tolerance** measures in bdsnode
and the underlying `bdslib` — the mechanisms that keep the process
from crashing and keep it from *silently* losing work when something
internal goes wrong.

Reliability is the floor that [`NODE_SELF_HEALING.md`](NODE_SELF_HEALING.md)
builds on.  The distinction:

- **Reliability** (this document) — the process survives internal
  faults: panics are caught, threads are respawned, a poison record
  fails alone, a saturated pool fails fast instead of hanging.
- **Self-healing** — the node *detects its own degraded state and
  actively recovers* (a corrupt shard is quarantined and rebuilt).

Most of the reliability layer is **automatic and has no
configuration** — it is code hardening, always on.  Where a knob
exists, it is called out with a config example.

---

## Table of contents

1. [Hot-path panic elimination](#1-hot-path-panic-elimination)
2. [Ingest flusher supervision](#2-ingest-flusher-supervision)
3. [Per-record batch fallback](#3-per-record-batch-fallback)
4. [DuckDB connection-pool bounding](#4-duckdb-connection-pool-bounding)
5. [Background-task panic supervision](#5-background-task-panic-supervision)
6. [Adaptive per-peer RPC timeout](#6-adaptive-per-peer-rpc-timeout)
7. [SQL hardening](#7-sql-hardening)
8. [Lock-poison recovery](#8-lock-poison-recovery)
9. [Configuration reference](#9-configuration-reference)
10. [Observability reference](#10-observability-reference)

---

## 1. Hot-path panic elimination

The ingest write path runs on a small pool of flusher threads.  A
panic anywhere in that path used to kill the thread.  The path was
audited and every `unwrap()` / `unreachable!()` / unchecked index that
depended on a cross-function invariant was converted to a
`Result`-returning error:

- `ObservabilityStorage::add` / `add_batch` — the
  "secondary record always has a similar primary" and "primary record
  always has an embedding" invariants now fail *one record* with an
  error instead of panicking the flusher.
- `embed_batch` is length-checked against its input once, so a short
  return can never become an index-out-of-bounds panic downstream.
- `Shard::add` / `add_batch` — the same `opt_emb.unwrap()` pattern is
  now an error.

**Net effect:** a violated invariant fails the *one offending record*
(which then goes through the per-record fallback, §3) instead of
taking down the flusher.  This work has **no configuration** — it is
pure code hardening.

---

## 2. Ingest flusher supervision

`pipe_flushers` flusher threads drain the `ingest` channel.  Before
the supervisor, a flusher that died (panic, or an unrecoverable error)
was invisible until shutdown — ingest silently stopped and records
piled up in the channel until it overflowed.

A dedicated **supervisor thread** now owns the flusher lifecycle:
every 5 seconds it checks each flusher's `JoinHandle::is_finished()`
and **respawns** any that have exited unexpectedly, logging the panic
payload.  It maintains process-wide counters (`alive`, `configured`,
`restarts_total`) and reports flusher health into the health registry.

### `pipe_flushers`

```hjson
// v2/add — single-record + small-batch ingest
pipe_batch_size:  500
pipe_timeout_ms:  500
pipe_flushers:    1     // concurrent flusher threads; clamp [1, 16]
```

**Important constraint:** the flush operation itself is internally
serialised by `ShardsManager::add_batch` — DuckDB, Tantivy, and HNSW
are not safe under multiple concurrent same-shard writers.  Setting
`pipe_flushers > 1` therefore does **not** parallelise the flush;
extra threads only let one thread accumulate a new batch while another
is mid-flush.  Within a single `add_batch` call the work is already
parallelised across shards via Rayon, so a lone flusher saturates the
engines.

Keep the default `1` unless `v2/perf.ingest.lag.p95_us` is high *and*
`ingest.flush.p95_us` is low — the signature of batch accumulation
(not the flush) being the bottleneck.  Regardless of the count, the
supervisor keeps every flusher alive.

See [`NODE_SELF_HEALING.md §6`](NODE_SELF_HEALING.md) for the
supervisor's place in the self-healing picture.

---

## 3. Per-record batch fallback

A flush is one `add_batch` call wrapping `BEGIN … COMMIT` — it is
all-or-nothing.  A single malformed record (a `data` field that breaks
the JSON cast, a constraint violation) used to fail the **whole
batch**, silently dropping up to `pipe_batch_size` (default 500) good
records with it.

Now, when a whole-batch insert fails, the flusher **retries each
record as its own 1-element batch** — identical code path, but a
failure isolates exactly one record.  The good records land; the
poison record is logged (one `dropped poison record` line per
increment) and counted in `ingest_flushers.records_dropped`.

This work is **automatic — no configuration**.  The only operator
signal is the `records_dropped` counter: non-zero means genuine,
identifiable data loss, and the log has the per-record reason.

---

## 4. DuckDB connection-pool bounding

Each shard's storage engine wraps an r2d2 connection pool.  r2d2's
default `connection_timeout` is **30 seconds** — long enough that a
saturated pool looks like a hang.

The pool is now built with an explicit **10-second** checkout timeout.
A checkout that can't be satisfied in 10 s fails fast with a clear
error (`pool checkout failed for <op> (pool saturated?)`) instead of
stalling.  Every such timeout increments a process-wide counter,
surfaced as `v2/status.pool.checkout_timeouts`.

The 10-second ceiling is **fixed in code** — not configurable.  The
operator lever is `pool_size`:

```hjson
pool_size:  4     // DuckDB connections per shard engine; default 4
```

A non-zero `checkout_timeouts` means a pool ran out of connections
under load — raise `pool_size`, or shed the load that is holding
connections.

---

## 5. Background-task panic supervision

bdsnode runs ~10 long-lived background tasks (gossip, scheduler, sync,
retention, rebalancer, llm_jobs, the shard healer, …).  Each is a
`loop { select! { … } }`.  A panic in a tick body that `.await`s work
directly used to unwind the whole task — the subsystem then silently
stopped until the process restarted.

A shared helper, `supervise::tick`, now wraps each tick body in
`catch_unwind`: a panic is **caught, logged, and swallowed**, and the
loop carries on to the next tick.  Tasks that already ran their work
on `spawn_blocking` (which catches panics via `JoinError`) keep that
pattern; the rest — gossip, the rebalancer sweep, the LLM job drain,
the shard healer — went through `supervise::tick`.

Combined with the **heartbeat watchdog** (see
[`NODE_SELF_HEALING.md §1`](NODE_SELF_HEALING.md)), this closes both
failure modes: `supervise::tick` catches a *panicking* tick; the stale
heartbeat catches a *hung* loop.

This work is **automatic — no configuration**.

---

## 6. Adaptive per-peer RPC timeout

(Cluster mode only.)  The cluster read fan-out (`fan_out_v2`) does not
use a single static deadline for every peer.  When
`cluster.adaptive_peer_timeout_enabled` is `true` (the default), it
derives a per-peer deadline from that peer's observed p95 latency:

```
dynamic_us = min(peer_rpc_timeout, p95_us × multiplier)
               .max(peer_rpc_timeout / 10)
               .max(1_000)
```

- It **never exceeds** the configured `peer_rpc_timeout` — that
  remains the operator's hard contract.
- It **never drops below 10 %** of it.
- It falls back to the static timeout until the peer has ≥ 20 recent
  samples (small windows produce unstable percentiles).
- It is **self-stabilising**: every RPC outcome, success *or* timeout,
  is recorded into the peer's p95 series, so a chronically broken
  peer's p95 converges to the static timeout and the heuristic stops
  tightening.

```hjson
cluster: {
  peer_rpc_timeout:                 "2s"
  adaptive_peer_timeout_enabled:    true     // default
  adaptive_peer_timeout_multiplier: 3.0      // default
}
```

Disable it (`adaptive_peer_timeout_enabled: false`) on WAN clusters
where occasional latency spikes are expected and a "wait the full
deadline" policy is preferred over fail-fast.

Writes (replication) deliberately do **not** adapt — a write is
durable or it is hinted, and shortening its deadline based on read
history would only convert durable-with-latency into
hinted-with-retry for no benefit.

---

## 7. SQL hardening

`bdslib` builds SQL by `format!` interpolation rather than
parameterised queries.  An audit confirmed **every** call site that
interpolates caller-supplied data (telemetry `key` / `data_text`,
document bodies, LLM job payloads, cache keys) wraps it in
`common::sql::sql_escape`; values that are not caller-controlled
(UUIDs, integers, hex/base64 blobs, `serde_json` output) are quote-free
by construction and deliberately skip it.

`sql_escape`'s contract is now documented in detail (it doubles `'`,
which is necessary and sufficient for DuckDB string literals, since
DuckDB does not process C-style backslash escapes), with a regression
test suite.  This is a **convention**, not a type-enforced guarantee —
a new query path that forgets `sql_escape` reopens the surface — so
the convention is loudly documented at the `sql_escape` definition.

**No configuration** — this is a code-discipline measure.

---

## 8. Lock-poison recovery

A `std::sync::Mutex` becomes *poisoned* if a thread panics while
holding it — every subsequent `lock()` then returns `Err` forever.
The VM stdlib's shared RNG and conditional-format mutexes used
`.lock().unwrap()`, so a panic in one Bund worker would permanently
brick `math.random.*` for every subsequent VM.

Those sites now use `.lock().unwrap_or_else(|e| e.into_inner())` —
which recovers the guard from a poisoned lock.  For a RNG and a
control-flow registry there is no broken invariant a panic could
leave behind that matters, so recovering is correct.

The data-path `std::sync::Mutex` instances (`add_batch_lock`, the
embedding-model-name slot) surface poisoning as a clean `Result::Err`
rather than re-panicking.

**No configuration** — pure hardening.

---

## 9. Configuration reference

Most of the reliability layer is automatic.  The knobs that *do* exist:

| Key                                       | Default | Lever for |
|--------------------------------------------|---------|-----------|
| `pipe_flushers`                            | `1`     | §2 — flusher thread count (clamp `[1,16]`) |
| `pipe_batch_size`                          | `500`   | §3 — records per flush; also the max a single poison batch can affect before the per-record fallback |
| `pipe_timeout_ms`                          | `500`   | §2/§3 — partial-batch flush deadline |
| `pool_size`                                | `4`     | §4 — DuckDB connections per shard engine |
| `cluster.peer_rpc_timeout`                 | `"2s"`  | §6 — the hard per-peer RPC deadline ceiling |
| `cluster.adaptive_peer_timeout_enabled`    | `true`  | §6 — adaptive per-peer timeout master switch |
| `cluster.adaptive_peer_timeout_multiplier` | `3.0`   | §6 — adaptive timeout aggressiveness |
| `perf.slow_query_threshold_ms`             | `500`   | §10 — slow-query log threshold (observability) |

Everything else in §1, §3, §5, §7, §8 is unconditional code
hardening with no knobs.

A representative reliability-relevant `bds.hjson` slice:

```hjson
// ── ingest ──
pipe_batch_size:  500
pipe_timeout_ms:  500
pipe_flushers:    1

// ── per-shard DuckDB pool ──
pool_size:        4

// ── perf / slow-query observability ──
perf: {
  slow_query_threshold_ms: 500   // 0 disables the slow-query log
}

// ── cluster transport (cluster mode only) ──
cluster: {
  peer_rpc_timeout:                 "2s"
  adaptive_peer_timeout_enabled:    true
  adaptive_peer_timeout_multiplier: 3.0
}
```

---

## 10. Observability reference

### `v2/status` blocks

| Block             | Reliability signal |
|-------------------|--------------------|
| `ingest_flushers` | `alive` / `configured` (a gap means a flusher is mid-respawn or wedged), `restarts_total` (non-zero ⇒ a flusher panicked at least once), `records_dropped` (genuine data loss — §3) |
| `pool`            | `checkout_timeouts` — non-zero ⇒ a DuckDB pool ran out of connections (§4) |
| `perf`            | headline `ingest.flush` / `ingest.lag` percentiles |
| `health`          | aggregate verdict — a `failed` here may be a hung background task (§5) |

### `v2/perf` + `v2/perf.slow_queries`

Every `perf::time` / `record_us` call participates in the perf
registry.  When a call's elapsed time exceeds
`perf.slow_query_threshold_ms`, it also lands in a bounded 100-entry
**slow-query ring** — visible via `v2/perf.slow_queries` or
`bdscmd perf-slow`.  This catches single-event outliers that p95
sample-dilution hides.

```bash
$ bdscmd perf-slow --name-prefix ingest.
{ "threshold_ms": 500, "entries": [
    { "name": "ingest.flush", "elapsed_ms": 1284, "ts": 1778900000 } ] }
```

Set `perf.slow_query_threshold_ms: 0` to disable the slow-query ring
(the percentile registry stays populated).

### Log lines to grep

| Pattern                                          | Meaning |
|--------------------------------------------------|---------|
| `[add] flusher N panicked: ... — respawned`      | the supervisor caught + respawned a dead flusher (§2) |
| `dropped poison record:`                         | a single record failed the per-record fallback (§3) |
| `pool checkout failed for ... (pool saturated?)` | a DuckDB pool hit the 10 s ceiling (§4) |
| `[<task>] tick panicked ... — loop survives`     | `supervise::tick` caught a background-task panic (§5) |
| `[<task>] no heartbeat for ...s — task hung?`    | the heartbeat watchdog flagged a hung loop (§5) |

---

## See also

- [`NODE_SELF_HEALING.md`](NODE_SELF_HEALING.md) — the layer that
  builds on this: detect degraded state and actively recover.
- [`BDSCONFIG.md`](BDSCONFIG.md) — the full configuration reference
  (ingest tuning §3, perf §4.2, cluster §6).
- [`jsonrpc_api/v2_perf.md`](jsonrpc_api/v2_perf.md) — the perf
  registry + slow-query log RPC reference.
