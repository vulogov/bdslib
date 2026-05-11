# v3/user.add

Create a new user in the cluster-replicated user store, then fan the
write out to every Alive peer.

HMAC-protected (`_hmac` field over canonical params, key =
`cluster.shared_secret`) — **except** the first call on an empty user
store, which is admitted unsigned to support first-user bootstrap.

See [`CLUSTER.md` § 13](../CLUSTER.md) for architecture and
[`CLUSTER_DETAILS.md` § 11.2](../CLUSTER_DETAILS.md) for the wire
protocol.

## Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `username`    | string | yes | Unique cluster-wide username. |
| `password`    | string | yes | Plaintext.  Hashed locally on each replica with argon2id. |
| `auth_method` | string | no, default `"password"` | One of `password`, `oauth-<provider>`, `ldap-<server>`, `custom-<name>`.  Verifier must be registered at startup. |
| `metadata`    | object | no | Free-form JSON; conventionally `{display_name, email}`. |
| `id`          | string | no | Caller-supplied UUIDv7.  When omitted a fresh id is minted (used by the coordinator path; replicas always carry it). |
| `_hmac`       | string | yes (except bootstrap) | HMAC-SHA256 over canonical params with `_hmac` removed.  Key = `cluster.shared_secret`. |

## Response

```json
{
  "id": "019e15a2-4484-7b32-954b-61d63221d32d",
  "outcome": {
    "peers_attempted": 2,
    "peers_succeeded": 2,
    "hints_queued":    0
  },
  "cluster_meta": { "enabled": false }
}
```

`outcome` describes the per-replica fan-out result.  `hints_queued`
counts peers that were Alive at the start of fan-out but failed
mid-call; the standard hint replay loop retries them on the next
`hint_replay_interval`.

## Errors

| Code | Condition |
|---|---|
| `-32001` | `ShardsManager` not initialised |
| `-32097` | Cluster mode disabled on this node |
| `-32098` | Missing or invalid `_hmac` (and not first-user bootstrap) |
| `-32011` | UserStorage rejection — typically duplicate username |
| `-32602` | Missing required field or invalid `id` UUID |

## Examples

```bash
# First-user bootstrap (no HMAC required)
curl -s -X POST http://10.0.0.5:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"v3/user.add","params":{
        "username":"alice","password":"hunter2","metadata":{"display_name":"Alice"}
      }}' | jq

# Later, HMAC required (bdscmd is the easy path)
bdscmd user --secret "$CLUSTER_SECRET" add -u bob -p hunter3 -n "Robert"
```
