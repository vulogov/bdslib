# v3/project_logs

Cluster-wide variant of [`v2/project_logs`](v2_project_logs.md).  Same
parameters and same response shape, plus the standard `cluster_meta`
block; the input corpus is the *union* of every Alive peer's
[`v2/fingerprints.recent_timed`](v2_fingerprints_recent_timed.md),
deduped by UUID and sorted chronologically.  The semi-Markov projection
runs **once on the coordinator** over the union.

This is the same "single-analysis-on-union" correctness invariant that
[`v3/anomaly.recent`](v3_anomaly_recent.md), [`v3/knn`](v3_knn.md),
and [`v3/denoise.recent`](v3_denoise_recent.md) follow.  Running the
projection per-peer and concatenating outputs would be incorrect — the
empirical inter-arrival distribution is corpus-relative, and so are
the n-gram backoff transitions.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).
Algorithm reference: [`Documentation/Algorithm/MARKOV.md`](../Algorithm/MARKOV.md).

## Parameters

Same as `v2/project_logs`:

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `duration_back` | string | no | `"1h"` | Humantime lookback for training data. |
| `duration_forward` | string | no | `"30min"` | Humantime projection window. |
| `order` | integer | no | `2` | Markov order K (clamped `[1, 4]`). |
| `n_samples` | integer | no | `50` | Monte Carlo rollouts. |
| `time_bins` | integer | no | `20` | Aggregation bin count over `duration_forward`. |
| `min_consensus` | number | no | `0.10` | Minimum share-of-samples consensus per emitted event. |
| `events_per_second` | number | no | `1.0` | Fallback gap rate when no empirical observation matches. |
| `max_events` | integer | no | `200` | Cap on the `events[]` array. |
| `smoothing` | number | no | `0.5` | Additive α inside the matched n-gram context. |
| `seed` | integer | no | `null` | RNG seed (set for reproducible projections / shared snapshots). |
| `bucketing` | string | no | `"drain3"` | `"drain3"` / `"normalize"` / `"identity"`. |

## Response

The same shape as `v2/project_logs`, plus the standard `cluster_meta`
block and two cross-peer input counts:

```json
{
  "n":                9,
  "n_unique_inputs":  32,
  "n_raw_inputs":     32,
  "duration_back":    "1h",
  "duration_forward": "30min",
  "events":           [ … ],
  "cluster_meta": {
    "enabled":        true,
    "partial":        false,
    "peers_queried":  2,
    "peers_answered": 2,
    "failed":         []
  }
}
```

| Field | Description |
|---|---|
| `n_unique_inputs` | Distinct primary records in the union, deduped by UUID, that the projection trained on. |
| `n_raw_inputs` | Sum of per-peer record counts before dedup.  Equal to `n_unique_inputs` when no record is replicated across multiple peers. |
| `cluster_meta` | Standard v3 fan-out metadata: how many peers were queried, how many answered, and which ones failed.  `partial = true` when at least one peer was queried but did not respond — the projection still runs on the partial union. |

See [`v3/anomaly.recent`](v3_anomaly_recent.md) for the full
`cluster_meta` schema.

## Example

```bash
curl -s -X POST http://127.0.0.1:9711 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc":"2.0","id":1,"method":"v3/project_logs",
    "params":{
      "duration_back":    "1h",
      "duration_forward": "30min",
      "n_samples":        50,
      "min_consensus":    0.10,
      "seed":             42
    }
  }'
```

Example abbreviated response on a 3-node test cluster:

```
n=9  n_unique_inputs=32  n_raw_inputs=32
+30.0s   p=0.30  sshd host: <*> message: <*> <*> for user <*>
+150.0s  p=0.53  sshd host: <*> message: <*> <*> for user <*>
+210.0s  p=0.47  sshd host: <*> message: <*> <*> for user <*>
+270.0s  p=0.17  sshd host: <*> message: <*> <*> for user <*>
+330.0s  p=0.13  sshd host: <*> message: <*> <*> for user <*>
+390.0s  p=0.60  sshd host: <*> message: <*> <*> for user <*>
...
cluster_meta: peers_queried=2 peers_answered=2 partial=false
```

The `bdscmd project-logs` subcommand calls this endpoint and
pretty-prints the JSON.

## See also

- [`Documentation/Algorithm/MARKOV.md`](../Algorithm/MARKOV.md) —
  full algorithm reference.
- [`v2/project_logs`](v2_project_logs.md) — local-only variant.
- [`v2/fingerprints.recent_timed`](v2_fingerprints_recent_timed.md) —
  the per-peer primitive this RPC fans out to.
- bdsweb `Analysis → Project events` page — interactive surface backed
  by this RPC, with an `"Analyze this!"` LLM review button.
