//! Generic local-vs-cluster dispatch shared by every `vm::api::*` read.
//!
//! Cluster-aware Bund helpers all follow the same recipe:
//!
//! 1. Look up the global DB.
//! 2. Run the local computation (synchronously — we're inside a Bund
//!    word handler, on a blocking thread under bdsnode/bdsweb or the
//!    fallback runtime under bdscmd).
//! 3. If `db.cluster().is_none()` → standalone: return the local
//!    value, clear `meta::LAST_META`.
//! 4. Otherwise: drive `fanout::fan_out_v2` to completion via
//!    `runtime::block_on`, hand both the local value and the fan
//!    results to the per-method `merge` closure, store the new
//!    `cluster_meta` in `meta::LAST_META`, and return the merged value.
//!
//! [`read`] captures all of this so each area module's helper is just:
//!
//! ```ignore
//! dispatch::read("v2/topics", params,
//!     || local_topics(...),
//!     |local, fan| merge::pick_largest_by_field(&local, fan, "n_records").0,
//! )
//! ```
//!
//! Writes use a different shape (no merge — a local commit followed by
//! best-effort hinted handoff fan-out) and live in
//! `vm::api::dispatch_write`.

use crate::cluster::fanout::{self, FanOutResults};
use crate::cluster::replication;
use crate::vm::api::{meta, runtime};
use easy_error::{err_msg, Error};
use serde_json::Value as JsonValue;

/// Cluster-aware read dispatcher.  See module docs for the recipe.
///
/// `local` is a synchronous closure that produces the local body in v2
/// shape.  `merge` collapses `(local_body, peer_results)` into a final
/// JsonValue — same signature the per-method helpers in
/// `crate::cluster::merge` expose.
///
/// Returns `Err` only when the global DB hasn't been initialised yet
/// (callers usually pre-check with `?db`).
pub fn read<L, M>(
    fan_method: &str,
    params:     JsonValue,
    local:      L,
    merge:      M,
) -> Result<JsonValue, Error>
where
    L: FnOnce() -> Result<JsonValue, Error>,
    M: FnOnce(JsonValue, Option<&FanOutResults>) -> JsonValue,
{
    let db = crate::globals::get_db()
        .map_err(|e| err_msg(format!("vm::api: global DB unavailable: {e}")))?;
    let local_value = local()?;
    let cluster = match db.cluster() {
        Some(c) => c.clone(),
        None    => {
            // Standalone — no fan-out, no merge across peers.  Pass
            // `None` to merge so per-method post-processing (sort,
            // truncate) still runs uniformly.
            meta::clear();
            return Ok(merge(local_value, None));
        }
    };
    let fan = runtime::block_on(fanout::fan_out_v2(&cluster, fan_method, params));
    let merged = merge(local_value, Some(&fan));
    meta::set(cluster_meta(&fan));
    Ok(merged)
}

/// Build the same `cluster_meta` block the bdsnode v3/* handlers embed.
/// Set on the per-thread `meta::LAST_META` after every successful
/// cluster fan-out so `?cluster.meta` Bund word can return it.
fn cluster_meta(fan: &FanOutResults) -> JsonValue {
    let mut m = fan.cluster_meta();
    if let Some(obj) = m.as_object_mut() {
        obj.insert("enabled".into(), JsonValue::Bool(true));
    }
    m
}

// ─────────────────────────────────────────────────────────────────────────────
// Write dispatchers — fully-replicated and sharded.
// ─────────────────────────────────────────────────────────────────────────────

/// Cluster-aware write that replicates to **every** Alive peer.  Use
/// for fully-replicated stores (docs, signals, scripts, templates).
///
/// `local` commits the record locally and returns whatever id Bund
/// should see (typically the UUID as a string).  `inject_id` is given
/// the local id and a mutable copy of `params` so it can splice the
/// id into the v2 fan-out parameters; if you don't need the id in the
/// params (rare), pass `|_, _|` and forward `params` unchanged.
///
/// `meta::LAST_META` is set to a Json object containing both the
/// `cluster_meta` block (peers_queried/answered/partial/failed) and a
/// `replication` block ({peers_attempted, peers_succeeded,
/// hints_queued}) for write-side introspection from `?cluster.meta`.
pub fn write_replicated<L>(
    fan_method: &'static str,
    mut params: JsonValue,
    local:      L,
    inject_id:  fn(&str, &mut JsonValue),
) -> Result<String, Error>
where
    L: FnOnce() -> Result<String, Error>,
{
    let db = crate::globals::get_db()
        .map_err(|e| err_msg(format!("vm::api: global DB unavailable: {e}")))?;
    let id = local()?;
    let cluster = match db.cluster() {
        Some(c) => c.clone(),
        None    => { meta::clear(); return Ok(id); }
    };
    inject_id(&id, &mut params);
    let outcome = runtime::block_on(replication::replicate_to_all(cluster.clone(), fan_method, params));
    meta::set(serde_json::json!({
        "enabled":    true,
        "replication": outcome.to_json(),
    }));
    Ok(id)
}

