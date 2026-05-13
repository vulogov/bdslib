# Shard retention — time-based eviction

bdsnode can drop telemetry shards whose `end_ts` is older than a
configurable retention window.  Eviction is **online** (no node stop,
no quiesce) and **per-node** (each peer enforces its own policy
independently).  The feature is **opt-in** — fresh deployments do not
delete data until `retention.enabled = true` is explicit in
`bds.hjson`.

This document is the operator-facing reference.  Architectural
rationale + cluster semantics live in
[`CLUSTER_DETAILS.md`](CLUSTER_DETAILS.md); the library API for
running a sweep programmatically is in
[`STORAGEENGINE.md`](STORAGEENGINE.md).

---

## What gets evicted

Only **sharded telemetry data** — the per-shard directory hierarchy
under `{dbpath}/{start_ts}_{end_ts}/`, containing `obs.db` (DuckDB),
`fts/` (Tantivy), `vec/` (HNSW), and `tplstorage/` (drain-mined
templates).

**Never** touched by retention:

| Store        | Lives at                    | Why retained                                      |
|--------------|-----------------------------|---------------------------------------------------|
| Documents    | `{dbpath}/docstore/`        | Fully replicated, no time partition               |
| Signals      | `{dbpath}/signals/`         | Fully replicated, append-only                     |
| Scripts      | `{dbpath}/scripts/`         | Fully replicated, addressable by UUID             |
| Users        | `{dbpath}/users/`           | Fully replicated, cluster auth                    |
| LLM cache    | `{dbpath}/llm/`             | Fully replicated, anti-entropy keeps it converged |
| Catalog      | `{dbpath}/shards_info.db`   | The catalog ITSELF — never evicted                |

If you want to delete a document, signal, script, or LLM cache row,
use the corresponding `v2/doc.delete` / `v3/script.delete` / etc.
methods.  Retention does not touch them.

---

## Cluster semantics (per-node)

Retention is **per-node**, with no cluster coordination:

- Anti-entropy (`cluster.antientropy_*`) never touches sharded
  telemetry, so an evicted shard cannot be resurrected from a peer.
- Cluster reads (`v3/search`, `v3/aggregationsearch`, `v3/count`, …)
  fan out to every Alive peer and merge what's available.  When a
  peer has evicted a shard, its `peers_answered` is just lower for
  that window — `cluster_meta.partial: true` flags the situation in
  the response.
- Each peer's `retention.duration` is independent.  An edge node may
  keep `"6h"` while a long-retention core keeps `"90days"`.

**Critical**: cluster-wide retention is the **minimum** across peers
holding each shard.  Two peers both evicting the same shard at the
same time = permanent data loss.  Mitigations:

1. Pick `retention.duration` ≥ the longest expected read window
   you actually care about.
2. Run with `cluster.replication_factor ≥ 2` so every record is on
   at least two peers.
3. Avoid pinning identical schedules on every peer if you can stagger
   them.

bdsnode logs a startup WARN when it detects `replication_factor=1`
AND `retention.enabled=true`:
`[retention] WARN: replication_factor=1 with retention enabled — evicted data has no peer copy`.

### Auditing cluster-wide policy alignment

Per-node policy is a feature, not a bug — but a forgotten config
drift after a rolling restart can silently push your effective
cluster-wide retention down to the tightest peer's window.  Phase 2
of the retention feature adds a read-only cluster RPC that lets you
audit the policy on every peer in one call:

```bash
bdscmd cluster retention-status
```

The response includes:

- `local` — this node's `v2/retention.settings` payload.
- `peers[]` — one entry per Alive peer (each calls
  `v2/retention.settings` in-process; failed peers carry `error`
  instead of `settings`).
- `summary.consistent` — `true` iff every peer agrees on
  `duration` AND `interval_secs` AND has retention installed.
- `summary.distinct_durations` — sorted unique humantime strings
  observed.  When `consistent: false`, this is the audit trail.

Wire it into a probe:

