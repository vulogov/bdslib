# v2/user.* — receiver methods for cluster replication

Unauthenticated, idempotent methods called by the v3 coordinator
write fan-out (`v3/user.add` / `v3/user.modify` / `v3/user.delete` →
fan-out `v2/user.*` to every Alive peer), hint replay, and the
anti-entropy loop.  All methods are **idempotent under retry**: same
inputs may be re-submitted any number of times safely.

Direct callers (operators, scripts) should use the `v3/user.*` admin
surface or the `bdscmd user …` subcommand instead — these v2
endpoints are part of the data plane.

| Method | Purpose | Idempotency anchor |
|---|---|---|
| [`v2/user.add`](#v2useradd) | Insert a new user row | UUID — re-add is no-op |
| [`v2/user.modify`](#v2usermodify) | Partial update with optional LWW | `if_newer` timestamp gate |
| [`v2/user.delete`](#v2userdelete) | Hard delete + write tombstone | DELETE is idempotent by definition |
| [`v2/user.get_by_username`](#v2usergetbyusername) | Read row by username (optionally with hash) | read-only |
| [`v2/user.get_by_id`](#v2usergetbyid) | Read full row by UUID (used by AE) | read-only |
| [`v2/user.list_ids`](#v2userlistids) | AE id+timestamp listing | read-only |

---

## `v2/user.add`

Insert a user row under the caller-supplied `id`.  If a row with the
same `id` already exists, returns `{id}` without modifying anything
(critical for hint replay correctness).  Duplicate `username` on a
fresh `id` errors.

### Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `id`         | string | yes | UUIDv7 |
| `username`   | string | yes | Unique. |
| `credential` | string | yes | Plaintext.  Hashed via the verifier for `auth_method`. |
| `auth_method`| string | no, default `"password"` | |
| `metadata`   | object | no | Free-form JSON. |
| `now_secs`   | int    | no | Replication coordinator supplies a shared timestamp so all replicas agree.  Defaults to the receiver's wall clock. |

### Response

```json
{ "id": "019e15a2-…" }
```

---

## `v2/user.modify`

Apply a partial update.  Each non-null field replaces; null/missing
fields preserve their existing values.

### Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `id`               | string | yes |   |
| `credential`       | string | no  | New raw credential — re-hashed by the row's verifier. |
| `new_auth_method`  | string | no  | Switch the row's auth method (combine with `credential` to migrate). |
| `metadata`         | object | no  | Replace metadata (not merge). |
| `disabled`         | bool   | no  | Lock / unlock. |
| `if_newer`         | bool   | no, **default `true`** | When true the update is a no-op unless `now_secs > existing.updated_at`.  Coordinators set `false` for their authoritative local commit; replicas keep the default. |
| `now_secs`         | int    | no  | Shared timestamp.  Defaults to wall clock. |

### Response

```json
{ "id": "019e15a2-…", "applied": true }
```

The receiver always returns `applied: true`; when the `if_newer`
gate caused a no-op the row simply doesn't change — the
acknowledgement is for the coordinator's bookkeeping.

---

## `v2/user.delete`

Hard delete + tombstone.  Idempotent: deleting an absent id is a
silent success.

### Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `id`         | string | yes | UUIDv7 |
| `deleted_at` | int    | no  | Tombstone timestamp.  Coordinators supply a shared value; receivers default to wall clock. |

### Response

```json
{ "id": "019e15a2-…", "deleted_at": 1778479479 }
```

---

## `v2/user.get_by_username`

Look up a user by username.  Returns `null` for unknown.

### Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `username`     | string | yes |   |
| `include_hash` | bool   | no, default `false` | When true, the response includes `credential_hash`.  Set true only when the caller needs to verify a credential locally — e.g. the `v3/user.authenticate` fan-out fallback.  Admin listings MUST set false (or omit). |

### Response

```json
{ "user": {
    "id":              "019e15a2-…",
    "username":        "alice",
    "auth_method":     "password",
    "metadata":        { … },
    "created_at":      1778479000,
    "updated_at":      1778479060,
    "disabled":        false,
    "credential_hash": "$argon2id$v=19$m=19456,t=2,p=1$…"   // only when include_hash=true
  } }
```

`{"user": null}` when not found.

---

## `v2/user.get_by_id`

Fetch the full row (always with `credential_hash`) by UUID.  Used
by the anti-entropy pull path to copy a peer's row verbatim into the
local store via `UserStorage::add_with_hash`, bypassing the local
argon2 setup so two nodes can converge on the exact same hash.

### Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | string | yes | UUIDv7 |

### Response

Same shape as `get_by_username` with `include_hash: true`.
`{"user": null}` when not found (race window — peer deleted between
`list_ids` and `get_by_id`).

---

## `v2/user.list_ids`

Anti-entropy id listing.  Returns `live` and `tombstones` arrays in
the standard shape AE expects across stores.

### Parameters

None.

### Response

```json
{
  "live":       [ {"id":"019e15a2-…","updated_at":1778479060}, … ],
  "tombstones": [ {"id":"019e15b1-…","deleted_at":1778479479}, … ]
}
```

`live` is the current `users` table; `tombstones` is the
`tombstones.duckdb` rows scoped to `store = "users"`.  Sorted by id.

---

## Errors (all methods)

| Code | Condition |
|---|---|
| `-32001` | `ShardsManager` not initialised |
| `-32097` | Cluster mode disabled (user store only opens in cluster mode) |
| `-32011` | UserStorage error (typically duplicate username on `add`) |
| `-32602` | Missing required field or invalid UUID |

---

## See also

- [`v3_user_add.md`](v3_user_add.md), [`v3_user_modify.md`](v3_user_modify.md),
  [`v3_user_delete.md`](v3_user_delete.md),
  [`v3_user_authenticate.md`](v3_user_authenticate.md),
  [`v3_user_list.md`](v3_user_list.md) — the cluster-aware coordinator
  surface that consumes these receivers.
- [`../CLUSTER.md` § 13](../CLUSTER.md) — operator view of the user store.
- [`../CLUSTER_DETAILS.md` § 11](../CLUSTER_DETAILS.md) — protocol-level walk-through.
