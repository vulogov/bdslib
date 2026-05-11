# v3/user.authenticate

Public login path.  **NOT** HMAC-protected (this is the standard
endpoint users hit).  Rate limiting is enforced by
`cluster.auth_rate_limit_per_minute` in `bds.hjson` (default 10/min).

Recipe:

1. Local verify: look up `username` in this node's `users.duckdb`,
   dispatch the row's `auth_method` to its `CredentialVerifier`,
   argon2-verify the password.
2. Local miss fallback: if the user isn't here yet (AE window
   after a fresh `v3/user.add` on a peer), fan
   `v2/user.get_by_username` with `include_hash: true` out to every
   Alive peer.  First successful verify wins.
3. Issue a stateless HMAC-signed session token
   (`<user_id>.<expires_at>.<hmac>`) using
   `cluster.shared_secret` as the key.

Disabled users, unknown users, and wrong passwords all collapse to
the same `{ok: false, error: "invalid credentials"}` response —
never disclose which leg failed.

See [`CLUSTER_DETAILS.md` § 11.5](../CLUSTER_DETAILS.md) for
protocol details.

## Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `username` | string | yes | |
| `password` | string | yes | Plaintext. |

## Response — success

```json
{
  "ok":            true,
  "user_id":       "019e15a2-4484-7b32-954b-61d63221d32d",
  "session_token": "019e15a2-4484-…1778507919.7c40…25df72d32fbc91404…",
  "ttl_secs":      28800,
  "expires_at":    1778507919
}
```

`session_token` is the value bdsweb drops into the `bds_session`
cookie.  Format: `<user_uuid>.<expires_at_unix_secs>.<hex_hmac_sha256>`.
Single algorithm hard-coded — no JWT `alg=none` confusion possible.

## Response — failure

```json
{ "ok": false, "error": "invalid credentials" }
```

Generic message — the server NEVER tells the caller whether the
failure was unknown-user vs wrong-password vs disabled-account.
Anything else would enable user enumeration.

## Examples

```bash
# bdscmd convenience (no --secret)
bdscmd user authenticate -u alice -p hunter2 | jq

# curl
curl -s -X POST http://10.0.0.5:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"v3/user.authenticate",
       "params":{"username":"alice","password":"hunter2"}}' | jq

# Use the returned token directly as a bdsweb cookie:
TOKEN=$(curl … | jq -r .result.session_token)
curl -H "Cookie: bds_session=$TOKEN" http://10.0.0.5:8080/
```
