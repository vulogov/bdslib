# v2/scheduler.last_seen

Returns this **target node's** most recent local execution timestamp
(Unix seconds) for a given stored script.  Used by the cluster-aware
Scheduler internally during its dedup check; exposed here so
operators can verify dedup is working without tailing logs.

`bdscmd scheduler-last-seen <SCRIPT_ID>` is the operator-facing
wrapper.  See [`CLUSTER.md` § 12](../CLUSTER.md) and
[`CLUSTER_DETAILS.md` § 5](../CLUSTER_DETAILS.md) for the full
protocol.

## Parameters

| Parameter   | Type   | Required | Description                                       |
|-------------|--------|----------|---------------------------------------------------|
| `script_id` | string | yes      | UUIDv7 of the stored script (see `v2/scripts`).   |

## Response

```json
{
  "last_executed_at": 1778464673
}
```

| Field              | Type             | Description                                                                                                              |
|--------------------|------------------|--------------------------------------------------------------------------------------------------------------------------|
| `last_executed_at` | integer or null  | Unix seconds of the most recent execution this node has recorded.  `null` when this node has never run the script, when cluster mode is disabled (no scheduler log opened), or when the cluster-aware Scheduler hasn't fired the script yet. |

## Examples

```bash
# Standalone or never-fired
curl -s -X POST http://127.0.0.1:9711 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/scheduler.last_seen",
       "params":{"script_id":"019e1559-72f2-7e41-9260-c39b43ea9168"},"id":1}' | jq

# {
#   "jsonrpc": "2.0", "id": 1,
#   "result": { "last_executed_at": null }
# }


# After the Scheduler has fired
curl -s -X POST http://127.0.0.1:9711 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/scheduler.last_seen",
       "params":{"script_id":"019e1559-72f2-7e41-9260-c39b43ea9168"},"id":1}' | jq

# {
#   "jsonrpc": "2.0", "id": 1,
#   "result": { "last_executed_at": 1778464673 }
# }


# Compare across all nodes — healthy dedup means timestamps are close
for port in 9711 9712 9713; do
  printf "node %d: " "$port"
  curl -s -X POST "http://127.0.0.1:$port" \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"v2/scheduler.last_seen",
         "params":{"script_id":"019e1559-…"},"id":1}' \
    | jq -r '.result.last_executed_at // "never"'
done
```

## Error responses

| Code     | Condition                                          |
|----------|----------------------------------------------------|
| `-32001` | `ShardsManager` singleton not initialised          |
| `-32002` | Invalid `params` shape (missing `script_id`)       |
| `-32004` | Scheduler-log query failed                         |
| `-32602` | Invalid `script_id` UUID string                    |

## Notes

- This method is **per-node** — it returns only what *this* node has
  recorded.  The Scheduler's dedup check fans this method out to every
  Alive peer in parallel and takes the **max** to decide whether to
  skip the fire.
- Standalone nodes (cluster.enabled = false) always return `null` —
  the per-node scheduler log only exists on cluster-enabled nodes.
- The method is unauthenticated (no HMAC) — it's part of the data
  plane, not the membership plane.