```bash
bdscmd cluster retention-status \
  | jq -r '
      if .summary.consistent
      then "OK — every peer agrees on retention.duration"
      else "DRIFT: " + (.summary.distinct_durations | join(", "))
        + " across " + (.summary.total_nodes | tostring) + " nodes"
      end'
```

See [`jsonrpc_api/v3_cluster_retention.md`](jsonrpc_api/v3_cluster_retention.md)
for the full RPC reference.

> **Intentional omission**: there is **no** cluster-wide sweep RPC.
> Mass-evicting across every peer at once would be too easy to
> misuse (`--force --duration 1s` on every node = permanent data
> loss).  Per-node `v2/retention.sweep` stays under the operator's
> deliberate control.

### Phase 3 — cluster-aware quorum (opt-in safety net)

A Phase 3 quorum check refuses to evict a shard when no other live
peer holds a copy of the same `(start_ts, end_ts)` interval.
Turned on with `retention.quorum_check_enabled = true` in
`bds.hjson` (**default: false** — strictly opt-in).

When enabled, the sweeper does one extra step at the start of every
sweep:

1. Fan out [`v2/cluster.shards.list`](jsonrpc_api/v2_cluster_shards_list.md)
   to every Alive peer.
2. Build a `HashMap<(start_ts, end_ts), peer_count>` from the
   returned catalogs.
3. For each candidate shard, only proceed to eviction when
   `peer_count >= retention.quorum_min_peers` (default 1).
4. Candidates that fail the check are **skipped**, not deleted —
   they survive the sweep and are reconsidered next tick.

Skips are surfaced everywhere stats appear:

- `EvictionReport.quorum_skipped` — per-sweep count.
- `v2/status.retention.quorum_skipped_lifetime` and
  `…last_run` — process-wide totals.
- `v2/retention.sweep` response carries `quorum_skipped` +
  `quorum_enabled`.
- Per-shard log lines:
  `[retention] quorum check refused eviction of shard ⟨uuid⟩ (⟨path⟩): fewer than N other live peers hold the [start,end) interval`.

**Fail-safe by default.**  When the cluster is unreachable, every
peer fails the shards.list probe, or `cluster.enabled = false`, the
sweeper treats the situation as "no quorum" and skips ALL
candidates that tick.  This guarantees `quorum_check_enabled = true`
can never cause data loss — only false-negative skips that the
operator sees in the logs and the lifetime counter.

#### When to enable it

| Scenario                                          | Recommendation |
|---------------------------------------------------|----------------|
| `cluster.replication_factor = 1`                   | **Don't enable** — every shard is by definition the lone copy.  Quorum will skip every candidate forever; you may as well set `retention.enabled = false`. |
| `replication_factor ≥ 2` with uniform `shard_duration` | **Recommended.**  Catches operator errors where a misconfigured peer would otherwise evict the last replica. |
| Mixed `shard_duration` across peers                | **Don't enable.**  Exact-interval matching fails closed when peers use different durations → every eviction is skipped.  Standardise `shard_duration` first. |
| Standalone (single-node) deployments               | **Don't enable.**  There are no peers; every sweep will skip everything. |

#### Worked example

Two-node cluster with `replication_factor = 2`,
`retention.enabled = true`, `retention.duration = "1h"`, and on
node A `retention.quorum_check_enabled = true`:

1. Record ingested with `timestamp = 1700000000`.  Sharded write
   lands on both nodes (RF=2) at the same `[1699999200, 1700002800)`
   interval.
2. An hour passes.  Both nodes' retention sweepers tick.
3. Node A's sweep fans out `v2/cluster.shards.list` to node B,
   sees node B has the interval, allows the eviction.  Node A's
   shard is gone.
4. Node B's sweep is also enabled; it fans out to node A, gets
   "no shards" because node A already evicted, **skips the
   eviction**.  Node B's shard survives.
5. End state: one replica remains.  Data preserved.

The same recipe works in 3+ node clusters — as long as
`quorum_min_peers` peers still hold the interval at eviction
time, the candidate is allowed through.

