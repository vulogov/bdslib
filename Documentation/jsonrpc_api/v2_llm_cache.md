# v2/llm.cache.*

Unauthenticated internal receivers for the inference cache.  Called
by `cluster::replication::replicate_to_all` (write fan-out from
`vm::api::llm::cache_store`), by anti-entropy (pull missing rows by
id), and reserved for the cluster-wide cache-read fan-out (Phase 3.d,
not yet wired).

Same trust model as the other `v2/*` receivers — bdsnode's RPC port
is the firewall.  No `_hmac` field per call.

## Methods

### `v2/llm.cache.get`

Local cache lookup by content hash.  Used by the future
`dispatch::read` fan-out so a local miss can still hit when only a
peer has the entry.

**Params:** `{ "cache_key": "<sha256 hex>" }`

**Returns:** `{"hit": false}` OR `{"hit": true, …row…}` (see schema
in [`../LLM.md`](../LLM.md) § _Inference cache_).

### `v2/llm.cache.get.by_id`

Local cache lookup by UUID — the path anti-entropy's `pull_one`
calls when copying a row from a peer.

**Params:** `{ "id": "<uuid>" }`

**Returns:** `{"found": false}` OR `{"found": true, …row…}`.

### `v2/llm.cache.put`

Insert a cache row.  Idempotent on both `id` and `cache_key` — replication
or hint replay can re-fire safely.

**Params:**

```json
{
  "id":            "<uuid>",
  "cache_key":    "<sha256 hex>",
  "provider":     "ollama",
  "model":        "llama3.2",
  "kind":         "complete" | "chat" | "analyze:<sub_kind>",
  "request_json":  { … },
  "response_json": { "text": "…", "tokens_in": …, "tokens_out": …, "finish_reason": "stop" },
  "source_meta":   { … } | null,
  "created_at":   <unix-secs>,
  "expires_at":   <unix-secs>           // 0 = never expires
}
```

**Returns:** `{"ok": true, "id": "<uuid>"}`.

### `v2/llm.cache.list_ids`

Enumerate `(id, updated_at)` pairs for anti-entropy.  Same shape
the AE sync_store loop expects from `v2/doc.list_ids` /
`v2/signal.list_ids` / `v2/script.list_ids` / `v2/user.list_ids`.

**Params:** `{}`

**Returns:**

```json
{
  "live":       [{"id": "<uuid>", "updated_at": <unix-secs>}, …],
  "tombstones": []
}
```

Tombstones are not yet wired for this store — the receiver always
returns an empty list.  Cluster-wide purge of cache rows therefore
relies on TTL expiry, not on a coordinator delete path.

### `v2/llm.cache.delete`

Hard-delete a row by id.  Used by AE when applying a remote tombstone
(no tombstones exist yet for `llm_cache`, so this path is reserved
for future use).

**Params:** `{ "id": "<uuid>" }`

**Returns:** `{"ok": true, "id": "<uuid>"}`.

## See also

- [`../LLM.md`](../LLM.md) § _Inference cache_ for the full
  replication + AE story
- [`../CLUSTER.md`](../CLUSTER.md) § _Fully-replicated stores_ for the
  general AE machinery this plugs into
