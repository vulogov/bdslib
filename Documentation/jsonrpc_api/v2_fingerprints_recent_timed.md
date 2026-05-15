# v2/fingerprints.recent_timed

Return `(uuid, ts, fingerprint)` triples for every primary record
observed in the lookback window — the timed sibling of
[`v2/fingerprints.recent`](v2_fingerprints_recent.md).

Designed as the per-node primitive that [`v3/project_logs`](v3_project_logs.md)
fans out to.  The semi-Markov projection algorithm needs the
inter-arrival timestamps to learn the empirical gap distribution (see
[`Documentation/Algorithm/MARKOV.md`](../Algorithm/MARKOV.md) §4.3), so
the plain `v2/fingerprints.recent` shape — `(uuid, fingerprint)` — is
not enough.  The coordinator collects triples from every Alive peer,
dedupes by UUID, sorts chronologically, and runs the projection once
over the union.

This is the same fan-out + dedup + single-analysis pattern that
`v3/knn`, `v3/anomaly.recent`, and `v3/denoise.recent` follow.

## Parameters

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `duration` | string | yes | — | Lookback window in humantime (`"30min"`, `"1h"`, `"24h"`). |

## Response

```json
{
  "n":            32,
  "fingerprints": [
    {
      "id":          "019e3fa1-1f57-7c25-9213-b7d1a4f3a002",
      "ts":          1700000042,
      "fingerprint": "sshd host: worker-01 message: authentication ok ..."
    },
    {
      "id":          "019e3fa1-23e3-7e4a-89c1-3ee5fbe5c0a1",
      "ts":          1700000051,
      "fingerprint": "kernel host: worker-01 message: NMI received on CPU 3 ..."
    }
  ]
}
```

| Field | Description |
|---|---|
| `n` | Number of triples returned. |
| `fingerprints[].id` | UUIDv7 of the primary record. |
| `fingerprints[].ts` | Unix seconds when the record was observed. |
| `fingerprints[].fingerprint` | The record's `key` followed by `json_fingerprint(data)` — same recipe `v2/fingerprints.recent` uses. |

Records whose fingerprint is empty are silently skipped.  Ordering of
the returned array is unspecified — the v3 coordinator sorts by `ts`
ascending before feeding the result to the projection.

## Example

```bash
curl -s -X POST http://127.0.0.1:9711 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"v2/fingerprints.recent_timed",
       "params":{"duration":"30min"}}'
```

## See also

- [`v2/fingerprints.recent`](v2_fingerprints_recent.md) — sibling
  without timestamps; the per-peer primitive for `v3/knn`,
  `v3/anomaly.recent`, `v3/denoise.recent`.
- [`v3/project_logs`](v3_project_logs.md) — the cluster-wide caller
  for this RPC.
- [`Documentation/Algorithm/MARKOV.md`](../Algorithm/MARKOV.md) — why
  the empirical gap distribution matters.