#### Cost

Each sweep adds one extra RPC per Alive peer (the shards.list
fetch) before any eviction is attempted.  At a 5-minute sweep
cadence on a 5-node cluster that's 5 RPCs every 5 minutes per
node — operationally invisible compared to the ingest path.  The
catalog read itself is sub-millisecond on disk (single indexed
SELECT, no shard opens), so peers see no meaningful load increase.

---

## Configuration

```hjson
retention: {
  // Master switch.  Defaults to false so existing deployments
  // don't suddenly start deleting data on upgrade.
  enabled: false

  // Humantime window.  Shards whose end_ts < (now - duration) are
  // eligible for eviction.  Parsed via humantime::parse_duration.
  // Examples: "30days", "7d", "12h", "90min".  Must be > 0.
  duration: "30days"

  // How often the sweeper runs, in seconds.  Clamped [60, 86400].
  // Setting 0 disables the BACKGROUND task without affecting the
  // master enabled flag — useful for operators who want to drive
  // sweeps exclusively via `v2/retention.sweep`.
  interval_secs: 300

  // Cap evictions per run so a one-time policy tightening
  // ("365d → 7d") doesn't bulk-delete the entire historic corpus
  // in a single tick.  0 = no cap.  Default 50.
  max_evictions_per_run: 50

  // When true, log what WOULD be evicted but don't touch the
  // catalog or filesystem.  Useful for verifying a policy change
  // before flipping enabled = true.
  dry_run: false

  // After a sweep evicts ≥ 1 shard, re-seed the in-memory drain
  // parser from the current set of stored templates.  Without
  // this the parser still holds in-memory cluster IDs that
  // back-reference templates whose underlying shard is gone.
  // Defaults to true; cost is one DocumentStorage scan per sweep.
  reload_drain_after_evict: true

  // ── Phase 3 — cluster-aware quorum (opt-in safety net) ──
  //
  // Refuse to evict a shard when fewer than `quorum_min_peers` other
  // Alive peers hold a copy of the same (start_ts, end_ts) interval.
  // Defaults to FALSE — strictly opt-in.  See § Phase 3 above for the
  // full design.
  quorum_check_enabled: false

  // Minimum number of OTHER live peers that must hold a copy.
  // Ignored when quorum_check_enabled = false.  Default 1.
  quorum_min_peers: 1
}
```

When the `retention:` block is absent the defaults apply (everything
above with `enabled: false` — i.e. the feature is off).

---

## Runtime surface

### `v2/retention.sweep` — operator-triggered sweep

```bash
curl -s -X POST http://node:9000/ -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/retention.sweep","id":1,
       "params":{"duration":"7days","dry_run":true}}'
```

Accepts these override params (all optional):

| Param                    | Type   | Effect |
|--------------------------|--------|--------|
| `duration`               | string | Override `retention.duration` for this call (humantime). |
| `max_evictions_per_run`  | int    | Override the per-call cap.  `0` = no cap. |
| `dry_run`                | bool   | Override `retention.dry_run`. |
| `force`                  | bool   | Force-enable for this call even when `retention.enabled = false`. |

Response:

```json
{
  "enabled":      true,
  "duration_secs": 604800,
  "dry_run":      true,
  "disabled":     false,
  "evicted":      3,
  "errors":       0,
  "freed_bytes":  4567890,
  "cutoff_ts":    1715712100,
  "took_ms":      24,
  "min_start_ts": 1700000000,
  "max_end_ts":   1700700000
}
```

`disabled: true` is the distinguishing flag for the
"`retention.enabled = false` and you didn't pass `force: true`"
case — `evicted` will always be `0` then.

### `v2/retention.settings` — echo active config

```bash
curl -s -X POST http://node:9000/ -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/retention.settings","params":{},"id":1}'
```

