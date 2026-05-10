# v3/denoise.recent

Cluster-wide variant of [`v2/denoise.recent`](v2_denoise_recent.md). Same
fan-out recipe as [`v3/knn`](v3_knn.md) and [`v3/anomaly.recent`](v3_anomaly_recent.md):
each peer's [`v2/fingerprints.recent`](v2_fingerprints_recent.md) feeds the
union; the analysis (`ngram_remove_noise_with`) runs once on the coordinator
over the deduped corpus.

Per-peer analysis followed by summary merge would mis-classify — noise
scores are corpus-relative, so the right scope is the whole deduped corpus.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

Same as `v2/denoise.recent`:

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `session` | string | no | `""` | UUIDv7 transaction id (echoed only). |
| `duration` | string | yes | — | Lookback window. |
| `n` | integer | no | `2` | N-gram length. |
| `min_word_len` | integer | no | `2` | Tokens shorter than this are dropped. |
| `noise_threshold` | number | no | `0.85` | Mean commonness at-or-above this classifies a fingerprint as noise. |
| `max_kept` | integer | no | `100` | Cap on the `kept[]` array. |
| `max_removed` | integer | no | `100` | Cap on the `removed[]` array. |

## Response

The same shape as `v2/denoise.recent` plus the standard `cluster_meta`
block and the cross-peer fingerprint counts:

```json
{
  "n_logs":            7,
  "n_unique_fingerprints": 7,
  "n_raw_fingerprints":    7,
  "n":                 2,
  "n_unique_ngrams":   34,
  "noise_threshold":   0.85,
  "n_kept":            5,
  "n_removed":         2,
  "kept":              [ … ],
  "removed":           [ … ],
  "cluster_meta":      { … }
}
```

See [v3/knn](v3_knn.md) for the meaning of `n_unique_fingerprints` /
`n_raw_fingerprints`. The full schema for `kept[]` and `removed[]` is in
`Documentation/Algorithm/NGRAM_NOISE.md`.

## Example

```bash
bdscmd cluster denoise -d 6h --noise-threshold 0.5
```

## Error responses

Same as [`v3/knn`](v3_knn.md).
