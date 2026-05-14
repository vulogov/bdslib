# Node Self-Healing

This document describes bdsnode's **self-healing** layer — the
mechanisms by which a node *detects its own degraded state and
actively recovers from it*, rather than merely surviving a fault.

Self-healing is distinct from — and builds on top of — the
fault-tolerance work documented in
[`NODE_RELIABILITY.md`](NODE_RELIABILITY.md).  Reliability is "the
process does not crash and does not silently lose work."  Self-healing
is "a corrupt shard is detected, isolated, and rebuilt without an
operator touching anything."

> **Scope.** Everything here is **per-node** and works on a standalone
> node — with one exception clearly marked: **Tier-3 shard recreation**
> requires cluster mode (it relies on peers to repopulate a recreated
> empty shard).  See [`CLUSTER_DETAILS.md §8.5`](CLUSTER_DETAILS.md)
> for the cluster-protocol view of that piece.

---

## Table of contents

1. [The health registry](#1-the-health-registry)
2. [Shard quarantine — detection + isolation](#2-shard-quarantine--detection--isolation)
3. [The rebuild healer — three tiers](#3-the-rebuild-healer--three-tiers)
4. [Consistency sweep](#4-consistency-sweep)
5. [Circuit breaker](#5-circuit-breaker)
6. [Ingest flusher supervisor](#6-ingest-flusher-supervisor)
7. [Configuration reference](#7-configuration-reference)
8. [Observability reference](#8-observability-reference)

---

## 1. The health registry

The spine of the self-healing layer is a process-wide **health
registry** (`bdslib::health`).  Every long-lived subsystem registers a
named source and reports two independent signals:

- **`status`** — the subsystem's own self-assessment:
  `Healthy` / `Degraded(reason)` / `Failed(reason)`.
- **`last_heartbeat`** — wall-clock of the last heartbeat.  Background
  loops bump this every tick.

The crucial design point: a **stale heartbeat overrides a healthy
status**.  A *hung* loop — deadlocked, or stuck in an infinite await —
can never get the chance to set `status = Failed`, so its `status`
stays whatever it last was.  The registry treats any source whose
heartbeat is older than its declared `stale_after` window as
effectively `Failed`, regardless of the self-reported status.  That
is how a hung subsystem is caught.

Registered sources and their staleness windows:

| Source             | Heartbeat cadence            | `stale_after`        |
|--------------------|------------------------------|----------------------|
| `cluster.gossip`   | every gossip tick            | 6× gossip interval, ≥30 s |
| `rebalancer`       | every rebalance sweep        | 3× sweep interval, ≥60 s  |
| `sync`             | every global-sync tick       | 3× interval, ≥60 s   |
| `retention`        | every retention sweep        | 3× interval, ≥120 s  |
| `scheduler`        | every scheduler tick         | 3× interval, ≥60 s   |
| `llm_jobs`         | every job-drain poll         | 10× poll, ≥30 s      |
| `ingest.flushers`  | every supervisor poll (5 s)  | 30 s                 |
| `shard_healer`     | every heal sweep             | 4× interval, ≥120 s  |
| `shard.<start>_<end>` | on quarantine / heal      | (no staleness check) |

### `v2/health`

A dedicated readiness/liveness probe — cheap, in-process, no DB
access.  Intended for load balancers and orchestrators (k8s
readiness/liveness, HAProxy health checks):

```bash
$ bdscmd health
{
  "status":  "healthy",
  "reason":  "",
  "ts":      1778900000,
  "sources": [
    { "name": "cluster.gossip",  "status": "healthy", "reason": "",
      "last_heartbeat": 1778899998, "stale": false },
    { "name": "ingest.flushers", "status": "healthy", "reason": "",
      "last_heartbeat": 1778899999, "stale": false }
  ]
}
```

The aggregate `status` is the worst of all sources (`failed` beats
`degraded` beats `healthy`).  `v2/status` also carries a compact
`health` block — the same verdict plus `n_sources` / `n_degraded` /
`n_failed` counts — for the dashboard tile.

The health registry itself has **no configuration** — it is always on
and always free when nothing is wrong.

---

## 2. Shard quarantine — detection + isolation

A shard is three storage engines under one directory:

| Sub-path        | Engine                        |
|-----------------|-------------------------------|
| `obs.db`        | DuckDB — the **source of truth** |
| `fts/`          | Tantivy full-text index       |
| `vec/`          | HNSW vector index             |

A crash mid-write or a disk fault can corrupt any of them.  Without
intervention the failure is permanent: every operation touching that
time window fails forever.

**Detection.** `ShardsCache::shard()` validates **all three** engines
on open — `Shard::with_config` eagerly probes the lazily-opened
Tantivy/HNSW indexes so corruption surfaces *at open time*, not later
on a query.  Consecutive open failures for one shard interval are
counted by the `shardhealth` tracker; after **3 in a row** the shard
is flagged `quarantined` in the catalog.

**Isolation.** A quarantined shard's catalog row carries a
`quarantined` boolean; `ShardsCache::shard()` **short-circuits** it —
reads and writes for that time window get a transient-failure error
instead of touching the broken shard, so the rest of the node keeps
serving.  A 5-minute cooldown prevents re-quarantine thrash.

In a cluster, a quarantined shard narrows what this node can answer:
cluster reads that fan out here return fewer rows, surfaced as
`partial: true` in `cluster_meta`.

Quarantine has **no configuration** — the thresholds are fixed
(3 failures, 5-minute cooldown).  What *is* configurable is the
recovery side: the rebuild healer (§3) and consistency sweep (§4).

---

## 3. The rebuild healer — three tiers

The `shard_healer` background task sweeps the quarantined-shard list
every `self_healing.interval` (default 60 s) and attempts a repair,
cheapest tier first.

### Tier 1 — transient retry

Many quarantines are false positives — a momentary pool saturation or
fs hiccup that crossed the 3-failure threshold.  Tier 1 simply
re-opens the shard.  If it opens cleanly **and** passes a consistency
check (§4), the quarantine is cleared with no further action.

> The consistency check inside Tier 1 matters: a shard quarantined by
> the *consistency sweep* (§4) opens fine but is internally drifted —
> Tier 1 must not declare it "healed" just because it opens.  When the
> shard opens but is inconsistent, Tier 1 falls through to Tier 2.

### Tier 2 — index rebuild

If the re-open still fails, the healer probes DuckDB alone.  When
DuckDB opens (the source of truth is intact) the corruption is in the
`fts/` / `vec/` directories: the healer **deletes** both, re-opens the
shard (which recreates them empty), and replays every primary record
from DuckDB via `Shard::rebuild_indexes`.  The vector embedding is
recovered **exactly** from the `primary_embeddings` table — no ONNX
re-embedding — so a rebuild is cheap and deterministic.

### Tier 3 — recreate (opt-in, destructive, cluster-only)

When DuckDB *itself* won't open, the shard cannot be rebuilt from
local data.  By default it stays `FAILED` forever, waiting for an
operator.  **Tier 3 automates that recovery — but only when it is
safe.**  All four conditions must hold:

1. the shard has been continuously *unhealable* for longer than
   `self_healing.failed_shard_recreate_after` (default `1h`);
2. `self_healing.recreate_failed_shards` is `true` (opt-in — it is
   destructive);
3. the cluster `rebalancer` is enabled;
4. the node is **actually in cluster mode** — a hard safety net
   independent of the config flags.

When all four hold, the healer **destroys** the failed shard
(`remove_dir_all` + catalog row delete) and recreates it empty for the
same `[start, end)` interval.  The peers' rebalancers then repopulate
it via `v2/cluster.replicate_record` — their next sweep sees this node
missing those records and pushes them back.  The full chain:

```
corrupt obs.db  →  3 failed opens  →  quarantined
   →  healer Tier-1/2 fail  →  Unhealable
   →  unhealable > failed_shard_recreate_after  →  Tier-3 recreate (empty)
   →  peers' rebalancers push the records back  →  shard whole again
```

Condition 4 is a hard data-safety gate: a **standalone** node has no
peers to repopulate from, so recreating an empty shard there is
permanent data loss — the healer refuses, regardless of the flags, and
logs why.  If `recreate_failed_shards` is off, or the rebalancer is
disabled, or the node is standalone, the shard simply stays `FAILED`.

---

## 4. Consistency sweep

Quarantine catches a shard that fails to *open*.  But a shard can open
cleanly and still be **internally inconsistent**: a partial flush —
DuckDB committed, but the Tantivy commit or HNSW upsert failed —
leaves the three engines holding different record sets, silently
degrading search.

Every `self_healing.consistency_interval` (default 10 m) the healer
runs a consistency sweep: it walks every **sealed** shard (one whose
time interval has fully elapsed — the active write target is skipped,
because its counts legitimately diverge for the milliseconds between a
batch's DuckDB and Tantivy commits) and compares:

- DuckDB primary-record count (`telemetry WHERE is_primary = 1`)
- Tantivy document count
- HNSW active-vector count

In a healthy sealed shard all three are **exactly equal** — every
primary record is one DuckDB row + one Tantivy doc + one HNSW vector.
Any divergence **quarantines** the shard, so the rebuild healer's
Tier 2 re-indexes it from DuckDB on its next pass.

Set `consistency_interval: "0s"` to disable the sweep.

---

## 5. Circuit breaker

Between a shard *starting* to fail and quarantine engaging (3
consecutive open failures), every caller hitting that time window
would block up to the full DuckDB pool-checkout timeout (10 s) on each
doomed open.  The per-shard **circuit breaker** bounds that latency
cost.

After **2** consecutive open failures (one *below* the quarantine
threshold, so it engages first) the breaker trips **Open**: every
`ShardsCache::shard()` call for that shard fast-fails *instantly* for a
30-second cooldown.  Then it goes **HalfOpen** and lets one probe
attempt through — which closes the breaker on success, or re-arms the
Open window on failure.

The breaker **paces, not replaces** quarantine — its HalfOpen probes
still feed the quarantine tracker, so a genuinely-broken shard still
gets quarantined, just without every caller paying the open cost on
the way there.  The breaker is fully automatic — **no configuration**.

---

## 6. Ingest flusher supervisor

The ingest pipeline runs `pipe_flushers` flusher threads (default 1).
A panic in a flusher used to be invisible — `Handle::stop` only joined
the threads at shutdown — so a panicked flusher silently stopped
ingest until the next restart.

The **flusher supervisor** is a dedicated thread that owns the flusher
lifecycle: every 5 seconds it checks each flusher's liveness and
**respawns** any that have died, logging the panic payload and bumping
`restarts_total`.  It also reports flusher health into the registry
(`alive == 0` → `Failed`, `alive < configured` → `Degraded`).

The flusher *panic-elimination* work (making the hot path panic-free
in the first place) is documented in
[`NODE_RELIABILITY.md`](NODE_RELIABILITY.md) — the supervisor is the
self-healing backstop for the residual case.

---

## 7. Configuration reference

The only configuration block for the self-healing layer is
`self_healing:` in `bds.hjson`.  Every key has a safe default; a
missing block keeps all defaults.

```hjson
self_healing: {
  // Master switch for the shard rebuild healer (Tiers 1-2).  On by
  // default — the healer only ever touches shards the quarantine
  // layer has already isolated, so the blast radius is small.
  enabled:                      true

  // Cadence of the heal sweep (Tiers 1-2).  Clamped to [10s, 1h].
  interval:                     "60s"

  // Cadence of the cross-engine consistency sweep (§4).
  // "0s" disables the consistency sweep entirely.
  consistency_interval:         "10m"

  // Tier-3 escalation — DESTRUCTIVE, opt-in.  When true (and the
  // rebalancer is enabled, and the node is in cluster mode), a shard
  // that has been unhealable for longer than the window below is
  // destroyed and recreated empty for the rebalancer to repopulate.
  recreate_failed_shards:       false

  // How long a shard must stay continuously unhealable before Tier-3
  // may recreate it.  Generous default so transient faults and
  // operator intervention both have time to act first.
  failed_shard_recreate_after:  "1h"
}
```

### Recommended profiles

**Default (single node or cluster, conservative):**

```hjson
self_healing: {
  enabled:               true
  interval:              "60s"
  consistency_interval:  "10m"
  // recreate_failed_shards stays false — Tier-3 off
}
```

Tiers 1-2 + the consistency sweep run; an unrepairable shard stays
`FAILED` for an operator to handle.  Safe everywhere.

**Cluster with full automated recovery:**

```hjson
rebalancer: {
  enabled:  true
  interval: "10m"
}
self_healing: {
  enabled:                      true
  interval:                     "60s"
  consistency_interval:         "10m"
  recreate_failed_shards:       true
  failed_shard_recreate_after:  "30m"
}
```

Tier-3 is armed: a shard with corrupt DuckDB that the local healer
can't repair within 30 minutes is recreated empty and repopulated from
peers.  Only safe in a cluster — the `recreate_failed_shards` flag is
a no-op (with a logged warning) on a standalone node.

**Minimal — detection only, no automated repair:**

```hjson
self_healing: {
  enabled:               false   // no rebuild healer
  consistency_interval:  "0s"    // no consistency sweep
}
```

Quarantine still isolates broken shards (that is not configurable —
it is part of the read/write path), and `v2/health` still reports
them — but nothing is rebuilt automatically.  Use this when you want
a human in the loop for every shard repair.

---

## 8. Observability reference

### `v2/health`

The dedicated probe.  See §1 for the payload shape.  `bdscmd health`
wraps it.

### `v2/status` blocks

`v2/status` carries four self-healing-relevant blocks:

| Block             | Fields |
|-------------------|--------|
| `health`          | `status`, `reason`, `n_sources`, `n_degraded`, `n_failed` — the aggregate verdict |
| `self_healing`    | `quarantined_now`, `quarantines_total`, `heals_total`, `unhealable_total`, `recreations_total`, `breaker_trips_total` |
| `ingest_flushers` | `alive`, `configured`, `restarts_total`, `records_dropped` |
| `pool`            | `checkout_timeouts` — DuckDB connection-pool exhaustion count |

### Reading the counters

- `quarantined_now > 0` — shards are currently isolated; the healer is
  (or will be) working on them.
- `heals_total` climbing while `quarantined_now` returns to 0 —
  healthy self-healing in action.
- `unhealable_total > 0` — shards whose DuckDB itself is corrupt.  If
  `recreations_total` is also climbing, Tier-3 is recovering them; if
  not, they are sitting `FAILED` and need an operator (or Tier-3 to be
  enabled).
- `breaker_trips_total > 0` — shards have been struggling enough to
  trip the circuit breaker; cross-reference with `quarantines_total`.
- `v2/health` verdict `failed` — at least one source (a hung task, or
  an unhealable shard) needs attention; the `sources` array names it.

### Log lines to grep

| Pattern                                | Meaning |
|----------------------------------------|---------|
| `[shard-health] ... quarantining`      | a shard just crossed the 3-failure threshold |
| `[consistency] shard ... DRIFTED`      | the consistency sweep found engine divergence |
| `[shard-healer] ... recovered (transient` | Tier-1 cleared a false-positive quarantine |
| `[shard-healer] ... rebuilt — N primary record(s) re-indexed` | Tier-2 index rebuild succeeded |
| `[shard-healer] ... RECREATED empty`   | Tier-3 destroyed + recreated a shard |
| `[shard-healer] ... CANNOT self-heal — stays FAILED` | unhealable shard, Tier-3 not applicable |
| `circuit breaker OPEN — fast-failing`  | the per-shard breaker is shedding load |

---

## See also

- [`NODE_RELIABILITY.md`](NODE_RELIABILITY.md) — the fault-tolerance
  layer this builds on (panic elimination, task supervision, pool
  bounding, per-record batch fallback).
- [`CLUSTER_DETAILS.md §8.5`](CLUSTER_DETAILS.md) — the
  cluster-protocol view, especially Tier-3's dependency on the
  rebalancer.
- [`BDSCONFIG.md §5.7`](BDSCONFIG.md) — the `self_healing:` block in
  the full configuration reference.
- [`RETENTION.md`](RETENTION.md) — the `evicting` shard lifecycle,
  which the `quarantined` machinery mirrors.
