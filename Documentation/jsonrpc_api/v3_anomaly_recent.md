# v3/anomaly.recent

Cluster-wide variant of [`v2/anomaly.recent`](v2_anomaly_recent.md). Same
fan-out recipe as [`v3/knn`](v3_knn.md): each peer's
[`v2/fingerprints.recent`](v2_fingerprints_recent.md) feeds the union; the
analysis (`ngram_anomaly_with`) runs once on the coordinator over the
deduped corpus.

Per-peer analysis followed by summary merge would be incorrect — n-gram
phrase-rarity scores are corpus-relative.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

Same as `v2/anomaly.recent`:

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `session` | string | no | `""` | UUIDv7 transaction id (echoed only). |
| `duration` | string | yes | — | Lookback window. |
| `n` | integer | no | `2` | N-gram length (1=unigram, 2=bigram, 3=trigram). |
| `min_word_len` | integer | no | `2` | Tokens shorter than this are dropped. |
| `anomaly_threshold` | number | no | `0.7` | Records with mean-rarity above this are flagged. |
| `max_anomalies` | integer | no | `20` | Cap on the `anomalies[]` array. |
| `max_novel_ngrams` | integer | no | `5` | Cap on novel-ngram detail per anomaly. |

## Response

The same shape as `v2/anomaly.recent` plus the standard `cluster_meta`
block and the cross-peer fingerprint counts:

```json
{
  "n_logs":            7,
  "n_unique_fingerprints": 7,
  "n_raw_fingerprints":    7,
  "n_unique_ngrams":   34,
  "anomaly_threshold": 0.7,
  "anomalies":         [ … ],
  "cluster_meta":      { … }
}
```

See [v3/knn](v3_knn.md) for the meaning of `n_unique_fingerprints` and
`n_raw_fingerprints`. The full schema for `anomalies[]` is in
`Documentation/Algorithm/NGRAM_ANOMALY.md`.

## Example

```bash
bdscmd cluster anomaly -d 1h --anomaly-threshold 0.6
```

## Error responses

Same as [`v3/knn`](v3_knn.md).
