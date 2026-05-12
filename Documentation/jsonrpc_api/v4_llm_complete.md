# v4/llm.complete

Single-shot text completion.  HMAC-protected (`_hmac` field over canonical
params, key = `cluster.shared_secret`).

Routes through the inference cache (Phase 3) and cluster-wide dedup
layer (Phase 4) — see [`../LLM.md`](../LLM.md) for the full pipeline.

## Parameters

| Field        | Type                | Required | Description |
|--------------|---------------------|----------|-------------|
| `prompt`     | string              | one of   | Shortcut for a single user message.  Mutually exclusive with `messages`. |
| `messages`   | list of {role, content} | one of | Explicit conversation turns.  `role` ∈ `"system"` / `"user"` / `"assistant"` / `"tool"`. |
| `provider`   | string              | no       | Registry name (`ollama` / `anthropic` / `openai` / …).  Default: configured `llm.default`. |
| `model`      | string              | no       | Provider-specific model id.  Default: provider's `default_model`. |
| `options`    | object              | no       | `{temperature, max_tokens, top_p, stop[], seed, num_ctx}` — see [`../LLM.md`](../LLM.md) § _Provider layer_. |
| `cache`      | bool                | no       | `false` to bypass the cache for this call.  Default: cache on unless `options.temperature > 0`. |
| `_hmac`      | string              | yes      | HMAC-SHA256 over canonical params with `_hmac` removed. |

## Response

```json
{
  "response":      "…model output…",
  "provider":      "ollama",
  "model":         "llama3.2",
  "finish_reason": "stop",
  "tokens_in":     312,
  "tokens_out":    58,
  "ms":            842,
  "cache":         "miss",
  "dedup":         "ran"
}
```

`cache` is one of:
`"hit"` · `"miss"` · `"disabled"` · `"disabled:opt-out"` · `"disabled:temperature"`.

`dedup` is one of:
`"ran"` (acquired lease, ran the inference)
· `"waited"` (peer was mid-flight; polled cache then fell through)
· `"skipped:done"` (peer recently finished; cache should have it)
· `"disabled"` (standalone or `llm.dedup.enabled=false`).
Cache hits don't carry `dedup` (decision short-circuited).

## Errors

| Code | Condition |
|---|---|
| `-32001` | `ShardsManager` not initialised |
| `-32097` | Cluster mode disabled on this node |
| `-32098` | Missing or invalid `_hmac` |
| `-32004` | Provider error, validation failure, or no providers registered |
| `-32602` | Invalid params shape — neither `prompt` nor `messages` supplied |

## Example

```bash
curl -s -X POST http://localhost:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"v4/llm.complete","params":{
        "prompt": "summarise the goal of this codebase",
        "provider": "ollama",
        "options": { "temperature": 0, "max_tokens": 200 },
        "_hmac": "<sha256 hex>"
      }}'
```

`bdscmd llm complete -p "summarise the goal of this codebase" --provider ollama --temperature 0`
wraps this with HMAC signing.
