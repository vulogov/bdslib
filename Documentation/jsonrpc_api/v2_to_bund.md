# v2/to.bund

LLM-based English → Bund translator.  Hands a natural-language
request to the configured LLM provider, expects a fenced
```` ```bund```` block back, validates the result through
`bund_language_parser::bund_parse`, and runs an undefined-word
dry-run against the Adam VM's registered-word set.  Parse failures
and undefined-word references trigger up to `llm.to_bund.max_retries`
follow-up turns where the validation error is fed back to the model
with "fix the issue, keep the intent" instructions.

The endpoint **returns** the script; it does **not** execute it.
Consumers (chat, bdscmd, bdsweb) decide whether to run.

Unauthenticated v2/ surface — the bdsnode RPC port is the trust
boundary, not an HMAC header.  Companion RPC:
[`v2/to.bund.settings`](#v2tobundsettings) echoes the effective
runtime config (including the active sandbox policy spliced into
the system prompt).

Config block: `llm.to_bund.*` in `bds.hjson`
([`../BDSCONFIG.md`](../BDSCONFIG.md) § 7.4).  When
`llm.to_bund.enabled = false` the RPC returns error `-32004` without
contacting the LLM.

## Parameters

| Field         | Type   | Required | Description |
|---------------|--------|----------|-------------|
| `message`     | string | yes      | English request to translate.  Trimmed before dispatch; empty/whitespace-only returns error `-32004`. |
| `provider`    | string | no       | Override the cluster's default provider (`""` / omitted → `llm.default`). |
| `model`       | string | no       | Override the provider's default model (`""` / omitted → that provider's `default_model`). |
| `max_retries` | int    | no       | Override `llm.to_bund.max_retries` for this call.  Server clamps to `[0, 5]`. |
| `options`     | object | no       | Provider sampling block (`temperature`, `max_tokens`, `top_p`, `seed`, `num_ctx`, `stop[]`).  `num_ctx` is auto-bucketed by prompt size when omitted (16k / 32k / 64k). |

## Response

Always an object with these fields:

| Field            | Type    | Notes |
|------------------|---------|-------|
| `script`         | string  | Generated Bund body extracted from the model's fenced output.  When `valid=false` carries the last attempt for debugging. |
| `valid`          | bool    | `true` iff the parse + undefined-word dry-run both succeeded. |
| `parse_attempts` | int     | `1` on first-try success; `2..=max_retries+1` if retries were needed. |
| `provider`       | string  | The provider id that actually answered (`"ollama"` / `"anthropic"` / …). |
| `model`          | string  | The model id reported by the provider. |
| `ms`             | int     | Wall-clock cost in milliseconds. |
| `tokens_in`      | int     | Sum across attempts when reported by the provider; omitted otherwise. |
| `tokens_out`     | int     | Same convention as `tokens_in`. |
| `parse_error`    | string  | **Only present when `valid=false`.**  Last validation message — syntax error from `bund_parse`, or `"references N word(s) not registered: …"` from the dry-run. |

## Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc":"2.0","id":1,
    "method":"v2/to.bund",
    "params":{
      "message":"find all records with severity ERROR in the last hour",
      "options":{"temperature":0.1}
    }
  }' | jq
```

Success:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "script": "[ \"severity\" \"ERROR\" ] $tomap\n\"1h\" $duration\ncls.search ! .\n",
    "valid": true,
    "parse_attempts": 1,
    "provider": "ollama",
    "model": "llama3.2",
    "ms": 1842,
    "tokens_in": 4120,
    "tokens_out": 78
  }
}
```

Hard failure after retries exhausted:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "script": "make.unicorn ride.dragon\n",
    "valid": false,
    "parse_attempts": 3,
    "provider": "ollama",
    "model": "llama3.2",
    "ms": 6431,
    "parse_error": "The script references 2 word(s) that are not registered in this Bund VM: make.unicorn, ride.dragon. Use only words listed in the stdlib catalogue from the system prompt, or define new words inline via `:name { body } register`."
  }
}
```

## Validation layers

Each retry-loop iteration runs in order:

1. **Block extraction** — pull a ```` ```bund ```` (or untagged) fence
   from the model's output; fall back to the trimmed raw text when
   no fence is present.
2. **Syntax** — `bund_language_parser::bund_parse(script)` rejects
   malformed tokens.
3. **Undefined-word dry-run** — every `CALL` value in the parsed AST
   (including those nested in lambdas, lists, contexts, maps,
   matrices, and queues) is checked against the registered-word set
   snapshotted from the Adam VM (`bdslib::vm::registered_word_names`).
   Unknown words fail the dry-run with a "not registered" message
   naming the offending word(s).

On failure, the previous assistant reply + a user-turn quoting the
error are appended to the conversation, and the loop runs again.
After `max_retries` exhausts, the endpoint returns `valid=false`
with `parse_error` populated.

## Policy-aware system prompt

When `bund.disabled_categories` / `bund.disabled_words` is set, the
active policy is spliced into the system prompt under a **Disabled
words (sandbox policy)** section so the model can avoid words that
would be denied at runtime.  See
[`../BDSCONFIG.md`](../BDSCONFIG.md) § 4.1 for the sandbox and
§ 7.4 for prompt assembly.  The full effective blocklist is
returned by [`v2/to.bund.settings`](#v2tobundsettings) under
`disabled_groups`.

## Error codes

| Code     | Trigger                                                                     |
|----------|-----------------------------------------------------------------------------|
| `-32000` | task panic on the bdsnode tokio pool                                        |
| `-32004` | `enabled = false` · empty `message` · provider call failed · manager not initialised |

The endpoint returns a normal `result` with `valid: false` for
**validation** failures (syntax / undefined words after retries
exhausted) — those are not RPC errors.

## v2/to.bund.settings

Companion read-only RPC.  Echoes the active runtime config so
operators can confirm the right provider/model defaults and the
sandbox policy spliced into the system prompt.

### Parameters

None.

### Response

```json
{
  "enabled":                 true,
  "timeout_secs":            120,
  "max_retries":             2,
  "provider":                "",
  "model":                   "",
  "extra_system_prompt_len": 0,
  "disabled_groups": [
    {"category": "os_shell",        "words": ["system.shell", "system.shell."]},
    {"category": "process_control", "words": ["bund.exit", "sleep.seconds"]}
  ]
}
```

`disabled_groups` is empty when no sandbox policy is active.
`extra_system_prompt_len` is the byte length of
`llm.to_bund.extra_system_prompt`; the prompt text itself is not
echoed (it can contain operator secrets).

## See also

- [`../LLM.md`](../LLM.md) § _English → Bund translator_ — design,
  prompt assembly, retry loop
- [`../BDSCONFIG.md`](../BDSCONFIG.md) § 7.4 — the `llm.to_bund.*`
  config block
- [`../BDSCMD.md`](../BDSCMD.md) § `to-bund` — CLI consumer
- [`../BDSWEB.md`](../BDSWEB.md) § _Bund Workbench_ — `/bund`
  Translate-from-English panel
