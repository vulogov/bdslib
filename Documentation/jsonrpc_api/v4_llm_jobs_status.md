# v4/llm.jobs.status

Inspect a single job by id.  HMAC-protected.

## Parameters

| Field    | Type   | Required | Description |
|----------|--------|----------|-------------|
| `job_id` | string | yes      | UUIDv7 returned by `v4/llm.{complete,analyze}_async` |
| `_hmac`  | string | yes      | |

## Response

Same row shape as one entry in
[`v4/llm.jobs.list`](v4_llm_jobs_list.md):

```json
{
  "job_id":       "01997e92-…",
  "result_id":    "01997e93-…",
  "kind":         "analyze:rca",
  "state":        "running",
  "owner_node":   "019e0a-…",
  "submitted_at": 1715456112,
  "started_at":   1715456113,
  "finished_at":  null,
  "error":        null
}
```

## Errors

| Code | Condition |
|---|---|
| `-32004` | Job queue not initialised / job not found |

## Example

```bash
bdscmd llm status -i 01997e92-...
```