```json
{
  "installed":             true,
  "enabled":               true,
  "duration":              "30days",
  "duration_secs":         2592000,
  "interval_secs":         300,
  "max_evictions_per_run": 50,
  "dry_run":               false,
  "reload_drain_after_evict": true,
  "drain_load_duration":   "24h",
  "stats": {
    "evicted_lifetime":     147,
    "evicted_last_run":     2,
    "freed_lifetime_bytes": 12345678901,
    "freed_last_run_bytes": 23456789,
    "last_run_ts":          1715712100,
    "last_run_ms":          412,
    "errors_lifetime":      0
  }
}
```

`installed: false` indicates `server::retention::start` never ran in
this process (test harness, partial init); the stats block is still
populated.

### `v2/status` — retention block

The standard `v2/status` response carries a `retention` block built
from the same process-wide atomic counters:

```json
"retention": {
  "evicted_lifetime":     147,
  "evicted_last_run":     2,
  "freed_lifetime_bytes": 12345678901,
  "freed_last_run_bytes": 23456789,
  "last_run_ts":          1715712100,
  "last_run_ms":          412,
  "errors_lifetime":      0
}
```

### `bdscmd retention-sweep` / `retention-settings`

```bash
# Trigger a sweep, default config
bdscmd retention-sweep

# Preview against a tighter policy
bdscmd retention-sweep --duration 7d --dry-run

# Manual cleanup on a read-mostly node where retention.enabled=false
bdscmd retention-sweep --duration 30d --force

# Echo what bdsnode actually has loaded
bdscmd retention-settings
```

---

## The 5-step eviction procedure

Each shard is evicted by [`ShardsManager::evict_shard`]:

1. **Mark in catalog** — `evicting = TRUE` for the row.  Racing
   ingest calls into [`ShardsCache::shard`] see this flag and fail
   the open with "shard is being evicted", forcing the caller to
   retry or drop.
2. **Close the open Shard** — flushes DuckDB (`CHECKPOINT`), commits
   Tantivy, syncs the VecStore HNSW, saves tplstorage HNSW, drops
   the `ShardsCache` LRU entry.  No-op when the shard wasn't open.
3. **Rename the directory** — `{path} → {path}.evicting`.  If the
   process crashes between here and step 5, startup discovery
   (see "Crash recovery" below) picks it up.
4. **Delete the catalog row** — `DELETE FROM shards WHERE shard_id = ?`.
5. **Recursive `remove_dir_all`** — unlinks the `*.evicting` directory.

Step 2's flush is on a best-effort basis: if the underlying DuckDB
WAL CHECKPOINT errors (e.g. a query is still iterating), the
eviction continues and a WARN log line surfaces the failure.  Steps
3–5 then proceed regardless because POSIX `remove_dir_all` is safe
with open file descriptors — inodes survive until the last close, so
in-flight queries complete against the about-to-be-unlinked files.
(Windows would need a refcount wait; bdslib is POSIX-only.)

[`ShardsManager::evict_shard`]: ../src/shardsmanager.rs
[`ShardsCache::shard`]: ../src/shardscache.rs

## Cache invalidation

After each sweep the following caches are scrubbed:

| Cache               | Scope of invalidation                              |
|---------------------|----------------------------------------------------|
| `ShardsCache` LRU   | The evicted shard's `(start, end)` key only.       |
| `JsonCache`         | `drop_window(min_start_ts, max_end_ts)` covers the union of every shard evicted in the sweep — a single filter pass over the cache. |
| Drain parser        | When `retention.reload_drain_after_evict = true` and the sweep evicted ≥ 1 shard, the entire in-memory parser is re-seeded from current tplstorage rows via [`ShardsManager::drain_reload`].  No-op when drain is disabled. |
| Dashboard cache     | bdsweb's dashboard snapshot expires on its own `dashboard_refresh_secs` cadence — no explicit invalidation needed.  Worst case: dashboard shows one stale shard count for up to 30 s. |
| LLM cache           | Untouched (fully-replicated, not time-sharded).    |

## Crash recovery

