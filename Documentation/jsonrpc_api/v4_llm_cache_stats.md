# v4/llm.cache.stats

Aggregate counters for the replicated inference cache.  HMAC-protected.

Per-node read — each node tracks its own counters; anti-entropy keeps
the row contents consistent across the cluster.

## Parameters

| Field   | Type   | Required | Description |
|---------|--------|----------|-------------|
| `_hmac` | string | yes      | |

## Response

```json
{
  "enabled":     true,
  "ttl_secs":    86400,
  "rows":        812,
  "total_hits":  4471,
  "bytes_rough": 9437184
}
```

`bytes_rough` is the sum of `length(response_json)` — a cheap
approximation of cache footprint.  Doesn't count `request_json`,
`source_meta`, or DuckDB metadata.

When the cache manager hasn't been initialised on this node (no
`llm.cache.enabled` / no dbpath), returns:

```json
{ "enabled": false, "rows": 0, "total_hits": 0, "bytes_rough": 0 }
```

## Example

```bash
bdscmd llm cache stats
```