/// Cluster-aware write that replicates to `replication_factor - 1`
/// random Alive peers.  Use for the standard `v3/add` write path.
///
/// Same return shape as [`write_replicated`].  Fan-out failures still
/// enqueue hints via `replicate_one`; the random subset matches the
/// `v3/add` policy so a record lands on `replication_factor` distinct
/// nodes (1 local + RF−1 peers).
pub fn write_sharded<L>(
    fan_method: &'static str,
    mut params: JsonValue,
    local:      L,
    inject_id:  fn(&str, &mut JsonValue),
) -> Result<String, Error>
where
    L: FnOnce() -> Result<String, Error>,
{
    let db = crate::globals::get_db()
        .map_err(|e| err_msg(format!("vm::api: global DB unavailable: {e}")))?;
    let id = local()?;
    let cluster = match db.cluster() {
        Some(c) => c.clone(),
        None    => { meta::clear(); return Ok(id); }
    };
    let rf  = cluster.config.replication_factor;
    let pick_k = rf.saturating_sub(1);
    let peers = replication::pick_random_alive(&cluster, pick_k);
    inject_id(&id, &mut params);

    let outcome = runtime::block_on(async {
        let mut attempted = 0usize;
        let mut succeeded = 0usize;
        let mut hinted    = 0usize;
        if peers.is_empty() {
            return replication::Outcome { peers_attempted: 0, peers_succeeded: 0, hints_queued: 0 };
        }
        let canonical = serde_json::to_vec(&params).unwrap_or_default();
        let mut joins = Vec::with_capacity(peers.len());
        for peer in peers {
            attempted += 1;
            let cluster = cluster.clone();
            let params  = params.clone();
            let bytes   = canonical.clone();
            joins.push(tokio::spawn(async move {
                match replication::call_peer_v2(&cluster, &peer.url, fan_method, &params).await {
                    Ok(_)  => true,
                    Err(e) => {
                        log::warn!("[vm::api {fan_method}] -> {} failed: {e}; hinting", peer.url);
                        if let Err(err) = cluster.hints.enqueue(peer.node_id, fan_method, &bytes) {
                            log::error!("[vm::api {fan_method}] enqueue hint for {}: {err}", peer.url);
                        }
                        false
                    }
                }
            }));
        }
        for j in joins {
            match j.await {
                Ok(true)  => succeeded += 1,
                Ok(false) => hinted    += 1,
                Err(e)    => log::error!("[vm::api {fan_method}] task panicked: {e:?}"),
            }
        }
        replication::Outcome { peers_attempted: attempted, peers_succeeded: succeeded, hints_queued: hinted }
    });

    meta::set(serde_json::json!({
        "enabled":     true,
        "replication": outcome.to_json(),
    }));
    Ok(id)
}

/// Local-only write — used for operations that don't have cluster
/// fan-out semantics (e.g. `delete_by_id` for shard records that aren't
/// in a fully-replicated store, or one-off DDL).  Always clears the
/// per-thread meta on the way out so a subsequent `?cluster.meta`
/// returns nodata rather than stale info.
pub fn write_local<L, T>(local: L) -> Result<T, Error>
where
    L: FnOnce() -> Result<T, Error>,
{
    let _db = crate::globals::get_db()
        .map_err(|e| err_msg(format!("vm::api: global DB unavailable: {e}")))?;
    let result = local()?;
    meta::clear();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `read` returns the local value untouched when the merge closure
    /// is the identity.  Verifies the basic plumbing without standing
    /// up a cluster.  (We can't easily exercise the cluster path in a
    /// unit test because `globals::get_db()` returns `Err` until
    /// `init_db()` runs; the smoke test in Phase 4 covers it end-to-end.)
    #[test]
    fn read_errors_without_global_db() {
        let res = read(
            "v2/example",
            json!({}),
            || Ok(json!({"hello": "world"})),
            |local, _fan| local,
        );
        assert!(res.is_err(), "no global DB => Err");
        let e = res.unwrap_err().to_string();
        assert!(e.contains("global DB unavailable"), "error mentions DB: {e}");
    }
}
