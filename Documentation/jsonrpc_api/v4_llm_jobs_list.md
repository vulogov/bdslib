# v4/llm.jobs.list

List queued / in-flight / terminal async jobs.  HMAC-protected.

Reads the local `llm_jobs` table — per-node, not replicated.  Each
node sees only the jobs it claimed or that were submitted to it.

## Parameters

| Field   | Type    | Required | Description |
|---------|---------|----------|-------------|
| `state` | string  | no       | `"pending"` / `"running"` / `"done"` / `"failed"` / `"cancelled"` |
| `limit` | integer | no       | Default 100; max-rows cap |
| `_hmac` | string  | yes      | |

## Response

```json
{
  "count": 3,
  "jobs": [
    {
      "job_id":       "01997e92-…",
      "result_id":    "01997e93-…",
      "kind":         "analyze:rca",
      "state":        "done",
      "owner_node":   "019e0a-…",
      "submitted_at": 1715456112,
      "started_at":   1715456113,
      "finished_at":  1715456120,
      "error":        null
    },
    …
  ]
}
```

Rows are returned newest-first by `submitted_at`.  `owner_node` is
null for `pending` rows.

## Example

```bash
bdscmd llm jobs --state pending --limit 50
bdscmd llm jobs   # all states, default limit 100
```
