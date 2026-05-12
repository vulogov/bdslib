# v4/llm.jobs.cancel

Cancel a pending or running async job.  HMAC-protected.

Idempotent on terminal states — re-cancelling a `done` / `failed` /
already-`cancelled` job returns `{ok: false}` with no error.

## Cancellation semantics

- **Pending** → flipped to `cancelled` immediately.  The runner's
  `claim_one` returns the next pending row, skipping this one
  (claim filters by `state = 'pending'`).
- **Running** → flipped to `cancelled` in the queue.  The runner
  checks state TWICE — once before the provider call (no-op if
  pending → cancelled in the gap), once after (skips `results.push`,
  pushes a `cancelled` sentinel instead, writes `cancelled` as the
  terminal row state).  The provider HTTP request is NOT aborted —
  the upstream has already incurred cost.

## Parameters

| Field    | Type   | Required | Description |
|----------|--------|----------|-------------|
| `job_id` | string | yes      | |
| `_hmac`  | string | yes      | |

## Response

```json
{ "ok": true,  "job_id": "01997e92-…" }   // cancellation took effect
{ "ok": false, "job_id": "01997e92-…" }   // already terminal, or unknown id
```

## Example

```bash
bdscmd llm cancel -i 01997e92-...
```
