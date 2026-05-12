# v4/llm.analyze_async

Enqueue an analyze job for the background runner.  Same submit/poll
shape as [`v4/llm.complete_async`](v4_llm_complete_async.md).

## Parameters

Same shape as [`v4/llm.analyze`](v4_llm_analyze.md) (including the
required `kind` selector + per-kind extras), plus the optional
`result_id`.

## Response

```json
{
  "job_id":    "01997e92-…",
  "result_id": "01997e92-…",
  "kind":      "analyze:rca",
  "state":     "pending"
}
```

Note `kind` includes the analyze sub-kind so `v4/llm.jobs.list`
can filter by analyze variant.

## Example

```bash
bdscmd llm async -k analyze --analyze-kind rca --duration 1h -q "what broke?"
```

Result payload on the ResultQueue carries the full
`v4/llm.analyze`-shaped response under `result`.
