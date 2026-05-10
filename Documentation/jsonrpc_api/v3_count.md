# v3/count

Cluster-wide variant of [`v2/count`](v2_count.md). Sums per-peer
`v2/count` results across the local node and every Alive peer.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Modes

`v3/count` supports two modes selected via the `distinct` flag:

| `distinct` | Wire shape | Cost | Correctness under replication |
|---|---|---|---|
| `false` (default) | Sums per-peer `v2/count` results. | Cheap (one int per peer). | Overcounts replicated records by ~`replication_factor`. |
| `true` | Fans out `v2/primaries` (UUID lists) and unions the sets. | Bandwidth ~ #records × 36 bytes. | Exact. |

Phase 5 added the `distinct: true` mode to fix the Phase 3 overcounting
caveat.

## Parameters

Same shape as `v2/count` — accept either a duration or an explicit range:

| Parameter | Type | Required | Description |
|---|---|---|---|
| `duration` | string | no | Lookback window in humantime (`"1h"`, `"30min"`). |
| `start_ts` | integer | no | Unix-second range start. Requires `end_ts`. |
| `end_ts` | integer | no | Unix-second range end. Requires `start_ts`. |
| `distinct` | bool | no (default `false`) | Switch to UUID-union mode (Phase 5). |

If neither window is supplied, the count covers all time.

## Response

```json
{
  "count":        5,
  "local_count":  3,
  "distinct":     false,
  "cluster_meta": {
    "enabled":        true,
    "peers_queried":  1,
    "peers_answered": 1,
    "partial":        false,
    "failed":         []
  }
}
```

| Field | Description |
|---|---|
| `count` | In sum mode: total across local + every Alive peer that responded. In distinct mode: \|union of all UUID sets\|. |
| `local_count` | Records counted on the responding node only. |
| `distinct` | Echoes the mode the responder ran in. |
| `cluster_meta.*` | See [v3/timeline](v3_timeline.md) for the field reference. |

## Example

```bash
bdscmd cluster count
bdscmd cluster count -d 1h

# Strict distinct count (UUID-union):
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v3/count","id":1,"params":{"distinct":true}}' | jq
```

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32002` | Shard listing failed |
| `-32003` | Shard open failed |
| `-32004` | Shard count failed |
| `-32602` | Invalid window (e.g. only one of `start_ts`/`end_ts` set) |
