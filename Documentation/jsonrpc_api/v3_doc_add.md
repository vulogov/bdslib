# v3/doc.add

Fully-replicated document add.  Same shape as [`v2/doc.add`](v2_doc_add.md)
but the coordinator also fans the write out to **every** Alive peer via
`v2/doc.add` (with the assigned UUID injected so each replica writes
under the same identity).

This is the canonical Phase 4 replicated-write pattern — the same recipe
applies to [`v3/signal.emit`](v3_signal_emit.md), [`v3/script.add`](v3_script_add.md),
and the parallel `update`/`delete` methods, which cross-reference this
doc rather than repeating the lifecycle.

Architectural overview: [`Documentation/CLUSTER.md`](../CLUSTER.md).

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `metadata` | object | yes | Document metadata. The coordinator stamps `updated_at` (Unix seconds) into this object before writing — anti-entropy uses it for last-write-wins comparison. |
| `content` | string | yes | UTF-8 document body. |
| `id` | string | no | Caller-supplied UUIDv7. Preserved on retries; replication is idempotent (the `(metadata, content)` already exists at the receiver if the same `id` arrives twice — receiver returns `"existing": true` without re-writing). |
| `session` | string | no | UUIDv7 transaction id (echoed only). |

## Response

```json
{
  "id": "019e1056-d0c9-75c3-927b-dbed1b62bdb2",
  "outcome": {
    "peers_attempted": 2,
    "peers_succeeded": 2,
    "hints_queued":    0
  },
  "cluster_meta": { "enabled": true }
}
```

| Field | Description |
|---|---|
| `id` | UUIDv7 of the stored document — identical on every replica. |
| `outcome.peers_attempted` | Number of Alive peers we fanned out to (always `alive_peers`, since this is full replication). |
| `outcome.peers_succeeded` | Number of peers that returned a successful `v2/doc.add` response. |
| `outcome.hints_queued` | Number of peers that failed and got their write enqueued in the hinted-handoff store for replay. |
| `cluster_meta.enabled` | `true` when cluster mode is on; `false` for stand-alone. |

## Lifecycle

```text
client → coordinator: v3/doc.add { metadata, content }
coordinator: stamp metadata.updated_at = now()
coordinator: write locally with shared UUIDv7 (sync)
coordinator → client: { id, outcome: {peers_attempted, peers_succeeded, hints_queued}, … }   (returns immediately)
coordinator: spawn detached task — for each Alive peer:
    try v2/doc.add (with `id` injected) with timeout cluster.peer_rpc_timeout
    on success → done
    on failure → enqueue hint in <dbpath>/network/hints.duckdb

every cluster.hint_replay_interval (default 10s):
    for each Alive peer with hints: replay them, delete on success

every cluster.antientropy_interval (default 5min):
    for each store in cluster.full_replication_stores:
        pick a random Alive peer, list_ids it,
        diff against local, pull missing entries (skip locally-tombstoned)
```

## Example

```bash
bdscmd cluster doc-add \
  -m '{"title":"runbook v3","tags":["ops"]}' \
  -c "Step 1: …\nStep 2: …"
```

Equivalent raw curl:

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc":"2.0","method":"v3/doc.add","id":1,
    "params":{
      "metadata": {"title":"runbook v3","tags":["ops"]},
      "content":  "Step 1: …\nStep 2: …"
    }
  }' | jq
```

## Idempotency

Replication retries (and the hint replay path) re-arrive at the same UUID.
The receiver-side [`v2/doc.add`](v2_doc_add.md) checks whether the id is
already present and returns `{"id":"…","existing":true}` without writing.
This makes client-side retries on `v3/doc.add` safe.

## Error responses

| Code | Condition |
|---|---|
| `-32000` | Internal task panic |
| `-32001` | Database unavailable |
| `-32011` | Local docstore add failed |
| `-32602` | Invalid params (missing required field, malformed `id`) |

## Notes

- **Standalone mode.** With `cluster.enabled = false` (or no Alive peers),
  v3/doc.add behaves like v2/doc.add — local write only,
  `outcome.peers_attempted: 0`.
- **Anti-entropy is the safety net.** A peer that's down at write time
  doesn't get the hint replayed if it stays down longer than
  `cluster.hint_max_age` (default 24h).  Anti-entropy then recovers the
  missed write the next time both peers are Alive simultaneously by
  diffing `v2/doc.list_ids` and pulling any entry that's on the remote
  but not local (and not locally tombstoned).
