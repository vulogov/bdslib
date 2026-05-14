# v2/health

Dedicated **readiness / liveness probe** for the node.  Returns the
aggregate self-healing verdict plus a per-source breakdown, computed
entirely from the in-process health registry — no database access,
cheap enough to call at high frequency from a load balancer or
orchestrator.

Unlike [`v2/status`](v2_status.md) (a broad operational snapshot),
`v2/health` answers exactly one question — *should traffic come
here?* — and answers it from the health registry alone.

## Parameters

This method accepts no parameters.  The `params` field may be omitted
or set to an empty object.

## Response

```json
{
  "status":  "healthy",
  "reason":  "",
  "ts":      1778900000,
  "sources": [
    {
      "name":           "cluster.gossip",
      "status":         "healthy",
      "reason":         "",
      "last_heartbeat": 1778899998,
      "stale":          false
    },
    {
      "name":           "ingest.flushers",
      "status":         "healthy",
      "reason":         "",
      "last_heartbeat": 1778899999,
      "stale":          false
    },
    {
      "name":           "shard.1778738400_1778742000",
      "status":         "failed",
      "reason":         "quarantined: DuckDB at .../obs.db will not open: ...",
      "last_heartbeat": 1778899900,
      "stale":          false
    }
  ]
}
```

| Field | Type | Description |
|---|---|---|
| `status` | string | Aggregate verdict — `"healthy"`, `"degraded"`, or `"failed"`. The **worst** of every source (`failed` beats `degraded` beats `healthy`). An empty registry is `"healthy"`. |
| `reason` | string | Operator-facing reason for a non-healthy verdict; empty when `"healthy"`. Names the source that drove the verdict. |
| `ts` | integer | Wall-clock Unix-second timestamp the probe was computed. |
| `sources` | array | One entry per registered health source — see below. |

### Source entries

| Field | Type | Description |
|---|---|---|
| `name` | string | Source name — see the registered-sources table below. |
| `status` | string | The source's **effective** status: its self-reported status, OR `"failed"` when its heartbeat is stale (a stale heartbeat overrides a healthy status — a hung loop can't update its own status). |
| `reason` | string | Operator-facing detail for a non-healthy source; empty when healthy. |
| `last_heartbeat` | integer | Unix-second of the source's last heartbeat (or status update). |
| `stale` | bool | `true` when `last_heartbeat` is older than the source's staleness window — i.e. the loop is hung. |

## Registered sources

Every long-lived subsystem registers a source and heartbeats each
tick:

| Source | What it tracks | Staleness window |
|---|---|---|
| `cluster.gossip` | the gossip loop | 6× gossip interval, ≥30 s |
| `rebalancer` | the data-rebalancer sweep | 3× sweep interval, ≥60 s |
| `sync` | the periodic global-sync tick | 3× interval, ≥60 s |
| `retention` | the retention sweep | 3× interval, ≥120 s |
| `scheduler` | the cron scheduler tick | 3× interval, ≥60 s |
| `llm_jobs` | the async LLM job-drain poll | 10× poll, ≥30 s |
| `ingest.flushers` | the ingest flusher supervisor | 30 s |
| `shard_healer` | the shard rebuild healer | 4× interval, ≥120 s |
| `shard.<start>_<end>` | one per quarantined shard | (no staleness check — status-reported) |

A disabled subsystem (e.g. cluster mode off, rebalancer off) simply
never registers — its absence from `sources` is correct, not an
error.

## Status semantics

- **`healthy`** — the subsystem is operating normally.
- **`degraded`** — working but at-risk or reduced (e.g.
  `ingest.flushers` reports `Degraded` when `alive < configured`
  mid-respawn).
- **`failed`** — not functioning: a hung loop (stale heartbeat), an
  unhealable quarantined shard, or zero ingest flushers alive.

## Use as a health check

`bdscmd health --quiet` prints just the verdict word and exits
**non-zero** when it is not `healthy` — usable directly as a shell
check or a Kubernetes exec probe:

```yaml
livenessProbe:
  exec:
    command: ["bdscmd", "-a", "http://127.0.0.1:9000", "health", "--quiet"]
```

## Errors

`v2/health` cannot fail under normal operation — it reads only
process-local in-memory state.

## See also

- [`v2/status`](v2_status.md) — the broad operational snapshot, which
  also carries a compact `health` block.
- [`v2/perf`](v2_perf.md) — the latency registry many health sources
  draw their staleness windows from.
- [`NODE_SELF_HEALING.md`](../NODE_SELF_HEALING.md) — what each source
  means and how the self-healing layer acts on the verdict.
