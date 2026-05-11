# v2/eval

Compile and evaluate a BUND VM script inside a named context (VM instance), then return the result that was on top of the workbench when the script finished.

Each `context` name maps to a lazily-created, persistent BUND VM instance. Re-using the same context name across requests shares the VM's heap and stack state between calls, enabling multi-step interactive evaluation sessions.

When the script invokes any `cls.*` cluster-aware Bund word, the response also carries a `cluster_meta` field describing the most-recent fan-out (peers queried/answered/partial) or replication outcome (peers attempted/succeeded/hinted).  Plain scripts that don't touch `cls.*` get `cluster_meta: null`.

## Parameters

| Parameter | Type | Required | Description |
|---|---|---|---|
| `context` | string | yes | Name of the BUND VM context to use. Created on first use; reused on subsequent calls with the same name. |
| `script` | string | yes | BUND source code to compile and evaluate. |

## Response

```json
{
  "result":       <any JSON value>,
  "cluster_meta": <object or null>
}
```

| Field | Type | Description |
|---|---|---|
| `result` | any JSON | The value that was on top of the VM workbench when the script finished, serialised as JSON.  `null` when the workbench is empty after evaluation. |
| `cluster_meta` | object or null | Cluster fan-out / replication summary populated by the most-recent `cls.*` Bund word the script ran.  `null` when the script ran no `cls.*`, or when the most-recent helper was local-only (e.g. `cls.signal.get`). |

### `cluster_meta` for cluster READS (e.g. `cls.search`, `cls.timeline`)

```json
{
  "enabled":         true,
  "peers_queried":   2,
  "peers_answered":  2,
  "partial":         false,
  "failed":          []
}
```

### `cluster_meta` for cluster WRITES (e.g. `cls.add`, `cls.signal.emit`)

```json
{
  "enabled":     true,
  "replication": {
    "peers_attempted": 2,
    "peers_succeeded": 2,
    "hints_queued":    0
  }
}
```

### `cluster_meta` for standalone bdsnode

`null` is returned regardless of which `cls.*` word ran — the cluster
layer wasn't engaged.

## Example

```bash
# Plain script — cluster_meta is null
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "method":  "v2/eval",
    "params":  { "context": "my-session", "script": "2 2 + ." },
    "id":      1
  }' | jq

# {
#   "jsonrpc": "2.0", "id": 1,
#   "result": { "result": null, "cluster_meta": null }
# }


# Cluster-aware script — cluster_meta carries the fan-out summary
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "method":  "v2/eval",
    "params":  { "context": "demo",
                 "script":  "cls.timeline ?cluster.meta." },
    "id":      1
  }' | jq

# {
#   "jsonrpc": "2.0", "id": 1,
#   "result": {
#     "result": {"enabled": true, "peers_queried": 2, "peers_answered": 2, …},
#     "cluster_meta": {"enabled": true, "peers_queried": 2, "peers_answered": 2, …}
#   }
# }
```

## Error responses

| Code | Condition |
|---|---|
| `-32001` | Named context could not be acquired (context registry not initialised) |
| `-32002` | Script compilation or evaluation failed (syntax error, runtime error, etc.) |

## Notes

- **Stateful contexts.** VM state (heap, stack, defined words) persists for the lifetime of the `bdsnode` process within a given `context`. Use distinct context names to isolate independent sessions.
- **Thread safety.** Each context is protected by a mutex; concurrent requests to the same context name are serialised. Concurrent requests to different context names execute in parallel.
- **Workbench result.** The `result` field is whatever the script left on top of the workbench (`pop_back()`-style).  Use `?cluster.meta.` (workbench-targeted) as the final word in cluster-aware scripts when you want the meta back as the response.
- **Cluster meta thread-safety.** The Bund VM runs each `v2/eval` on a tokio blocking thread; the `cluster_meta` cell is per-thread.  The handler clears it on entry so a stale value from a previous call on the same blocking thread cannot leak through.
