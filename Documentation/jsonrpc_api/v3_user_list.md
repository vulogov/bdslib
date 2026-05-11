# v3/user.list

Hash-free admin listing of every user.  HMAC-required.

The credential hash is **omitted at the SQL boundary** (the
`UserSummary` projection has no `credential_hash` field) so admin
listings can't accidentally leak hashes — a compile-time guarantee
in `cluster::user_store`.

## Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `_hmac` | string | yes | HMAC-SHA256 over canonical params (params can be `{}`). |

## Response

```json
{
  "users": [
    {
      "id":          "019e15a2-4484-7b32-954b-61d63221d32d",
      "username":    "alice",
      "auth_method": "password",
      "metadata":    { "display_name": "Alice" },
      "created_at":  1778479000,
      "updated_at":  1778479060,
      "disabled":    false
    },
    …
  ]
}
```

Sorted alphabetically by username.  No `credential_hash` field.

## Errors

| Code | Condition |
|---|---|
| `-32001` | DB not initialised |
| `-32097` | Cluster mode disabled |
| `-32098` | Missing / invalid `_hmac` |
| `-32011` | UserStorage query failed |

## Examples

```bash
# bdscmd
bdscmd user --secret "$SECRET" list | jq '.users[].username'

# Sanity check across all nodes
for port in 9711 9712 9713; do
  printf "%d: " "$port"
  bdscmd -a "http://127.0.0.1:$port" user --secret "$SECRET" list \
    | jq -r '[.users[].username] | sort | join(",")'
done
# 9711: alice,bob
# 9712: alice,bob
# 9713: alice,bob   ← all nodes converged
```
