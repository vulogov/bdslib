# v3/knn

Cluster-wide variant of [`v2/knn`](v2_knn.md). The input fingerprint corpus
is the **union** of every Alive peer's [`v2/fingerprints.recent`](v2_fingerprints_recent.md),
deduped by UUID. The k-NN analysis (`knn_summary_with`) runs once on the
coordinator over the union.

Per-peer analysis followed by summary merge would be incorrect — cluster IDs
and density rankings are corpus-relative, so the right scope for the
analysis is the *whole* deduped corpus.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

Same as `v2/knn`:

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `session` | string | no | `""` | UUIDv7 transaction id (echoed only). |
| `duration` | string | yes | — | Lookback window in humantime. |
| `k` | integer | no | `5` | Neighbours per node in the k-NN graph. |
| `min_word_len` | integer | no | `2` | Tokens shorter than this are dropped before TF-IDF. |
| `anomaly_threshold` | number | no | `0.2` | Top-1 cosine similarity below this flags a fingerprint as anomalous. |
| `max_cluster_members` | integer | no | `10` | Cap on `members[]` per cluster. |
| `max_anomalies` | integer | no | `20` | Cap on the `anomalies[]` array. |

## Response

The same shape as `v2/knn` with three additional top-level fields:

```json
{
  "n_logs":            7,
  "n_unique_fingerprints": 7,
  "n_raw_fingerprints":    7,
  "k":                 5,
  "anomaly_threshold": 0.2,
  "n_clusters":        2,
  "n_anomalies":       1,
  "clusters":          [ … ],
  "anomalies":         [ … ],
  "representatives":   [ … ],
  "cluster_meta": {
    "enabled":        true,
    "peers_queried":  3,
    "peers_answered": 3,
    "partial":        false,
    "failed":         []
  }
}
```

| New field | Description |
|---|---|
| `n_unique_fingerprints` | Fingerprint count after UUID dedup (== `n_logs` on the underlying analysis). |
| `n_raw_fingerprints` | Fingerprints seen across local + every responding peer **before** dedup. The gap between this and `n_unique_fingerprints` is the cross-peer replication overlap. |
| `cluster_meta.*` | See [v3/timeline](v3_timeline.md) for the field reference. |

The full schema for `clusters[]` / `anomalies[]` / `representatives[]` is
documented in [KNN.md § 5](../Algorithm/KNN.md#5-output-contract); v3/knn
returns it verbatim alongside the three coordinator-added fields above.

## Example

```bash
bdscmd cluster knn -d 1h -k 5 --anomaly-threshold 0.15
```

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32004` | Local fingerprint scan failed |
| `-32600` | Invalid `duration` string |
| `-32602` | Missing required parameter |

## Notes

- **Bandwidth cap.** Phase 2 doesn't yet enforce
  `cluster.max_fingerprints_per_peer` on the wire — peers return their
  full fingerprint list. A future revision will add per-peer bounds for
  very-high-cardinality windows.
- **Partial answers downgrade quality, not correctness.** When some peers
  fail to respond (`partial: true`), the analysis runs on a smaller corpus
  but produces well-formed output. Cluster identity and isolated-anomaly
  detection still hold for the records that *did* contribute.
