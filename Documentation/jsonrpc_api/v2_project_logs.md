# v2/project_logs

Project the next likely log/event arrivals over a configurable future
window, trained on this node's primary records from the lookback
window.  Local-only — for the cluster-wide variant that trains on the
union of every Alive peer's recent records, see
[`v3/project_logs`](v3_project_logs.md).

bdsnode reads every primary record whose `ts` falls in
`[now − duration_back, now)`, fingerprints each record (its `key`
followed by `json_fingerprint(data)` — same recipe used by
[`v2/anomaly.recent`](v2_anomaly_recent.md) and the rest of the
n-gram pipeline), and feeds the resulting `(ts, fingerprint)` stream
into `bdslib::analysis::markov::markov_project_timed_with`.  The chain
runs `n_samples` Monte Carlo rollouts forward and emits the modal
state per time bin (with its share-of-samples consensus) as the
projection.

The underlying algorithm — including the drain3 bucketing, K-order
backoff, empirical inter-arrival learning, and many-sample aggregation
— is documented in [`Documentation/Algorithm/MARKOV.md`](../Algorithm/MARKOV.md).

## Parameters

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `duration_back` | string | no | `"1h"` | Humantime lookback for training data (`"30min"`, `"1h"`, `"24h"`). |
| `duration_forward` | string | no | `"30min"` | Humantime projection window — how far to look forward. |
| `order` | integer | no | `2` | Markov order K.  Clamped to `[1, 4]` internally. |
| `n_samples` | integer | no | `50` | Number of Monte Carlo rollouts to aggregate. |
| `time_bins` | integer | no | `20` | Number of bins to slice `duration_forward` into for consensus aggregation. |
| `min_consensus` | number | no | `0.10` | Minimum share-of-samples consensus before an event is emitted (`[0, 1]`). |
| `events_per_second` | number | no | `1.0` | Fallback rate when the empirical gap learner has no observation for a transition. |
| `max_events` | integer | no | `200` | Hard cap on the returned `events[]` array. |
| `smoothing` | number | no | `0.5` | Additive Laplace-style α inside the matched n-gram context. |
| `seed` | integer | no | `null` | RNG seed for reproducible projections. |
| `bucketing` | string | no | `"drain3"` | State bucketing: `"drain3"` (recommended), `"normalize"`, or `"identity"`. |

## Response

```json
{
  "n":                9,
  "duration_back":    "1h",
  "duration_forward": "30min",
  "events": [
    {
      "offset_secs":     30.0,
      "source_state":    "sshd host: <*> message: <*> <*> for user <*>",
      "text":            "sshd host: worker-01 message: authentication ok for user bob pid: 2175 ...",
      "transition_prob": 0.30
    },
    {
      "offset_secs":     150.0,
      "source_state":    "sshd host: <*> message: <*> <*> for user <*>",
      "text":            "sshd host: worker-01 message: authentication ok for user bob pid: 2175 ...",
      "transition_prob": 0.53
    }
  ]
}
```

### Field meanings

| Field | Type | Description |
|---|---|---|
| `n` | integer | Number of events in `events[]` after the `min_consensus` filter and `max_events` cap. |
| `duration_back` | string | Echo of the training window (helpful for caching / dashboards). |
| `duration_forward` | string | Echo of the projection window. |
| `events[].offset_secs` | number | Seconds after "now" the event is projected to land — the midpoint of the time bin that emitted it. |
| `events[].source_state` | string | Operator-readable state label.  With `bucketing="drain3"` (the default) this is the mined drain template, e.g. `kernel: NMI received on CPU <*>`. |
| `events[].text` | string | A concrete exemplar line from the predicted state — the first input line that mapped to this state during training.  Re-emitted verbatim so the operator sees something concrete to grep for. |
| `events[].transition_prob` | number ∈ [0, 1] | Share of the `n_samples` rollouts that placed this state in this time bin.  `1.0` = unanimous; `0.10` = bare-minimum-consensus; anything between is partial agreement. |

When no primary record falls in the training window, or when no state
in any bin meets `min_consensus`, the response carries `n=0` and an
empty `events[]` array.  Invalid humantime strings cause the same
empty response (no error, fail-safe).

## Example

```bash
curl -s -X POST http://127.0.0.1:9711 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc":"2.0","id":1,"method":"v2/project_logs",
    "params":{
      "duration_back":    "30min",
      "duration_forward": "10min",
      "order":            2,
      "n_samples":        30,
      "min_consensus":    0.10,
      "seed":             42
    }
  }'
```

## See also

- [`Documentation/Algorithm/MARKOV.md`](../Algorithm/MARKOV.md) — full
  description of the algorithm, complexity, determinism, and edge
  cases.
- [`v3/project_logs`](v3_project_logs.md) — cluster-wide variant
  trained on the union of every Alive peer's recent primaries.
- [`v2/fingerprints.recent_timed`](v2_fingerprints_recent_timed.md) —
  the per-node primitive `v3/project_logs` fans out to.
- `bdscmd project-logs` — convenience CLI wrapper (calls
  `v3/project_logs` by default).
