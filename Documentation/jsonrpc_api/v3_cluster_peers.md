# v3/cluster.peers

Return the receiver's full peer table. Used by the gossip loop every 3rd tick
to converge the membership view across the mesh.

For unauthenticated local-trust reads (bdsweb, observability dashboards) use
[`v2/cluster.peers`](v2_cluster_peers.md) instead.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Authentication

HMAC-SHA256 in `_hmac` over the canonical params (with `_hmac` removed) under
`cluster.shared_secret`. See [`v3/cluster.hello`](v3_cluster_hello.md) for the
exact signing recipe.

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `_hmac` | string | yes | HMAC-SHA256 over the params (without `_hmac`). |

No other fields. Send an empty params object plus the signature.

## Response

```json
{
  "node_id": "019e1018-7c00-7000-9000-7c00000017c0",
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

The full peer-state schema (including the `last_seen` / `state` / `miss_count`
field meanings) is documented in
[`v2/cluster.peers`](v2_cluster_peers.md).

## Example

```bash
SECRET="change-me-32-bytes-or-more"
PARAMS='{}'
HMAC=$(printf %s "$PARAMS" | openssl dgst -sha256 -hmac "$SECRET" -hex | awk '{print $2}')
curl -s -X POST http://10.0.0.7:9000 \
  -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"method\":\"v3/cluster.peers\",\"id\":1,\"params\":{\"_hmac\":\"$HMAC\"}}" \
  | jq '.result.peers[] | {state, url, last_seen}'
```

## Error responses

| Code | Condition |
|---|---|
| `-32001` | Database unavailable |
| `-32097` | Cluster mode disabled on the receiver |
| `-32098` | Missing or invalid `_hmac` |
| `-32600` | Invalid params shape |
