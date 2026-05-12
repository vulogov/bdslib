# v4/llm.complete_async

Enqueue a completion job for the background runner.  Returns
immediately with `{job_id, result_id}` — poll `v2/results.pull`
against `result_id` to retrieve the response when the runner
finishes.  HMAC-protected.

## Parameters

Same shape as [`v4/llm.complete`](v4_llm_complete.md), plus:

| Field       | Type   | Required | Description |
|-------------|--------|----------|-------------|
| `result_id` | string | no       | UUIDv7 of an existing ResultQueue to reuse (useful when fanning out several jobs to a single waiter).  When omitted, a fresh id is minted. |

Sync validation (parse_messages, etc.) still runs synchronously so
malformed requests fail at submit time rather than later in the
runner.

## Response

```json
{
  "job_id":    "01997e92-…",
  "result_id": "01997e92-…",
  "kind":      "complete",
  "state":     "pending"
}
```

## When the runner finishes

The runner pushes a payload onto `v2/results.pull` under `result_id`:

```json
{
  "job_id":    "01997e92-…",
  "result_id": "01997e92-…",
  "kind":      "complete",
  "state":     "done" | "failed" | "cancelled",
  "result":    { /* same shape as v4/llm.complete */ },     // when done
  "error":     "…"                                          // when failed
}
```

The job's row in `llm_jobs` is marked terminal in parallel — query
via [`v4/llm.jobs.status`](v4_llm_jobs_status.md) for the audit trail.

## Example

```bash
bdscmd llm async -k complete -p "long-running analysis"
# {"job_id":"…","result_id":"…","kind":"complete","state":"pending"}

# Poll:
bdscmd results-pull --id <result_id>
```
