# v3/cluster.ping

Lightweight liveness probe used by the gossip loop. Receivers do not update
their peer table on `ping`; they just respond with their own identity and
current wall-clock timestamp so the caller can confirm the peer is alive and
record the success in its own table.

For the heavier handshake that exchanges peer views, use
[`v3/cluster.hello`](v3_cluster_hello.md).

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
  "node_id": "019e1018-7c00-7000-9000-7c00000017c0",
  "ts":      1778386700
}
```

| Field | Description |
|---|---|
| `node_id` | Receiver's stable UUIDv7. Lets the caller verify it talked to the expected peer. |
| `ts` | Receiver's wall-clock Unix seconds at response time. |

## Example

```bash
SECRET="change-me-32-bytes-or-more"
PARAMS='{}'
HMAC=$(printf %s "$PARAMS" | openssl dgst -sha256 -hmac "$SECRET" -hex | awk '{print $2}')
curl -s -X POST http://10.0.0.7:9000 \
  -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"method\":\"v3/cluster.ping\",\"id\":1,\"params\":{\"_hmac\":\"$HMAC\"}}" \
  | jq
```

## Error responses

| Code | Condition |
|---|---|
| `-32001` | Database unavailable |
| `-32097` | Cluster mode disabled on the receiver |
| `-32098` | Missing or invalid `_hmac` |
| `-32600` | Invalid params shape |

## Notes

- **Default timeout.** The gossip loop uses `cluster.peer_rpc_timeout`
  (default 2 s). Pings that exceed it are recorded as a miss and bump
  `miss_count`; consecutive misses promote the peer through Suspect → Dead
  per `cluster.suspect_timeout` and `cluster.dead_timeout`.
- **No side effects.** Unlike `cluster.hello`, this method does not touch
  the receiver's peer table or persistent storage.
