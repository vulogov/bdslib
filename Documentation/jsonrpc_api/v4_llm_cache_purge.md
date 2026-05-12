# v4/llm.cache.purge

Drop matching cache rows.  Empty filter set drops everything.
HMAC-protected.

**Local only** in the current implementation — purge runs on the node
that receives the call.  Cluster-wide convergence happens via
anti-entropy (peers compare `list_ids` and pull rows they're
missing — so purged rows on one node will be replenished from a peer
that still has them).  For genuine cluster-wide purge, run the
command against every node, or rely on TTL expiry + the background
sweeper.

## Parameters

| Field                | Type    | Required | Description |
|----------------------|---------|----------|-------------|
| `provider`           | string  | no       | Drop only rows with this provider |
| `kind`               | string  | no       | Drop only rows with this kind (e.g. `"complete"`, `"analyze:rca"`) |
| `older_than_created` | integer | no       | Drop rows with `created_at < older_than_created` (unix seconds) |
| `_hmac`              | string  | yes      | |

All filters are ANDed.

## Response

```json
{
  "purged": 23
}
```

## Errors

| Code | Condition |
|---|---|
| `-32004` | Cache manager not initialised |
| `-32098` | HMAC failure |

## Example

```bash
# Purge entries older than a day from the ollama provider
bdscmd llm cache purge --provider ollama --older-than-secs 86400

# Purge everything
bdscmd llm cache purge
```

bdsweb's `/admin/llm` page exposes a form-based purge with a JS
`confirm()` guard.
