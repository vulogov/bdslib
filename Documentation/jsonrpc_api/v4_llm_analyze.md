# v4/llm.analyze

Build a RAG context from bdslib data and run one completion over it.
The `kind` selector picks a `ContextSource` variant; each variant
routes through the matching cluster-aware `vm::api::*` helper so
standalone vs cluster mode is transparent.  HMAC-protected.

See [`../LLM.md`](../LLM.md) § _Inference cache_ (analyze responses
ARE cacheable, keyed by sorted fingerprints + kind + query + template
+ options).

## Parameters

| Field             | Type   | Required | Description |
|-------------------|--------|----------|-------------|
| `kind`            | string | yes      | One of `aggregation` / `knn` / `rca` / `anomaly` / `templates` / `telemetry` / `documents` / `supplied` |
| `query`           | string | no       | Operator's question.  Appended after the assembled context. |
| `prompt_template` | string | no       | Override the per-kind default preamble. |
| `system_prompt`   | string | no       | Override the default SRE-style system prompt. |
| `provider`        | string | no       | Provider override |
| `model`           | string | no       | Model override |
| `options`         | object | no       | Same shape as `v4/llm.complete` |
| `cache`           | bool   | no       | `false` to bypass cache for this call |
| `_hmac`           | string | yes      | HMAC-SHA256 over canonical params |

### Per-kind extras

| Kind         | Extras                                                                |
|--------------|-----------------------------------------------------------------------|
| `aggregation`| `duration` (req), `limit`                                             |
| `knn`        | `duration` (req), `k`                                                 |
| `rca`        | `duration` (req), `failure_key`, `bucket_secs`, `min_support`, `jaccard_threshold`, `max_keys` |
| `anomaly`    | `duration` (req), `limit`                                             |
| `templates`  | `duration` (req), `top_n`                                             |
| `telemetry`  | `duration` (req), `limit`                                             |
| `documents`  | `ids` (list of UUIDv7 strings, req)                                   |
| `supplied`   | `rows` (list of arbitrary JSON values, req) — direction 5 in the proposal |

## Response

```json
{
  "response":      "…model output…",
  "kind":          "rca",
  "source": {
    "kind":      "rca",
    "duration":  "1h",
    "n_events":  482,
    "row_count": 12
  },
  "n_rows":        12,
  "provider":      "ollama",
  "model":         "llama3.2",
  "finish_reason": "stop",
  "tokens_in":     1180,
  "tokens_out":    312,
  "ms":            1842,
  "cache":         "miss",
  "dedup":         "ran"
}
```

`source` echoes back the `RagContext.source_meta` so callers can see
exactly which retrieval ran — useful for bdsweb display and cache-key
explanation.

## Errors

| Code | Condition |
|---|---|
| `-32004` | Provider error / unknown kind / required field missing / context build failed |
| `-32602` | Invalid params shape |
| `-32097` / `-32098` | Cluster / HMAC issues |

## Example

```bash
curl -s -X POST http://localhost:9000 -H 'Content-Type: application/json' -d '{
  "jsonrpc":"2.0","id":1,"method":"v4/llm.analyze","params":{
    "kind":     "rca",
    "duration": "1h",
    "query":    "what broke?",
    "_hmac":    "<sha256 hex>"
  }
}'
```

`bdscmd llm analyze -k rca --duration 1h -q "what broke?"` wraps this
with HMAC signing.

Supplied-rows variant (no DB hit — operator pipes their own data):

```bash
bdscmd llm analyze -k supplied --rows-file rows.json -q "summarise"
```
