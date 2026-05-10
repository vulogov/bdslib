# v3/search

Cluster-wide semantic vector search. Each peer runs `v2/search` against its
own corpus; the coordinator merges per-peer hit lists, dedups by UUID,
sorts by score (descending), and truncates to `limit`.

When the same UUID appears on multiple peers (Phase 3+ replication), the
peer-reported scores are **averaged** so duplicated replicas never inflate
the rank order. The merged result advertises the replica count via the
`replicas` field on each hit.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

Same shape as `v2/search`:

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `session` | string | no | `""` | UUIDv7 transaction id (echoed only). |
| `query` | string | yes | — | Plain-text query. |
| `duration` | string | yes | — | Lookback window (`"1h"`, `"30min"`). |
| `limit` | integer | no | `10` | Maximum hits returned **after** dedup + sort. |

The same `limit` is forwarded to each peer, then re-applied after merge —
a peer can never return more than `limit` of its own hits, so the worst
case before the post-merge truncation is `limit × (peers + 1)`.

## Response

```json
{
  "results": [
    { "id": "019e102c-…", "timestamp": 1778387802, "score": 0.834, "replicas": 1 },
    { "id": "019e102d-…", "timestamp": 1778387810, "score": 0.812, "replicas": 2 }
  ],
  "cluster_meta": {
    "enabled":        true,
    "peers_queried":  3,
    "peers_answered": 2,
    "partial":        true,
    "failed":         [{"node_id": "…", "url": "http://…", "error": "timeout"}]
  }
}
```

| Field | Description |
|---|---|
| `results[].id` | Primary record UUID. |
| `results[].timestamp` | Unix seconds of the original record. |
| `results[].score` | Cosine similarity. When `replicas > 1`, this is the **mean** of the per-peer scores. |
| `results[].replicas` | Number of peers that returned this UUID. `1` in single-replica deployments. |
| `cluster_meta.*` | See [v3/timeline](v3_timeline.md) for the field reference. |

## Example

```bash
bdscmd cluster search -q "kernel panic" -d 6h --limit 20
```

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc":"2.0", "method":"v3/search", "id":1,
    "params": {"query":"kernel panic","duration":"6h","limit":20}
  }' | jq '.result.results[]'
```

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32004` | Local vector search failed |
| `-32602` | Invalid params (missing required field) |

## Notes

- **Score averaging — when it matters.** With Phase 1 (no replication),
  `replicas` is always `1` and the score equals the local v2/search score.
  Once Phase 3 ships replication, the same UUID can legitimately appear
  with multiple peer-reported scores; averaging keeps the ranking stable
  regardless of how many copies of a record happen to be online at query
  time.
- **`partial: true` is not an error.** Some peers may have timed out; the
  results array contains hits from the peers that did respond. Callers
  that need strict-all-peers semantics should check `partial` and either
  retry or fail at the application layer.
