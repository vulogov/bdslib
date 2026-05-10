# v3/cluster.sync

Force an immediate hint replay + anti-entropy tick.  Operator escape
hatch when you've just brought a peer back online and don't want to wait
for the periodic loops (`cluster.hint_replay_interval`, default 10s, and
`cluster.antientropy_interval`, default 5min) to schedule the catch-up.

Updates the same `stats.last_*` telemetry fields the periodic loops
write, so subsequent `v2/cluster.peers` / `v3/cluster.status` calls
reflect the on-demand work.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Authentication

HMAC-SHA256 in `_hmac` over the canonical params (with `_hmac` removed)
under `cluster.shared_secret`.  Same recipe as the other `v3/cluster.*`
methods — see [`v3/cluster.hello`](v3_cluster_hello.md).

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `_hmac` | string | yes | HMAC-SHA256 over the params (without `_hmac`). |

No other fields.

## Response

```json
{
  "node_id":         "019e1123-f9dc-7fa3-8e22-f774fb35ca09",
  "hints_replayed":  3,
  "ae_pulled":       2,
  "ae_tombstones":   1,
  "ae_pruned":       0,
  "hint_backlog":    0,
  "tombstone_total": 4
}
```

| Field | Description |
|---|---|
| `hints_replayed` | Number of hints successfully retried during this tick. |
| `ae_pulled` | Live entries pulled from peers (across all replicated stores). |
| `ae_tombstones` | Tombstones applied locally during this tick. |
| `ae_pruned` | Tombstones GC'd during this tick (older than `cluster.hint_max_age * 2`). |
| `hint_backlog` | Hints still queued (i.e., for peers that are currently down). |
| `tombstone_total` | Tombstones currently in the store. |

## Example

```bash
export BDSCMD_CLUSTER_SECRET="change-me-32-bytes-or-more"
bdscmd --address http://10.0.0.7:9000 cluster sync
```

## Error responses

| Code | Condition |
|---|---|
| `-32001` | Database unavailable |
| `-32097` | Cluster mode disabled on the receiver |
| `-32098` | Missing or invalid `_hmac` |

## Notes

- **No-op when standalone.** When the receiver has zero Alive peers,
  `ae_pulled` stays at 0 (anti-entropy needs a peer to diff against).
  Hint replay still runs but has nowhere to send the retries.
- **Per-peer ordering preserved.** The hint replay path is the same one
  the periodic loop uses (drains in seq order per peer); calling sync
  doesn't reorder anything.
