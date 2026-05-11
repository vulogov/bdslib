# Cluster Details — Protocol-Level Reference

This document complements [`CLUSTER.md`](CLUSTER.md) (configuration +
operations) with a protocol-level walk-through of every cluster
mechanism: peer discovery, eviction and re-acceptance, schedule
control, data distribution, replication, and read fan-out.  Each
section includes the actual JSON-RPC payloads exchanged between
peers and what they cause to happen on disk and in memory.

For prerequisites and config knobs, read [`CLUSTER.md`](CLUSTER.md)
first.

---

## Table of Contents

1. [Authentication primitives](#1-authentication-primitives)
2. [Peer discovery — gossip protocol](#2-peer-discovery--gossip-protocol)
3. [Eviction — Suspect → Dead](#3-eviction--suspect--dead)
4. [Re-acceptance — recovery probe + Hello](#4-re-acceptance--recovery-probe--hello)
5. [Schedule control — cluster-aware Scheduler](#5-schedule-control--cluster-aware-scheduler)
6. [Data distribution](#6-data-distribution)
7. [Write replication — sharded + fully-replicated](#7-write-replication--sharded--fully-replicated)
8. [Hinted handoff + anti-entropy](#8-hinted-handoff--anti-entropy)
9. [Read fan-out — v3/* surface](#9-read-fan-out--v3-surface)
10. [Bdsweb mode-aware routing](#10-bdsweb-mode-aware-routing)

---

## 1. Authentication primitives

Every `v3/cluster.*` call carries an HMAC-SHA256 signature computed
over the canonical-JSON form of the params object.  The shared key is
`cluster.shared_secret` from `bds.hjson`.  The receiver
([`cluster::hmac_auth`](../src/cluster/hmac_auth.rs)) re-canonicalises
the inbound params, recomputes the HMAC, and refuses any call where
the signature does not match.

The fields the caller adds:

| Field | Type | Purpose |
|---|---|---|
| `_hmac` | hex string | HMAC-SHA256 over the canonical params (excluding `_hmac` itself) |
| `_ts`   | int (unix s) | Timestamp; receiver may reject calls outside ±5 min skew |
| `_nonce`| string | Random per-call nonce (replay protection within the skew window) |

A typical body:

```json
{
  "jsonrpc": "2.0", "id": 1, "method": "v3/cluster.ping",
  "params": {
    "_ts":    1778464000,
    "_nonce": "kY7q5mZ0",
    "_hmac":  "8b1e…f2"
  }
}
```

`v2/*` and `v3/*` data-plane methods (search, add, scheduler, …) are
NOT HMAC-protected.  They are issued by trusted in-process peers
during fan-out and rely on the same trust boundary as `v2/*` clients.
For perimeter control, run mTLS or a reverse proxy in front of
bdsnode.

---

## 2. Peer discovery — gossip protocol

A node's view of the cluster is held in [`SharedPeerTable`](../src/cluster/peer_table.rs):
a per-node `BTreeMap<Uuid, Peer>` keyed by node UUIDv7.  Each `Peer`
carries `{node_id, url, state, last_seen, version, embedding_model,
started_at}`.  The state is one of:

- `Alive` — last contact was within `suspect_timeout` (default 30 s).
- `Suspect` — `[suspect_timeout, dead_timeout)` since last contact.
- `Dead` — `≥ dead_timeout` since last contact (default 120 s).

The gossip task in
[`bdsnode/server/cluster.rs`](../src/bin/bdsnode/server/cluster.rs)
fires every `gossip_interval` (default 5 s).  Each tick does three
things in order: liveness sweep, random ping, and (if the cluster has
gone dark) a recovery probe.

### 2.1 Bootstrap — the first contact

At startup the node calls
[`gossip::bootstrap_first_pass`](../src/cluster/gossip.rs):

1. Build the candidate set — `cluster.bootstrap` URL plus every URL
   in persisted `peers.json` (when `floating_bootstrap = true`).
2. For each candidate, send `v3/cluster.hello` in parallel:

```json
POST http://10.0.0.5:9000
{
  "jsonrpc": "2.0", "id": 1, "method": "v3/cluster.hello",
  "params": {
    "_ts": 1778464000, "_nonce": "…", "_hmac": "…",
    "node_id":         "019e155a-0000-7100-9000-000000000007",
    "bind_url":        "http://10.0.0.7:9000",
    "version":         "0.12.2",
    "embedding_model": "AllMiniLML6V2",
    "started_at":      1778463990
  }
}
```

3. The receiver:
   - Verifies HMAC.
   - Verifies the `embedding_model` matches its own (mismatched
     dimensions would silently corrupt federated vector queries —
     refused with `-32096`).
   - Calls `peers.write().upsert(Peer{state: Alive, last_seen: now()})`
     for the caller.
   - Persists `peers.json` (atomic tmp+rename).
   - Echoes back its identity + its full peer view:

```json
{
  "jsonrpc": "2.0", "id": 1,
  "result": {
    "node_id":         "019e155a-0000-7100-9000-000000000005",
    "bind_url":        "http://10.0.0.5:9000",
    "version":         "0.12.2",
    "embedding_model": "AllMiniLML6V2",
    "started_at":      1778460000,
    "peers": [
      { "node_id": "019e155a-0000-7100-9000-000000000005", "url": "http://10.0.0.5:9000",
        "state": "alive", "last_seen": 1778464001, "version": "0.12.2",
        "embedding_model": "AllMiniLML6V2", "started_at": 1778460000 },
      { "node_id": "019e155a-0000-7100-9000-000000000006", "url": "http://10.0.0.6:9000",
        "state": "alive", "last_seen": 1778463999, "version": "0.12.2",
        "embedding_model": "AllMiniLML6V2", "started_at": 1778461000 }
    ]
  }
}
```

4. The caller folds the returned `peers[]` into its own table — the
   cluster converges in **one round-trip**.

### 2.2 Steady-state ping

Every gossip tick picks a random Alive peer (or a random Suspect to
probe first) and calls `v3/cluster.ping`:

```json
{
  "jsonrpc": "2.0", "id": 1, "method": "v3/cluster.ping",
  "params": { "_ts": …, "_nonce": …, "_hmac": … }
}
```

Successful response:

```json
{ "jsonrpc": "2.0", "id": 1,
  "result": { "node_id": "019e155a-…", "ts": 1778464006 } }
```

The caller updates `peer.last_seen = now()` and `peer.state = Alive`.

### 2.3 Inspecting state from outside

`bdscmd cluster peers` issues an authenticated `v3/cluster.peers`:

```bash
$ bdscmd cluster peers --secret "$BDS_CLUSTER_SECRET"
{
  "node_id": "019e155a-…05",
  "peers": [ { "node_id": "…05", "state": "alive", "last_seen": 1778464006, … }, … ]
}
```

The unauthenticated `v2/cluster.peers` exposes the same view (used by
the bdsweb dashboard and `cls.timeline` etc.).  See
`Documentation/jsonrpc_api/v3_cluster_peers.md`.

---

## 3. Eviction — Suspect → Dead

Eviction is automatic and **passive**: peers are not actively kicked,
just downgraded as their `last_seen` ages out.  The per-tick liveness
sweep in
[`peer_table::PeerTable::sweep_states`](../src/cluster/peer_table.rs):

```text
for each peer p:
  if now() - p.last_seen >= dead_timeout:    p.state = Dead
  elif now() - p.last_seen >= suspect_timeout: p.state = Suspect
  else:                                       p.state = Alive  (no change)
```

State transitions affect the cluster surface in three ways:

1. **Read fan-out target set** —
   [`fanout::fan_out_v2`](../src/cluster/fanout.rs) calls
   `peers.read().alive()` and only ever fans out to Alive peers.
   Suspect/Dead peers are never queried in the same tick.

2. **Write replication target set** —
   [`replication::replicate_to_all`](../src/cluster/replication.rs) /
   `pick_random_alive` use the same Alive set.  Replication failures
   to a Dead peer enqueue hints (see § 8) so the missed write is
   retried after recovery.

3. **Cluster mode** — `cluster.mode()` returns:
   - `Standalone` when `alive_count == 0`
   - `Partial`    when `0 < alive_count < full_mode_threshold`
   - `Full`       when `alive_count >= full_mode_threshold`

   The bdsnode dashboard displays this; `v3/cluster.status` returns it.

Because eviction is passive, a network blip that drops a peer for 30 s
moves it to Suspect for one minute and then back to Alive on the next
successful ping — no operator action required.

---

## 4. Re-acceptance — recovery probe + Hello

A node coming back from a longer outage (`alive_count == 0` on its
side, or peers all in Suspect/Dead) runs two recovery mechanisms:

### 4.1 Recovery probe

At every gossip tick, if `alive_count == 0`, the node calls
[`gossip::probe_recovery`](../src/cluster/gossip.rs):

1. Pick a random Suspect or Dead peer from the local table.
2. Send `v3/cluster.ping` with the standard HMAC payload.
3. On success: mark Alive, `last_seen = now()`.

This handles "the rest of the cluster is fine, our network came back"
in a single tick — no full bootstrap pass needed.

### 4.2 Periodic re-bootstrap

When the recovery probe finds nothing (every Dead peer is genuinely
gone, or the local peer table is stale), the node falls back to a
full bootstrap pass every `bootstrap_retry_interval` (default 60 s).
This re-runs §2.1 against `cluster.bootstrap` plus `peers.json` URLs
in parallel.  The first peer to respond seeds the table; the others
are picked up via that peer's response `peers[]`.

### 4.3 Re-arrival from the peer's perspective

When the recovering peer's Hello/Ping reaches a node that already had
it as Suspect or Dead, the receiver:

- Updates `state = Alive`, `last_seen = now()`.
- (For Hello) refreshes the cached `version` / `embedding_model`.
- Persists `peers.json`.
- Hint replay (see § 8) drains pending hints for that node on the
  next `hint_replay_interval` tick (default 10 s).

So a recovering peer typically catches up on missed writes within ~15
seconds of becoming reachable again.

---

## 5. Schedule control — cluster-aware Scheduler

Section 12 of [`CLUSTER.md`](CLUSTER.md) covers the user-facing
behaviour.  Here is the wire-level flow.

The Scheduler tick loops through every stored script whose cron
pattern matches the current minute.  In standalone mode it submits
each immediately.  In cluster mode each fire goes through:

### 5.1 Local check

```rust
cluster.scheduler_log.last_executed(script_id)?  // SQL: SELECT MAX(executed_at) ...
```

This is a single DuckDB lookup against
`<dbpath>/network/scheduler_log.duckdb`.  If the result is within
`scheduler_dedup_window` of `now`, the tick logs at debug and
returns early — no fan-out.

### 5.2 Fan-out to peers

If the local log shows nothing recent, the tick calls
[`fanout::fan_out_v2`](../src/cluster/fanout.rs) against every Alive
peer with the unauthenticated `v2/scheduler.last_seen` method:

```json
POST http://10.0.0.6:9000
{
  "jsonrpc": "2.0", "id": 1,
  "method": "v2/scheduler.last_seen",
  "params": { "script_id": "019e1559-72f2-7e41-9260-c39b43ea9168" }
}
```

Each peer responds with **its own** local log query result:

```json
{ "jsonrpc": "2.0", "id": 1,
  "result": { "last_executed_at": 1778464673 } }
```

`null` when that peer has never run it.

### 5.3 Decide

The Scheduler takes `max(local, peer1, peer2, …)`.  If `now - max <
dedup_window`, the tick logs at debug and skips:

```
[scheduler] skip 019e1559-… — already executed cluster-wide within 300s
```

### 5.4 Record + fire

Otherwise the Scheduler records its own execution **before** firing:

```rust
cluster.scheduler_log.record(script_id, this_node_id, now_secs())?;
submit_script_with_id(script_id, &script_body)?;
```

The record-then-fire order means the very next tick (on this OR any
other node) will see this execution and skip.

### 5.5 Pruning

At the start of every tick, rows older than `2 × dedup_window` are
deleted via:

```sql
DELETE FROM scheduler_log WHERE executed_at < (now - 2 * window)
```

So the file stays bounded regardless of run history.

### 5.6 Inspection from outside

`bdscmd scheduler-last-seen <id>` queries any node directly:

```bash
$ bdscmd -a http://node1:9000 scheduler-last-seen 019e1559-…
{ "last_executed_at": 1778464673 }

$ bdscmd -a http://node2:9000 scheduler-last-seen 019e1559-…
{ "last_executed_at": 1778464673 }   # both nodes saw the same fire
```

If different nodes report widely different timestamps, the dedup
window is too short — widen `cluster.scheduler_dedup_window`.

---

## 6. Data distribution

bdsnode has two storage families with different distribution rules:

### 6.1 Sharded stores

Observability records (the per-shard DuckDB tables under
`<dbpath>/shards/`) are **sharded by ingest time + replication
factor**.  Each `v3/add` lands the new record on:

- This node (always — local DB is the coordinator's source of truth).
- `replication_factor - 1` random Alive peers, picked per-record.

Reads (`v3/search`, `v3/count`, `v3/timeline`, …) fan out to **every**
Alive peer and merge by UUID dedup.  Replicated copies of the same
record produce one merged entry; missing replicas (due to past
partial-mode ingest) silently widen the searched corpus.

### 6.2 Fully-replicated stores

Three stores live on every node:

| Store    | Path                  | What                                       |
|----------|-----------------------|--------------------------------------------|
| docs     | `<dbpath>/docs/`      | Document store + embeddings                |
| signals  | `<dbpath>/signals/`   | Signal events + metadata                   |
| scripts  | `<dbpath>/scripts/`   | Stored Bund scripts + cron schedules       |

Configured via `cluster.full_replication_stores` (default
`["docs","signals","scripts"]`).  Writes go local-first then fan out
to **every** Alive peer (see § 7).  Reads are local-only — anti-
entropy keeps every node's copy converged within a few minutes.

### 6.3 Per-network state (cluster-only)

Under `<dbpath>/network/`:

| File                    | Purpose                                                                |
|-------------------------|------------------------------------------------------------------------|
| `node_id`               | This node's stable UUIDv7 (created on first start)                     |
| `peers.json`            | Last-known peer table — atomically rewritten by gossip                 |
| `hints.duckdb`          | Hinted-handoff queue for failed write fan-outs (§ 8)                   |
| `tombstones.duckdb`     | Tombstone log for fully-replicated deletes (§ 8.2)                     |
| `scheduler_log.duckdb`  | Cluster-aware Scheduler dedup log (§ 5)                                |

These files are private to the node — they are NOT replicated, but
their effects propagate via the protocols they back.

---

## 7. Write replication — sharded + fully-replicated

### 7.1 Sharded path (`v3/add`, `v3/add.batch`)

The coordinator (any node receiving the v3/add):

1. Mints a UUIDv7 id (or accepts the caller's id when given).
2. Inserts into the local shard.
3. Calls `replication::pick_random_alive(cluster, replication_factor - 1)`.
4. For each picked peer, fires `v2/add` with `{id, doc}` injected:

```json
POST http://10.0.0.6:9000
{
  "jsonrpc": "2.0", "id": 1, "method": "v2/add",
  "params": {
    "id":  "019e1561-7a-…",
    "doc": { "key": "cpu.user", "timestamp": 1778464000, "data": 0.42 }
  }
}
```

5. Failed peer calls enqueue hints (§ 8).

The v3/add response includes per-replica outcome:

```json
{
  "id": "019e1561-7a-…",
  "replication": {
    "peers_attempted": 2,
    "peers_succeeded": 2,
    "hints_queued":    0
  }
}
```

### 7.2 Fully-replicated path (`v3/doc.add`, `v3/signal.emit`, `v3/script.add`, …)

Same shape, but `replication::replicate_to_all` is used instead of
`pick_random_alive`.  Every Alive peer receives the same payload with
the same id so anti-entropy can de-dupe them later.

Receiver-side methods are **idempotent** — `v2/doc.add` with an `id`
field that already exists returns `{id, existing: true}` without
re-writing.  This makes hint replay safe to run multiple times.

### 7.3 Updates (`v3/doc.update.metadata`, `v3/script.update`, …)

Updates carry an `if_newer: true` field.  The receiver compares the
incoming `metadata.updated_at` to the locally-stored `updated_at` and
only applies the update when strictly newer.  This is the LWW
(last-write-wins) convergence protocol; it tolerates concurrent
updates on partitioned coordinators.

---

## 8. Hinted handoff + anti-entropy

Two complementary mechanisms recover from delivery failures.

### 8.1 Hinted handoff (real-time recovery)

When `replicate_to_all` (or sharded `v3/add` fan-out) fails to reach a
peer, the coordinator enqueues a hint into
`<dbpath>/network/hints.duckdb`:

```sql
CREATE TABLE hints (
    seq        BIGINT  PRIMARY KEY DEFAULT nextval('hints_seq'),
    peer_id    TEXT    NOT NULL,
    method     TEXT    NOT NULL,   -- e.g. "v2/doc.add"
    params     BLOB    NOT NULL,   -- serialised params (canonical JSON)
    created_at BIGINT  NOT NULL
);
```

The hint replay task (`bdsnode/server/cluster.rs`) tickles every
`hint_replay_interval` (default 10 s).  For every peer that has
hints AND is currently Alive, it drains a small batch and replays
each hint as a regular v2/* call.  Successes delete the hint row;
failures stay in the queue.

Inspection: `bdscmd cluster hints --secret …` shows pending hint
counts per peer.

### 8.2 Anti-entropy (background convergence)

For fully-replicated stores, the AE loop (every
`antientropy_interval`, default 5 min) does a per-store sweep:

1. Pick one random Alive peer.
2. Fetch the full id list from that peer (`v2/<store>.list_ids`).
3. Diff against the local id list.
4. For each id we have but the peer doesn't, push our copy via the
   regular replicated write method.
5. For each id the peer has but we don't, pull it via the regular
   getter and apply locally.

Tombstones (`tombstones.duckdb`) ensure deletes don't get
resurrected: a missing record on the peer that has been tombstoned
locally stays missing.  Tombstones older than 2× the AE interval are
GC'd to keep the file bounded.

### 8.3 Operator pushdown — `v3/cluster.sync`

`bdscmd cluster sync --secret …` triggers an immediate AE pass
without waiting for the next tick:

```bash
$ bdscmd cluster sync --secret "$BDS_CLUSTER_SECRET"
{
  "ok": true,
  "stores": ["docs", "signals", "scripts"],
  "hints_replayed": 4,
  "tombstones_propagated": 1
}
```

---

## 9. Read fan-out — v3/* surface

Every `v3/*` read method follows the same recipe (encoded in
[`src/cluster/fanout.rs`](../src/cluster/fanout.rs) +
[`src/cluster/merge.rs`](../src/cluster/merge.rs)):

1. Run the local v2 method on a blocking thread.
2. Concurrently call the same `v2/*` method on every Alive peer
   (`fan_out_v2`).
3. Apply the per-method merge strategy.
4. Embed `cluster_meta` in the response.

The merge strategies:

| Strategy                   | Used by                                              | Behaviour                                     |
|----------------------------|------------------------------------------------------|-----------------------------------------------|
| `dedup_avg_score`          | `v3/search`, `v3/search.get`, `v3/aggregationsearch`, `v3/fulltext`, `v3/signals_query`, `v3/tpl.search` | UUID dedup, score = mean across replicas, sort score DESC |
| `dedup_by_id`              | `v3/primaries.get`, `v3/signals`, `v3/tpl.list`, `v3/tpl.templates_recent` | UUID dedup, first-seen wins, optional ts-DESC sort |
| `dedup_by_id_newest_first` | `v3/fulltext.recent`                                 | UUID dedup, sort ts-DESC                      |
| `union_strings`            | `v3/keys`, `v3/keys.all`                             | Sorted string union                           |
| `union_string_ids`         | `v3/primaries`, `v3/count?distinct=true`             | Sorted UUID union                             |
| `pick_largest_by_field`    | `v3/trends`, `v3/rca`, `v3/rca.templates`            | Pick response with largest sample-count field |
| `pick_largest_per_key`     | `v3/topics.all`                                      | Per-key, pick the response with largest corpus|
| `pick_longest_string`      | `v3/textrank.templates`, `v3/summary_*`              | Pick response with longest summary text       |
| `min_max_fields`           | `v3/timeline`                                        | min(min_ts), max(max_ts)                      |
| `sum_field`                | `v3/count` (default mode)                            | Sum per-peer counts (overcounts replicas)     |
| `merge_explore_rows`       | `v3/primaries.explore[.telemetry]`                   | Per-key sum + UUID-set union                  |
| `merge_keys_get_rows`      | `v3/keys.get`                                        | Per-primary-id union of secondary_ids         |

The `cluster_meta` block is uniform across every v3/* read:

```json
"cluster_meta": {
  "enabled":         true,
  "peers_queried":   2,
  "peers_answered":  2,
  "partial":         false,
  "failed":          []
}
```

When a peer returns an error or times out, it appears under `failed`
with `{node_id, url, error}` and `partial: true`.  Bdsweb's
`ModeBadge::from_response` reads exactly this shape to render
"via Cluster · 2/2 peers" or the partial variant.

### 9.1 Example: cluster-wide search

```bash
$ curl -s -X POST http://node1:9000 -H "Content-Type: application/json" -d '{
    "jsonrpc":"2.0","id":1,"method":"v3/search",
    "params":{"session":"","query":"login failed","duration":"6h","limit":10}
  }' | jq '.result.cluster_meta, (.result.results | length)'

{ "enabled": true, "peers_queried": 2, "peers_answered": 2, "partial": false, "failed": [] }
8
```

Behind the scenes:
- Local node ran `v2/search` → 5 hits.
- Peer A ran `v2/search` → 4 hits (3 overlap with local).
- Peer B ran `v2/search` → 6 hits (2 overlap with local, 1 with A).
- `dedup_avg_score` → 8 distinct UUIDs, scores averaged across the
  replicas that returned them, sorted by averaged score, truncated
  to `limit=10`.

---

## 10. Bdsweb mode-aware routing

Bdsweb pages that show telemetry/analysis data call
`client::rpc_versioned(state, v2_method, v3_method, params)`.  The
helper checks the cached `cluster_enabled` flag (refreshed every 30 s
via `v2/status.cluster`) and routes:

- **cluster on**  → `v3_method` (cluster-wide, with badge from
  `cluster_meta`).
- **cluster off** → `v2_method` (local-only, badge "Standalone").

When a v3 counterpart doesn't exist for a given route, the helper
calls `v2_method` and the badge shows "Local node (no cluster
variant)" so operators know they're seeing partial data.

The Bund REPL at `/bund` extends this: every `cls.*` helper that
runs through `vm::api::*` populates a per-thread `cluster_meta`
cell.  The `v2/eval` response carries the cell back, and bdsweb
renders a collapsible "via Cluster · N/M peers" badge above the
script result.  See [`Phase 6 commit`](../#) and
[`examples/cluster/`](../examples/cluster/).

---

## See also

- [`CLUSTER.md`](CLUSTER.md) — configuration, operations, on-disk
  layout, RPC quick reference.
- [`jsonrpc_api/`](jsonrpc_api/) — per-method JSON schemas and
  examples.
- [`BDSCMD.md`](BDSCMD.md) — operator CLI for every `cluster.*`
  subcommand and `scheduler-last-seen`.
- [`../examples/cluster/`](../examples/cluster/) — runnable Bund
  scripts demonstrating `cls.*` helpers + `?cluster.meta`.
