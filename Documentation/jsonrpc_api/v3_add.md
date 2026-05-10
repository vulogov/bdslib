# v3/add

Replicated single-document write. Same shape as
[`v2/add`](v2_add.md) but the coordinator also fans the write out to
`replication_factor - 1` random Alive peers.

**Fire-and-forget semantics.** The call returns once the local write has
succeeded; replica writes are dispatched on a detached tokio task. Failures
on individual replicas are not propagated to the client — they are enqueued
as **hints** and replayed automatically when the failed peer next becomes
Alive. The only thing that fails the call is a local-write failure.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `doc` | object | yes | — | Telemetry document. Must contain `timestamp`, `key`, and `data`. May contain `id` (caller-supplied UUIDv7) — when present it is preserved as the replication identity, making retries idempotent. |
| `replication_factor` | integer | no | `cluster.replication_factor` | Override the cluster's configured replication factor. Clamped at runtime to `min(rf, alive_peers + 1)`. |

## Response

```json
{
  "id":                  "019e103e-4d9e-7aa2-a83f-faf98655b6e1",
  "replication_factor":  3,
  "replicas_dispatched": 2,
  "alive_peers":         2,
  "under_replicated":    false,
  "mode":                "full",
  "cluster_meta":        { "enabled": true }
}
```

| Field | Description |
|---|---|
| `id` | UUIDv7 of the stored record. Either the caller's id (preserved on retry) or freshly generated. **Same id is used on every replica** — this is the dedup key for v3 reads. |
| `replication_factor` | Effective rf for this call (after clamping to `alive_peers + 1`). |
| `replicas_dispatched` | Number of peer fan-outs queued. Always `replication_factor - 1` when there are enough Alive peers. |
| `alive_peers` | Number of Alive peers at dispatch time. |
| `under_replicated` | `true` when fewer peers were available than the requested rf - 1. The hint replay task does **not** retry under-replication on its own; future writes will hit fresh peers, but the under-replicated record stays at its lower replica count until you explicitly re-write it. |
| `mode` | Cluster mode at dispatch time (`standalone`, `partial`, or `full`). |

## Example

```bash
bdscmd cluster add -D '{"timestamp":1778000000,"key":"app.error","data":{"msg":"boom"}}'
```

Equivalent raw curl:

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc":"2.0", "method":"v3/add", "id":1,
    "params": {
      "doc": {"timestamp":1778000000,"key":"app.error","data":{"msg":"boom"}}
    }
  }' | jq
```

## Idempotency

Re-sending `v3/add` with the same `doc.id` is **safe** — every replica's
local v2/add path dedups by `(key, data_text)`, so the second arrival is
absorbed without creating a duplicate row. Retries by client code (or by the
hint replay task) therefore converge to the right state regardless of how
many times they fire.

The dedup is by `(key, data_text)` rather than by id, however, so a write
whose `(key, data_text)` was previously stored under a different id will
return that **other** id rather than the one supplied — this matters only
in degenerate cases where multiple records carry the same payload.

## Hinted handoff lifecycle

When a peer fails to ack a replicated write within
`cluster.peer_rpc_timeout` (default 2s), the write is enqueued in the
`<dbpath>/network/hints.duckdb` hinted-handoff store with the target peer's
node_id. The cluster background task (`bdsnode/server/cluster.rs`) then:

1. Every `cluster.hint_replay_interval` (default 10s), prunes hints older
   than `cluster.hint_max_age` (default 24h).
2. For each peer that's currently Alive and has at least one hint, drains
   up to 100 hints and replays each via `v2/add` against the peer's
   `bind_url`.
3. Hints that succeed are deleted; the first failure aborts that peer's
   batch (it's likely still down or overloaded — try again next tick).

`hint_backlog` is exposed in `v2/status.cluster` and `v2/cluster.peers`
so operators can monitor recovery progress.

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32004` | Local write failed |
| `-32602` | Invalid params (missing required field, malformed `id`) |

## Notes

- **Standalone mode is supported.** When `cluster.enabled = false` (or no
  Alive peers), v3/add behaves like v2/add with `sync: true` — local
  write only, `replicas_dispatched: 0`, `mode: "standalone"`.
- **No quorum.** This implementation deliberately does not block on
  replica acks. The trade-off is documented in
  [CLUSTER.md § 9](../CLUSTER.md#9-failure-modes--what-we-get-right-and-what-we-punt-on):
  there is a brief window after the call returns where the data exists
  only on the coordinator. Client-side retries on `v3/add` are safe
  because `_uuid` makes replication idempotent.
- **Replication factor at runtime.** The effective rf is
  `min(replication_factor, alive_peers + 1)`. In Partial mode (some
  peers Suspect/Dead) you'll see `under_replicated: true`; the data is
  durable on the coordinator + whoever responded, and the missing
  replicas will be re-attempted only when *new* writes happen — Phase 3
  does not have a background "anti-entropy" sweep for under-replicated
  records (that's Phase 4 territory for full-replicated stores).
