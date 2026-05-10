# v3/cluster.status

Compact summary of the receiver's cluster state: identity, mode, peer counts,
and replication knobs in effect. Suitable for `bdscmd cluster status` output
and small operator dashboards.

For the full peer list use [`v3/cluster.peers`](v3_cluster_peers.md).
For unauthenticated local-trust reads use [`v2/cluster.peers`](v2_cluster_peers.md)
(which combines status + peer list in one response).

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Authentication

HMAC-SHA256 in `_hmac` over the canonical params (with `_hmac` removed) under
`cluster.shared_secret`.

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `_hmac` | string | yes | HMAC-SHA256 over the params (without `_hmac`). |

## Response

```json
{
  "node_id":             "019e1018-7c00-7000-9000-7c00000017c0",
  "bind_url":            "http://10.0.0.7:9000",
  "uptime_secs":         42,
  "mode":                "partial",
  "alive":               1,
  "suspect":             0,
  "dead":                0,
  "full_mode_threshold": 3,
  "replication_factor":  3,
  "embedding_model":     "AllMiniLML6V2"
}
```

| Field | Description |
|---|---|
| `mode` | One of `standalone` / `partial` / `full`. See [CLUSTER.md § 2](../CLUSTER.md#2-modes). |
| `alive` / `suspect` / `dead` | Peer counts by state. |
| `full_mode_threshold` | Alive-peer count at which mode flips to `full`. |
| `replication_factor` | (Phase 3) target replica count for `v3/add`. |
| `uptime_secs` | Seconds since the cluster layer was constructed (not since the process started). |

## Example

```bash
export BDSCMD_CLUSTER_SECRET="change-me-32-bytes-or-more"
bdscmd --address http://10.0.0.7:9000 cluster status
```

Equivalent raw curl:

```bash
SECRET="change-me-32-bytes-or-more"
PARAMS='{}'
HMAC=$(printf %s "$PARAMS" | openssl dgst -sha256 -hmac "$SECRET" -hex | awk '{print $2}')
curl -s -X POST http://10.0.0.7:9000 \
  -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"method\":\"v3/cluster.status\",\"id\":1,\"params\":{\"_hmac\":\"$HMAC\"}}" \
  | jq
```

## Error responses

| Code | Condition |
|---|---|
| `-32001` | Database unavailable |
| `-32097` | Cluster mode disabled on the receiver |
| `-32098` | Missing or invalid `_hmac` |
| `-32600` | Invalid params shape |
