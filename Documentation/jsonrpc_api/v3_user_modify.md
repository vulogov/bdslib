# v3/user.modify

Apply partial updates to an existing user.  HMAC-required.

The local commit uses `if_newer = false` (operator intent is
authoritative).  Replicas use `if_newer = true` by default so a stale
hint replay can't clobber a concurrent edit — the standard LWW
mechanism.

## Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `id`               | string | yes | UUIDv7 of the user. |
| `password`         | string | no  | New plaintext.  Server re-hashes with the row's verifier. |
| `metadata`         | object | no  | New metadata object — full replace (not merge). |
| `disabled`         | bool   | no  | Lock / unlock the account. |
| `new_auth_method`  | string | no  | Switch the row's auth method. |
| `_hmac`            | string | yes | HMAC-SHA256 over canonical params. |

Only the fields you supply are touched.  Omitted fields preserve their
existing values.

## Response

```json
{
  "id": "019e15a2-4484-7b32-954b-61d63221d32d",
  "outcome": { "peers_attempted": 2, "peers_succeeded": 2, "hints_queued": 0 },
  "cluster_meta": { "enabled": false }
}
```

## Errors

| Code | Condition |
|---|---|
| `-32001` | DB not initialised |
| `-32097` | Cluster mode disabled |
| `-32098` | Missing / invalid `_hmac` |
| `-32011` | UserStorage rejection (typically: id not found) |
| `-32602` | Missing `id` or invalid UUID |

## Examples

```bash
# Reset password + disable in one call (bdscmd convenience)
bdscmd user --secret "$SECRET" modify -i 019e15a2-… -p NEWPASS --disable

# Re-enable later
bdscmd user --secret "$SECRET" modify -i 019e15a2-… --enable

# Or via curl (you'd compute _hmac yourself):
curl -s -X POST http://10.0.0.5:9000 -d '{
  "jsonrpc":"2.0","id":1,"method":"v3/user.modify",
  "params":{"id":"…","password":"new","_hmac":"<hex>"}
}'
```
