# v3/cluster.hello

Handshake that two peers exchange the first time they meet. The caller sends
its identity; the receiver registers the caller in its peer table and echoes
back its own identity plus its current peer view, so a single round-trip is
enough to converge a fresh node.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Authentication

Every `v3/cluster.*` request must carry a `_hmac` field whose value is the
hex-encoded HMAC-SHA256 of the canonical params (with `_hmac` removed) under
`cluster.shared_secret`.

```
_hmac = hex(HMAC-SHA256(shared_secret, json(params_without_hmac)))
```

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `node_id` | string | yes | Caller's stable UUIDv7. |
| `bind_url` | string | yes | URL the caller wants to be contacted on. |
| `version` | string | no | Caller's bdslib version (e.g. `"0.12.0"`). |
| `embedding_model` | string \| null | no | Caller's fastembed model name. |
| `started_at` | integer | no | Unix seconds the caller started. |
| `_hmac` | string | yes | HMAC-SHA256 over the params (without `_hmac`). |

## Response

```json
{
  "node_id":         "019e1018-7c00-7000-9000-7c00000017c0",
  "bind_url":        "http://10.0.0.7:9000",
  "version":         "0.12.0",
  "embedding_model": "AllMiniLML6V2",
  "started_at":      1778386600,
  "peers": [
    { "node_id": "...", "url": "...", "state": "alive", "last_seen": 1778386648,
      "version": "0.12.0", "embedding_model": "AllMiniLML6V2",
      "started_at": 1778386580, "miss_count": 0 }
  ]
}
```

The `peers` array is the receiver's full peer table snapshot. The caller is
expected to merge it into its own table (last-seen wins).

## Example

```bash
SECRET="change-me-32-bytes-or-more"
PARAMS='{"node_id":"019e1019-3c00-7000-9000-3c0000003c00","bind_url":"http://10.0.0.8:9000","version":"0.12.0","embedding_model":"AllMiniLML6V2","started_at":1778386600}'
HMAC=$(printf %s "$PARAMS" | openssl dgst -sha256 -hmac "$SECRET" -hex | awk '{print $2}')
curl -s -X POST http://10.0.0.7:9000 \
  -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"method\":\"v3/cluster.hello\",\"id\":1,\"params\":{\"node_id\":\"019e1019-3c00-7000-9000-3c0000003c00\",\"bind_url\":\"http://10.0.0.8:9000\",\"version\":\"0.12.0\",\"embedding_model\":\"AllMiniLML6V2\",\"started_at\":1778386600,\"_hmac\":\"$HMAC\"}}" \
  | jq
```

In Rust, `bdslib::cluster::rpc_client::cluster_hello` does this for you.

## Error responses

| Code | Condition |
|---|---|
| `-32001` | Database unavailable |
| `-32096` | Embedding-model mismatch (caller's `embedding_model` differs from receiver's). HNSW indexes are dimension-locked, so federated vector search across mixed-dimension peers would silently return wrong results; we refuse to peer rather than corrupting future replication. |
| `-32097` | Cluster mode disabled on the receiver (`cluster.enabled = false`). |
| `-32098` | Missing or invalid `_hmac` (wrong shared secret, or tampered body). |
| `-32600` | Invalid params shape (not a JSON object, missing `node_id`/`bind_url`, malformed UUID). |

## Notes

- **Idempotency.** Re-sending `cluster.hello` from the same caller updates
  the existing peer entry (URL, version, embedding model) rather than
  creating a duplicate. Useful for a node whose `bind_url` changes.
- **Embedding-model field.** Empty string is treated as "not provided" and
  is **not** rejected — only a non-empty mismatch triggers `-32096`.
- **No replay protection in Phase 1.** The HMAC alone protects integrity and
  authenticity; nonce/timestamp replay protection is on the Phase 2 list.
  In trusted-network deployments this is acceptable; consider TLS at the
  transport layer for hostile networks.
