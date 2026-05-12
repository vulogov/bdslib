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
11. [Authentication — `v3/user.*` + sessions](#11-authentication--v3user--sessions)

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

## 11. Authentication — `v3/user.*` + sessions

A 4th fully-replicated cluster store — `users` — backs bdsweb's
login wall and the cluster-wide user-management RPCs.  Mechanics
mirror docs/signals/scripts (§ 6.2, § 7.2, § 8) with one extra
wrinkle: `v3/user.authenticate` is the public login path and is NOT
HMAC-protected.  All other `v3/user.*` calls follow the standard
admin-authentication rules.

### 11.1 Store layout

`<dbpath>/users/users.duckdb` holds:

```sql
CREATE TABLE users (
  id              TEXT PRIMARY KEY,
  username        TEXT UNIQUE NOT NULL,
  credential_hash TEXT NOT NULL,         -- argon2id PHC string
  auth_method     TEXT NOT NULL,         -- "password" | "oauth-google" | "ldap-…"
  metadata        JSON NOT NULL,
  created_at      BIGINT NOT NULL,
  updated_at      BIGINT NOT NULL,
  disabled        BOOLEAN NOT NULL DEFAULT false
);
```

Hashes use argon2id (`m=19 MiB, t=2, p=1, 32-byte output`).  Salt is
randomly generated per-row.  The PHC string serialised into
`credential_hash` carries the parameters used so verifiers stay
forward-compatible if defaults change.

### 11.2 `v3/user.add` (and the bootstrap exception)

Identical recipe to `v3/doc.add` (§ 7.2) — local commit, then fan
out `v2/user.add` to every Alive peer with the same UUIDv7 so all
replicas converge on the same identity.  Replicas re-hash the
plaintext password locally (we deliberately do NOT ship the local
hash; each peer's argon2 setup runs independently so future param
changes can roll out without coordination).

The standard HMAC gate applies — every payload carries `_hmac` over
the canonical params.  **Exception**: when the user store is empty
(`users.is_empty()`) the call is admitted unsigned.  This is the
first-user bootstrap so a fresh deployment can mint its first admin
without distributing the secret ahead of time.

```bash
# First admin on a fresh cluster — no HMAC required.
$ curl -s -X POST http://10.0.0.5:9000 \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":1,"method":"v3/user.add","params":{
          "username":"alice","password":"…","auth_method":"password",
          "metadata":{"display_name":"Alice"}
        }}'
{ "jsonrpc":"2.0","id":1, "result": {
    "id":"019e15a2-…",
    "outcome": { "peers_attempted": 2, "peers_succeeded": 2, "hints_queued": 0 },
    "cluster_meta": { "enabled": false }
  }
}

# After the first user lands, the bootstrap window closes:
$ curl -s -X POST http://10.0.0.5:9000 \
    -d '{"jsonrpc":"2.0","id":1,"method":"v3/user.add","params":{"username":"bob","password":"…"}}'
{ "error": { "code": -32098, "message": "missing _hmac field" } }
```

### 11.3 `v3/user.modify` — LWW

Same HMAC gate as `v3/user.add` (no bootstrap exception).  Local
commit uses `if_newer = false` (operator intent is authoritative);
replicas use `if_newer = true` (default, set by the `v2/user.modify`
receiver) so a stale hint replay can't clobber a concurrent edit.

Wire payload:

```json
{ "id":"019e15a2-…",
  "password":"new-secret",         // optional — when present, re-hashed
  "metadata":{"display_name":"Alice S."},  // optional
  "disabled": false,               // optional
  "new_auth_method":"oauth-google" // optional — switches the row
}
```

### 11.4 `v3/user.delete` — tombstoned

HMAC-protected.  Local delete + write a tombstone for the `"users"`
store (same shared `tombstones.duckdb` used by docs/scripts) +
replicate `v2/user.delete` to every Alive peer.  The anti-entropy
loop applies remote tombstones to local rows on the next tick, so a
delete from n1 reaches a transiently-Dead n2 via either hinted
handoff or AE — whichever recovers first.

### 11.5 `v3/user.authenticate` — public login

NOT HMAC-protected; this is the path human users hit.  Recipe:

1. **Local verify**: look up `username` in `users.duckdb`, dispatch
   the row's `auth_method` to its registered `CredentialVerifier`,
   argon2-verify the password.  Disabled rows, unknown rows, and
   wrong passwords ALL collapse to `Ok(false)` — never disclose
   which leg failed.
2. **Local miss → fan-out fallback**: if the user isn't local yet
   (AE window after a fresh `user.add` on a peer), fan
   `v2/user.get_by_username` with `include_hash: true` out to every
   Alive peer.  For each peer that returns a row, verify the
   credential locally.  First successful match wins.
3. **Issue session token**:

```text
<user_id>.<expires_at_unix_secs>.<hex_hmac_sha256>
```

where the HMAC covers `<user_id>.<expires_at>` keyed by
`cluster.shared_secret`.  Single algorithm hard-coded (HMAC-SHA256)
— no JWT `alg=none` confusion possible.

Response shapes:

```json
// success
{ "ok": true,
  "user_id":       "019e15a2-…",
  "session_token": "019e15a2-…1778507919.7c40…25df72d32fbc91404…",
  "ttl_secs":      28800,
  "expires_at":    1778507919 }

// any failure (unknown user / wrong password / disabled)
{ "ok": false, "error": "invalid credentials" }
```

### 11.6 Anti-entropy — `users` store

AE adds `"users"` to its per-store sweep alongside docs/signals/
scripts (§ 8.2).  Differences from the existing stores:

- **list_ids**: `v2/user.list_ids` returns `{live:[{id, updated_at}],
  tombstones:[{id, deleted_at}]}` matching the existing AE
  contract.
- **pull_one**: when local is missing an id the peer advertises,
  AE calls `v2/user.get_by_id` (returns the FULL row including
  `credential_hash`), then writes it locally via
  `UserStorage::add_with_hash` — bypassing the verifier so the
  exact argon2 hash from the peer lands verbatim.  Two nodes with
  divergent argon2 setups can converge safely.
- **overwrite_one** (LWW for `updated_at > local`): same fetch, then
  delete + re-add locally.  Both halves are idempotent so a partial
  failure converges on the next AE tick.
- **Tombstone application**: AE marks the row deleted on the
  receiver before applying — same flow as docs/scripts.

### 11.7 Bdsweb session middleware

bdsweb sets a `bds_session` cookie (`HttpOnly; SameSite=Lax;
Max-Age=session_ttl`) after a successful login.  The middleware
hits three short-circuits before checking the cookie:

1. **Open-access mode** — `cluster.shared_secret` is empty in the
   bdsweb config → no gating.
2. **Public allow-list** — `/login`, `/logout`, `/version`.
3. **First-user bootstrap window** — `v3/user.list` returns no
   rows (cached 30 s) → middleware passes every request through so
   the operator can reach `/admin/users` to mint the first user.

Only then does the cookie get verified via
`bdslib::cluster::session::verify_session_token`.  Distinct
`SessionError` variants (Malformed, BadEncoding, Expired,
BadSignature) are logged at debug; the user always gets a generic
redirect to `/login?next=<original-path>` so the post-login flow
returns them to where they were.

There is no per-session revocation list in v1.  Cookie deletion
(`/logout`) is purely client-side; an attacker holding a token can
use it until expiry.  Tune `session_ttl` for your threat model.

## 12. LLM surface — `v4/llm.*`

The LLM integration layer plugs three new cluster artefacts into the
Phase 4 + Phase 7 + Phase 6 (scheduler) machinery without changing
any of those layers' wire protocols.  Full architecture +
configuration + per-method schemas are in [`LLM.md`](LLM.md); this
section documents only the cluster-mechanics-facing wire bits.

### 12.1 Replicated inference cache (`llm_cache`)

A 5th fully-replicated store joins `docs / signals / scripts / users`.
Lives at `<dbpath>/llm/cache.duckdb`.  Anti-entropy sweeps it
under the store name `"llm_cache"`; the existing `sync_store` loop
in `bdsnode/server/cluster.rs` grew one extra match arm per branch
(`list_method`, local-known query, tombstone-apply, `pull_one`) —
the same shape that's repeated for `users` and the others.

Distinguishing trait vs the other four stores: cache rows are keyed
by **`cache_key`** (content sha256) AND by **`id`** (UUIDv7).
Lookups by cache fingerprint (`v2/llm.cache.get`); AE walks `id`
space (`v2/llm.cache.list_ids` → `v2/llm.cache.get.by_id`).

Write fan-out:

```
v4/llm.complete coordinator (HMAC)
   │
   ▼
provider call → response
   │
   ▼
local cache.put
   │
   ▼
replicate_to_all → v2/llm.cache.put on every Alive peer
   │
   ▼ (any failure)
hints replay on next interval
```

Anti-entropy pull-one path on a node missing a row:

```
sync_store("llm_cache") sees a remote id we don't have locally
   │
   ▼
v2/llm.cache.get.by_id { id }
   │
   ▼ ({found: true, ...row...})
mgr.cache().put(row)  — idempotent on id AND cache_key
```

`cluster.full_replication_stores` must include `"llm_cache"` for
the sweep to fire.  The library default
(`["docs","signals","scripts","users","llm_cache"]`) already covers
it; setups that override the list explicitly must add it back.

Tombstones are not yet wired for this store — purges therefore
don't propagate across the cluster the way doc/signal/script
deletes do.  Cluster-wide purge relies on TTL expiry + per-node
admin calls.  See [`LLM.md`](LLM.md) § _Operational gotchas_.

### 12.2 Cluster-wide single-execution dedup

Modelled directly on the Phase 6 Scheduler dedup (§ 5 above).  A
per-node `<dbpath>/network/inference_log.duckdb` records every
(cache_key, started_at, finished_at, node_id, state) tuple; the
coordinator path in `vm::api::llm::{complete,analyze}` fans
`v2/llm.last_executed` to every Alive peer before invoking a
provider.

```
cache miss
   ↓
local recent_within(cache_key, window_secs)   — recent done/failed/running here?
   ↓ (nothing)
fan v2/llm.last_executed to every Alive peer
   │
   ▼ for each found-and-fresh row:
       state == "running"  →  SkipRunning
       state == "done"     →  SkipDone
       state == "failed"   →  ignore (retry locally)
       no found rows       →  Acquired (mint a local running row + run)
   │
   ▼
provider call (acquired path)
   │
   ▼
release_done / release_failed   — flips the local row to terminal
```

Same accepted race window as the scheduler: two coordinators that
both query peers in the same sub-second tick and both see no
running rows will both fire.  The phase-3 cache prevents the
second one from doing real work — once the first replicates its
result, the second's `cache_store` finds it already on disk and
short-circuits on the next request.

`SkipRunning` and `SkipDone` are not hard skips — the coordinator
polls the inference cache for up to `wait_max_secs` (default 30s)
hoping the peer's replicated result arrives.  Timeout → fall through
and run anyway, so a stuck peer never deadlocks the caller.

### 12.3 Async job runner

Per-node tokio task spawned alongside the gossip / scheduler / sync
/ AE loops (`bdsnode::server::llm_jobs::start`).  Drains the local
`llm_jobs` queue (DuckDB at `<dbpath>/llm/jobs.duckdb`) — not
replicated; cross-node "is this job claimed elsewhere" is the dedup
lease's job, not the queue's.

Result delivery reuses the existing `ResultQueue` machinery — the
runner pushes `{job_id, kind, state, result|error}` onto
`bdslib::vm::results()` under each job's `result_id`, exactly the
same path queued Bund script evaluations use today.  bdsweb's
existing `/scripts` polling loop (`v2/results.pull`) consumes
LLM async results without code changes.

Cancellation is checked twice — pre-flight (skip the provider
call entirely) and post-flight (discard the result, push a
`cancelled` sentinel).  The provider HTTP request is NOT aborted
mid-flight; that's an accepted tradeoff matching scheduler.

## See also

- [`CLUSTER.md`](CLUSTER.md) — configuration, operations, on-disk
  layout, RPC quick reference.
- [`jsonrpc_api/`](jsonrpc_api/) — per-method JSON schemas and
  examples.
- [`BDSCMD.md`](BDSCMD.md) — operator CLI for every `cluster.*`
  subcommand and `scheduler-last-seen`.
- [`../examples/cluster/`](../examples/cluster/) — runnable Bund
  scripts demonstrating `cls.*` helpers + `?cluster.meta`.
