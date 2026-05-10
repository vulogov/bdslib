# v2/fingerprints.recent

Return raw `(uuid, fingerprint)` pairs for every primary record observed in
the lookback window. Designed as the input source for the `v3/*` distributed
analytics endpoints.

Each peer in a cluster returns its local fingerprints; the coordinator dedups
by UUID and runs the analysis once over the union. This is the only
mathematically-correct way to fan analysis across peers — running KNN /
anomaly / noise scoring per-peer and merging summaries gives wrong results
because all three metrics are corpus-relative.

The fingerprint string itself is identical to the one used by the local
[`v2/anomaly.recent`](v2_anomaly_recent.md), [`v2/denoise.recent`](v2_denoise_recent.md),
and [`v2/knn`](v2_knn.md) methods:

```
"<key with .  _  - → spaces>  <json_fingerprint(data)>"
```

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `duration` | string | yes | Lookback window in humantime notation (`"1h"`, `"30min"`, `"7d"`). |

## Response

```json
{
  "n": 2,
  "fingerprints": [
    {
      "id":          "019e102c-5790-7156-b178-4a8fd5e87ce6",
      "fingerprint": "app error  msg: distinct boom 1 pattern"
    },
    {
      "id":          "019e102c-5793-7c21-9e3f-1a4f8b6e9d22",
      "fingerprint": "app warning  msg: completely different warning text"
    }
  ]
}
```

| Field | Description |
|---|---|
| `n` | Total fingerprints returned (== `fingerprints.length`). |
| `fingerprints[].id` | UUIDv7 of the primary record. Stable across peers — used by callers as the dedup key. |
| `fingerprints[].fingerprint` | Per-record fingerprint string (key + json_fingerprint(data)). |

## Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/fingerprints.recent","id":1,"params":{"duration":"1h"}}' \
  | jq '.result | {n, sample: .fingerprints[0]}'
```

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32004` | Shard scan failed |
| `-32600` | Invalid `duration` string |

## Notes

- **Primaries only.** The walk uses the same `list_primaries_with_data_in_range`
  helper as the n-gram and KNN endpoints, so secondary records (records that
  fell on the wrong side of `similarity_threshold` and were collapsed into a
  primary) are excluded.
- **Empty fingerprints are skipped.** Records whose key + data both flatten
  to an empty string contribute nothing to the output.
- **Unbounded payload.** No `limit` parameter — the caller (typically a v3/*
  coordinator) is expected to enforce a per-peer cap via
  `cluster.max_fingerprints_per_peer` if needed.
