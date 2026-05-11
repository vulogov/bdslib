//! `v3/rca`, `v3/rca.templates` — cluster-wide root-cause analysis.
//!
//! RCA output (event clusters + causal candidates) is corpus-relative and
//! not directly mergeable across peers, so the strategy mirrors v3/topics:
//! fan out the v2 method to every Alive peer, then **pick the response
//! with the largest `n_events`**.  In a fully-replicated steady state
//! every peer sees roughly the same corpus and the picks are equivalent;
//! during partial-mode or recovery, picking the richest corpus avoids
//! returning a result derived from a stale subset.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::{fanout, merge};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

pub fn register(module: &mut RpcModule<()>) {
    register_one(module, "v3/rca",           "v2/rca",           Kind::Events);
    register_one(module, "v3/rca.templates", "v2/rca.templates", Kind::Templates);
}

#[derive(Copy, Clone)]
enum Kind { Events, Templates }

fn register_one(module: &mut RpcModule<()>, v3_method: &'static str, v2_method: &'static str, kind: Kind) {
    module.register_async_method(v3_method, move |params, _ctx, _| async move {
        let raw: JsonValue = params.parse()?;
        let raw_local = raw.clone();

        // Local computation on a blocking thread — same code path the
        // existing v2 handlers use.
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let dur = raw_local.get("duration").and_then(|v| v.as_str())
                .ok_or_else(|| rpc_err(-32602, "missing 'duration'"))?.to_owned();
            humantime::parse_duration(&dur)
                .map_err(|e| rpc_err(-32600, format!("invalid duration {dur:?}: {e}")))?;

            let failure_key = raw_local.get("failure_key").and_then(|v| v.as_str()).map(str::to_owned);
            let bucket_secs = raw_local.get("bucket_secs").and_then(|v| v.as_u64()).unwrap_or(300);
            let min_support = raw_local.get("min_support").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
            let jaccard     = raw_local.get("jaccard_threshold").and_then(|v| v.as_f64()).unwrap_or(0.2);
            let max_keys    = raw_local.get("max_keys").and_then(|v| v.as_u64()).unwrap_or(200) as usize;

            match kind {
                Kind::Events => {
                    let cfg = bdslib::RcaConfig {
                        bucket_secs, min_support, jaccard_threshold: jaccard, max_keys,
                    };
                    let r = match &failure_key {
                        Some(fk) => bdslib::RcaResult::analyze_failure(fk, &dur, &cfg),
                        None     => bdslib::RcaResult::analyze(&dur, &cfg),
                    }.map_err(|e| rpc_err(-32004, e))?;
                    serde_json::to_value(&r)
                        .map_err(|e| rpc_err(-32004, format!("serialise: {e}")))
                }
                Kind::Templates => {
                    let cfg = bdslib::RcaTemplatesConfig {
                        bucket_secs, min_support, jaccard_threshold: jaccard, max_keys,
                    };
                    let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                    let r = match &failure_key {
                        Some(fk) => bdslib::RcaTemplatesResult::analyze_failure(db, fk, &dur, &cfg),
                        None     => bdslib::RcaTemplatesResult::analyze(db, &dur, &cfg),
                    }.map_err(|e| rpc_err(-32004, e))?;
                    serde_json::to_value(&r)
                        .map_err(|e| rpc_err(-32004, format!("serialise: {e}")))
                }
            }
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, v2_method, raw).await),
            None    => None,
        };

        let (mut out, best_n) = merge::pick_largest_by_field(&local, fan.as_ref(), "n_events");
        if let Some(obj) = out.as_object_mut() {
            obj.insert("cluster_meta".into(), v3_cluster_meta(fan));
            obj.insert("corpus_size".into(),  JsonValue::from(best_n));
        }
        Ok::<JsonValue, ErrorObject>(out)
    }).unwrap();
}
