# v2/retention.sweep · v2/retention.settings

Operator-facing surface for time-based shard retention.  The
underlying mechanics live in [`../RETENTION.md`](../RETENTION.md).

Both methods are **unauthenticated v2/*** — the bdsnode RPC port is
the trust boundary.  Operators who need HMAC on retention should run
bdsnode behind a reverse proxy that enforces it (loopback-only
binding is the common pattern).

---

## `v2/retention.sweep`

Trigger a one-shot retention sweep.  Reuses the same code path the
background tokio task drives (`bdslib::retention::evict_expired`),
so the outcome is exactly what would have happened during the next
periodic tick had the operator not called this RPC.

### Parameters

All optional.  Empty `params: {}` runs a sweep against the active
config.

| Field                    | Type   | Description |
|--------------------------|--------|-------------|
| `duration`               | string | Humantime override for `retention.duration` (e.g. `"7days"`, `"6h"`).  Must be > 0. |
| `max_evictions_per_run`  | int    | Per-call cap.  `0` = no cap. |
| `dry_run`                | bool   | Log what would be evicted but don't act.  Overrides `retention.dry_run`. |
| `force`                  | bool   | Force-enable for this call when `retention.enabled = false` in `bds.hjson`.  Useful for ad-hoc cleanup on a read-mostly node. |

### Response

```json
{
  "enabled":      true,
  "duration_secs": 604800,
  "dry_run":      false,
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

| Field          | Meaning |
|----------------|---------|
| `enabled`      | Effective enabled flag for this call (true when `force=true`). |
| `duration_secs`| Retention window actually applied, in seconds. |
| `dry_run`      | Echo — did this run actually mutate state? |
| `disabled`     | `true` only when `enabled=false` and the sweep short-circuited.  `evicted` is then always `0`. |
| `evicted`      | Number of shards that were (or would have been) removed. |
| `errors`       | Number of per-shard failures.  Per-shard errors are logged and counted but never abort the sweep. |
| `freed_bytes`  | Aggregate bytes reclaimed.  `0` in dry-run. |
| `cutoff_ts`    | Unix seconds — every shard whose `end_ts <` this value was considered. |
| `took_ms`      | Wall-clock cost. |
| `min_start_ts` / `max_end_ts` | Union range of evicted shards (zero when nothing was evicted).  Used internally to scope the JsonCache invalidation. |

### Example

```bash
# Preview a tighter 7-day policy against a node currently running 30 days.
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/retention.sweep","id":1,
       "params":{"duration":"7days","dry_run":true}}' \
  | jq

# Reclaim ≥ 10 shards immediately (cap raised from the default 50).
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/retention.sweep","id":1,
       "params":{"max_evictions_per_run":0}}'
```

### Error codes

| Code     | Trigger                                                                |
|----------|------------------------------------------------------------------------|
| `-32000` | Task panicked on the bdsnode tokio pool.                               |
| `-32004` | `evict_expired` returned an error (catalog unreachable, etc.).         |
| `-32600` | Invalid `duration` (unparseable humantime string, or zero duration).   |

---

## `v2/retention.settings`

Read-only echo of the live retention configuration plus lifetime
counters.  Useful for `bdscmd retention-settings` to confirm what's
actually loaded after a config reload, and for monitoring tooling.

### Parameters

None.  Any payload is ignored.

### Response

```json
{
  "installed":                true,
  "enabled":                  true,
  "duration":                 "30days",
  "duration_secs":            2592000,
  "interval_secs":            300,
  "max_evictions_per_run":    50,
  "dry_run":                  false,
  "reload_drain_after_evict": true,
  "drain_load_duration":      "24h",
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

| Field                       | Meaning |
|-----------------------------|---------|
| `installed`                 | `true` when `server::retention::start` has run in this process.  `false` for test harnesses / partial inits — only the `stats` block is populated then. |
| `enabled` …`reload_drain_after_evict` | Echo of the parsed `retention.*` block from `bds.hjson`. |
| `drain_load_duration`       | Top-level `drain_load_duration` setting that the drain-reload step uses after a sweep evicts anything. |
| `stats.*`                   | Atomic counters maintained by `bdslib::retention::record_run` after every sweep — same numbers `v2/status` surfaces under its own `retention` block. |

`duration` is rendered via `humantime::format_duration` so it
round-trips cleanly with what's in `bds.hjson` (e.g. `"30days"`,
`"6h"`).

### Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/retention.settings","params":{},"id":1}' \
  | jq '.result.enabled, .result.duration, .result.stats.evicted_lifetime'
```

---

## See also

- [`../RETENTION.md`](../RETENTION.md) — design overview, 5-step
  eviction procedure, cache invalidation, operator playbook.
- [`../BDSCONFIG.md`](../BDSCONFIG.md) § Retention — the
  `retention.*` config block reference.
- [`../BDSCMD.md`](../BDSCMD.md) § `retention-sweep` /
  `retention-settings` — CLI usage.
- [`v2_status.md`](v2_status.md) — the `retention` block in the
  standard status response.
