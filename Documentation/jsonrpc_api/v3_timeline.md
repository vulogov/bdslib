# v3/timeline

Cluster-wide variant of [`v2/timeline`](v2_timeline.md). Calls
`v2/timeline` on every Alive peer in parallel, then merges the responses by
taking `min(min_ts)` and `max(max_ts)` across the local node and every peer.

Falls back to local-only when cluster mode is disabled (`cluster.enabled =
false`) or no Alive peers exist (Standalone mode).

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

None.

## Response

```json
{
  "min_ts": 1778387802,
  "max_ts": 1778387900,
  "cluster_meta": {
    "enabled":        true,
    "peers_queried":  3,
    "peers_answered": 3,
    "partial":        false,
    "failed":         []
  }
}
```

| Field | Description |
|---|---|
| `min_ts` / `max_ts` | Global earliest / latest timestamps across the cluster (Unix seconds). `null` when no data exists anywhere. |
| `cluster_meta.enabled` | `true` when the responder has cluster mode on; `false` for stand-alone. |
| `cluster_meta.peers_queried` | Number of Alive peers we attempted to contact. |
| `cluster_meta.peers_answered` | Number of peers that returned successfully. |
| `cluster_meta.partial` | `true` when `peers_answered < peers_queried` — the result includes only the peers that responded. |
| `cluster_meta.failed[]` | Per-peer failure details (`node_id`, `url`, `error`). |

## Example

```bash
bdscmd cluster timeline
```

Or via raw HTTP:

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v3/timeline","id":1,"params":{}}' | jq
```

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |

## Notes

- **Partial answers are not errors.** A peer that times out or returns an
  error gets recorded in `cluster_meta.failed[]`, but the call succeeds
  with whatever the responding peers contributed. Callers that need
  strict-all-peers semantics should check `partial` and either retry or
  fail at the application layer.
- **Per-peer timeout.** Set by `cluster.peer_rpc_timeout` (default 2s).
