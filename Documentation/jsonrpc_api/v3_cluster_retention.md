# v3/cluster.retention.status

Cluster-wide retention introspection.  Read-only fan-out — calls
`v2/retention.settings` on every Alive peer in parallel, plus the
local handler in-process, merges the per-peer results, and adds a
**summary** block that surfaces policy drift between nodes.

Unauthenticated v3/* read surface — matches `v3/timeline` /
`v3/cluster.status` conventions.  No HMAC required.

## Why this exists

Retention is intentionally **per-node** (see
[`../RETENTION.md`](../RETENTION.md) § Cluster semantics): each
peer decides its own policy.  That's a feature — edge nodes can
keep `"6h"` while a long-retention core keeps `"90days"` — but it
also means a misconfigured peer (or a forgotten config drift after
a rolling restart) can silently delete shards earlier than the
operator expected.

`v3/cluster.retention.status` is the audit RPC that lets operators
**see the policy on every peer at once** and flag drift via the
`summary.consistent` boolean.

## Phase 2 vs future phases

This RPC is intentionally **read-only**.  A cluster-wide
`v3/cluster.retention.sweep` would be too easy to misuse
(`--force --duration 1s` against every peer = permanent data
loss).  Manual sweeps stay under the operator's deliberate per-node
control via `v2/retention.sweep`.

A future Phase 3 may add cluster-aware **retention quorum** (refuse
to evict a shard when no other live peer holds a copy).  That's not
in Phase 2 because it would need cluster-wide shard-presence
tracking, which bdslib does not maintain today.

## Parameters

None.  Any params payload is ignored.

## Response

```json
{
  "local": {
    "node_id": "019e22d9-…",
    "url":     "http://127.0.0.1:19021",
    "settings": {
      "installed":               true,
      "enabled":                 true,
      "duration":                "30days",
      "duration_secs":           2592000,
      "interval_secs":           300,
      "max_evictions_per_run":   50,
      "dry_run":                 false,
      "reload_drain_after_evict": true,
      "drain_load_duration":     "24h",
      "stats": { … }
    }
  },
  "peers": [
    {
      "node_id":  "019e22da-…",
      "url":      "http://127.0.0.1:19022",
      "settings": { … same shape as local.settings … }
    },
    {
      "node_id":  "019e22db-…",
      "url":      "http://127.0.0.1:19023",
      "settings": { … duration: "7days" — drift! … }
    }
  ],
  "summary": {
    "total_nodes":                3,
    "consistent":                 false,
    "distinct_durations":         ["30days", "7days"],
    "distinct_interval_secs":     [300],
    "any_disabled":               false,
    "any_dry_run":                false,
    "peers_uninstalled":          0,
    "evicted_lifetime_total":     147,
    "freed_lifetime_bytes_total": 12345678901,
    "errors_lifetime_total":      0,
    "max_last_run_ts":            1715712100
  },
  "cluster_meta": {
    "enabled":        true,
    "peers_queried":  2,
    "peers_answered": 2,
    "partial":        false,
    "failed":         []
  }
}
```

### `local` — this node's view

| Field      | Type   | Notes |
|------------|--------|-------|
| `node_id`  | string | UUID; `null` in standalone mode (no cluster block). |
| `url`      | string | `cluster.bind_url`; `null` in standalone mode. |
| `settings` | object | Same payload `v2/retention.settings` returns for this node — `installed`, `enabled`, `duration`, full stats block. |

### `peers[]` — every Alive peer's view

One entry per peer.  On success the entry carries `node_id`, `url`,
and `settings` (the v2/retention.settings payload from that peer).
On failure the entry carries `node_id`, `url`, and `error` — the
RPC failure message.  This shape mirrors what `cluster::fanout`
returns; clients can render a partial table without dropping the
failed rows.

### `summary` — operator alarm panel

| Field                          | Type    | Meaning |
|--------------------------------|---------|---------|
| `total_nodes`                  | int     | Number of nodes counted (local + peers that returned `settings`). |
| `consistent`                   | bool    | `true` iff every node has the SAME `duration` AND the SAME `interval_secs` AND `peers_uninstalled = 0`. |
| `distinct_durations`           | [string] | Sorted unique humantime strings across all nodes. |
| `distinct_interval_secs`       | [int]   | Sorted unique sweep cadences. |
| `any_disabled`                 | bool    | `true` iff at least one node has `enabled = false`. |
| `any_dry_run`                  | bool    | `true` iff at least one node has `dry_run = true`. |
| `peers_uninstalled`            | int     | Number of nodes whose `installed = false` — typically test harnesses, but on a production cluster means `server::retention::start` never ran (config error or partial init). |
| `evicted_lifetime_total`       | int     | Aggregate `evicted_lifetime` across every responding node. |
| `freed_lifetime_bytes_total`   | int     | Aggregate `freed_lifetime_bytes`. |
| `errors_lifetime_total`        | int     | Aggregate `errors_lifetime`. |
| `max_last_run_ts`              | int     | Latest `last_run_ts` seen — useful for "did anyone sweep recently?". |

**`consistent: false` is the audit signal.**  Followed by
`distinct_durations` it tells operators exactly which knobs drifted.

### `cluster_meta`

Standard v3/* fan-out summary — `enabled`, `peers_queried`,
`peers_answered`, `partial`, `failed[]`.  `partial: true` when one
or more peers failed to answer.  Standalone-mode responses carry
`{enabled: false}` with empty peer counts.

## Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v3/cluster.retention.status","params":{},"id":1}' \
  | jq '.summary, .peers[] | {url, dur: .settings.duration}'
```

Spot a policy-drifted peer:

```bash
bdscmd cluster retention-status \
  | jq -r '
      if .summary.consistent then "OK — every peer agrees"
      else "DRIFT: " + (.summary.distinct_durations | join(", "))
        + " across " + (.summary.total_nodes | tostring) + " nodes"
      end'
```

## Error codes

| Code     | Trigger                                                              |
|----------|----------------------------------------------------------------------|
| `-32000` | Task panicked on the bdsnode tokio pool.                             |
| any v2/* | Individual peer failures land in `peers[].error`, not the RPC error. |

The RPC itself succeeds even when every peer fails to answer —
`cluster_meta.peers_answered = 0` and `peers[].error` carries the
per-peer messages so operators see exactly what's broken.

## See also

- [`../RETENTION.md`](../RETENTION.md) — design overview, per-node
  semantics, cluster considerations.
- [`v2_retention.md`](v2_retention.md) — the underlying
  `v2/retention.settings` / `v2/retention.sweep` reference.
- [`../BDSCMD.md`](../BDSCMD.md) § `cluster retention-status` — CLI usage.
- [`../CLUSTER.md`](../CLUSTER.md) — cluster mode + fan-out
  primitives.