`server::retention::start` runs [`ShardsManager::cleanup_orphan_evicting`]
on every bdsnode startup, BEFORE the JSON-RPC listener binds.  This
walks the catalog for `evicting = TRUE` rows left from a crashed
prior sweep, removes any `{path}.evicting` (and `{path}` if the
rename never happened) directories, and deletes the catalog rows.

Log line on startup:

```
[retention] startup: cleaned 2 orphan shard(s) from a previous crashed sweep
```

[`ShardsManager::cleanup_orphan_evicting`]: ../src/shardsmanager.rs

---

## Operator playbook

### First-time enablement

```bash
# 1. Stage the policy in dry-run mode.
sed -i '/retention:/,$d' bds.hjson
cat >> bds.hjson <<EOF
retention: {
  enabled: true
  duration: "30days"
  interval_secs: 300
  dry_run: true
}
EOF

# 2. Restart bdsnode → settings RPC reflects dry_run=true.
bdscmd retention-settings | jq '.dry_run, .duration'

# 3. Run a manual sweep to preview what 30d retention does to your data.
bdscmd retention-sweep | jq '{evicted, freed_bytes, min_start_ts, max_end_ts}'

# 4. If the preview is fine, flip dry_run=false in bds.hjson and restart.
```

### Disk pressure right now

```bash
# Reclaim immediately, capping at 10 shards so we don't stall the node.
bdscmd retention-sweep --duration 7d --max-evictions-per-run 10

# Watch v2/status for confirmation.
watch -n 5 'bdscmd status | jq .retention'
```

### Tightening retention without bulk deletion

```bash
# Currently 365d.  Want 30d.  Don't bulk-delete 11 months in one tick.
# Set retention.duration = "30d" in bds.hjson and leave
# retention.max_evictions_per_run = 50.  At 5min intervals that's 600
# shards/h — works through the backlog in a few hours.
bdscmd retention-settings | jq '.duration, .max_evictions_per_run, .interval_secs'
```

### Forensic recovery

If you accidentally enabled retention with a too-aggressive duration
and lost data: nothing in bdslib resurrects it.  The
`{shard_dir}.evicting` directories are removed during step 5 of
eviction.  Recovery options are filesystem-level (ZFS snapshots,
LVM snapshots, etc.) — outside the scope of this document.

---

## Risks and limitations

1. **Disk-space-based eviction is not supported.**  Time only.
   Monitor `du -sh {dbpath}/` externally and alert on thresholds.
2. **No sub-shard pruning.**  Retention deletes whole shards.  If
   `shard_duration = "1h"` and `retention.duration = "30min"` the
   active shard is never eligible (its `end_ts > now` always), so
   data from "between 0–30 min ago" can hide inside an active
   shard whose total age is up to 1h.
3. **`replication_factor = 1` + retention enabled = irrecoverable
   on eviction.**  bdsnode logs a WARN at startup when this combo
   is detected.
4. **Long-running queries against an evicting shard succeed** on
   POSIX (open FDs keep inodes alive) but the next query against
   the same window misses.
5. **Drain reload after eviction is potentially expensive.**  When
   tplstorage holds thousands of templates the re-seed pass takes
   real time.  Operators can set
   `retention.reload_drain_after_evict = false` if they don't use
   drain.
6. **Manual `rm -rf` of `{shard_dir}` outside of the eviction path
   is unsafe.**  Always use `v2/retention.sweep` so the catalog
   stays in sync.

---

## See also

- [`BDSCONFIG.md`](BDSCONFIG.md) § Shard retention — the canonical
  config reference.
- [`jsonrpc_api/v2_retention.md`](jsonrpc_api/v2_retention.md) —
  the per-method JSON-RPC reference.
- [`BDSCMD.md`](BDSCMD.md) § `retention-sweep` /
  `retention-settings` — CLI usage.
- [`STORAGEENGINE.md`](STORAGEENGINE.md) — `ShardsManager::evict_shard`
  library API.
- [`CLUSTER.md`](CLUSTER.md) § Cluster-wide retention semantics.
