# v2/llm.last_executed

Most-recent `inference_log` row for a `cache_key` as seen by **this**
node.  Peer of [`v2/scheduler.last_seen`](v2_scheduler_last_seen.md);
fan-out target for the LLM dedup layer (Phase 4 of the LLM surface).

Unauthenticated receiver.  Reads `Cluster.inference_log` (per-node
DuckDB at `<dbpath>/network/inference_log.duckdb`).  Standalone nodes
have no `Cluster` and return `{found: false}`.

## Parameters

| Field         | Type    | Required | Description |
|---------------|---------|----------|-------------|
| `cache_key`   | string  | yes      | The same key used by the inference cache (sha256 hex of canonical request) |
| `window_secs` | integer | no       | Restrict to rows whose `started_at >= now - window_secs`.  Lets callers and peers agree on the dedup window even if config drifts. |

## Response

When no row exists for `cache_key` (or the row is outside the window):

```json
{ "found": false }
```

When a row exists:

```json
{
  "found":       true,
  "cache_key":   "<sha256 hex>",
  "started_at":  1715456112,
  "finished_at": 1715456120,
  "node_id":     "019e0a-…",
  "state":       "running" | "done" | "failed"
}
```

## How callers use the response

The coordinator path in `vm::api::llm::{complete,analyze}`:

1. Local `inference_log.recent_within(cache_key, window_secs)` check first.
2. If nothing locally, fan `v2/llm.last_executed` to every Alive peer.
3. For each `found: true` response within the window:
   - `state: "running"` → `SkipRunning` — wait for the cache entry
   - `state: "done"`    → `SkipDone` — cache should already have it
   - `state: "failed"`  → ignore (retry locally)
4. If no peer is mid-flight or recently done, mint a local `running`
   row and proceed with the inference.

See [`../LLM.md`](../LLM.md) § _Cluster-wide dedup_ for the full
lease flow and accepted race-window tradeoff.
