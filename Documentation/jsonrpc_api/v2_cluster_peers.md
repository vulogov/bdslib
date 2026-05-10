# v2/cluster.peers

Read the local peer table without authentication. Intended for **local trusted
clients** (bdsweb's `/cluster` page, observability dashboards) — the same
trust boundary as `v2/status`.

For peer-to-peer gossip use the HMAC-authenticated [`v3/cluster.peers`](v3_cluster_peers.md)
instead. Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

None. The method ignores every supplied parameter.

## Response

When `cluster.enabled = true` on the responding node:

```json
{
  "enabled":  true,
  "node_id":  "019e1018-7c00-7000-9000-7c00000017c0",
  "bind_url": "http://10.0.0.7:9000",
  "mode":     "partial",
  "alive":    1,
  "suspect":  0,
  "dead":     0,
  "full_mode_threshold": 3,
  "replication_factor":  3,
  "embedding_model":     "AllMiniLML6V2",
  "uptime_secs":         42,
  "peers": [
    {
      "node_id":         "019e1019-3c00-7000-9000-3c0000003c00",
      "url":             "http://10.0.0.8:9000",
      "last_seen":       1778386648,
      "state":           "alive",
      "version":         "0.12.0",
      "embedding_model": "AllMiniLML6V2",
      "started_at":      1778386600,
      "miss_count":      0
    }
  ]
}
```

When the cluster layer is disabled:

```json
{
  "enabled": false,
  "peers":   []
}
```

| Field | Type | Description |
|---|---|---|
| `enabled` | bool | `true` when the node has a live cluster layer; `false` for stand-alone deployments. |
| `node_id` | string | This node's stable UUIDv7 (persisted under `<dbpath>/network/node_id`). |
| `bind_url` | string | URL other peers should use to reach this node (`cluster.bind_url`). |
| `mode` | string | One of `standalone` / `partial` / `full`. See [CLUSTER.md § 2](../CLUSTER.md#2-modes). |
| `alive` / `suspect` / `dead` | integer | Peer counts by state. |
| `full_mode_threshold` | integer | Alive-peer count at which mode flips to `full`. |
| `replication_factor` | integer | (Phase 3) target replica count for `v3/add`. |
| `embedding_model` | string \| null | Loaded fastembed model name. |
| `uptime_secs` | integer | Seconds since the cluster layer was constructed. |
| `peers[].state` | string | `alive`, `suspect`, or `dead`. |
| `peers[].last_seen` | integer | Unix seconds of the most recent successful contact. `0` if never seen. |
| `peers[].started_at` | integer | Unix seconds the peer claims it started (from its `cluster.hello`). |
| `peers[].miss_count` | integer | Consecutive failed gossip ticks since the last success. |

## Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/cluster.peers","id":1,"params":{}}' \
  | jq '.result | {mode, alive, suspect, dead, peer_count: (.peers | length)}'
```

## Error responses

| Code | Condition |
|---|---|
| `-32001` | Database unavailable |

There is no auth check on this method, so no `-32096`/`-32097`/`-32098`
responses can occur.

## Notes

- **Why no auth.** Same trust model as `v2/status`. A caller that can reach
  the bdsnode HTTP endpoint can already read every record; exposing the
  membership topology adds no privilege.
- **Snapshot semantics.** The peer list is captured under the read lock at
  call time; no streaming. For very large clusters (>10⁴ peers) consider
  paginating in a future iteration.
