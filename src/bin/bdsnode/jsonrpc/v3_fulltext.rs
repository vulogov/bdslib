//! `v3/fulltext`, `v3/fulltext.get`, `v3/fulltext.recent` — cluster-wide
//! BM25 full-text search.
//!
//! Each peer runs its v2 counterpart against its own corpus; the
//! coordinator merges by UUID, applies the per-method ranking, and
//! truncates to `limit`.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::{fanout, merge};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

fn default_limit() -> usize { 10 }

#[derive(serde::Deserialize, Clone)]
struct Params {
    #[serde(default)] session: String,
    query: String,
    duration: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

pub fn register(module: &mut RpcModule<()>) {
    register_one(module, "v3/fulltext",        FtMode::Score);
    register_one(module, "v3/fulltext.get",    FtMode::Docs);
    register_one(module, "v3/fulltext.recent", FtMode::Recent);
}

#[derive(Copy, Clone)]
enum FtMode { Score, Docs, Recent }

fn register_one(module: &mut RpcModule<()>, method: &'static str, mode: FtMode) {
    module.register_async_method(method, move |params, _ctx, _| async move {
        log::debug!("{method}: start");
        let p: Params = params.parse()?;

        let v2_method = match mode {
            FtMode::Score  => "v2/fulltext",
            FtMode::Docs   => "v2/fulltext.get",
            FtMode::Recent => "v2/fulltext.recent",
        };
        let v2_params = serde_json::json!({
            "session": p.session, "query": p.query,
            "duration": p.duration, "limit": p.limit,
        });

        // Local + fan-out concurrently.
        let p_local = p.clone();
        let local_fut = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let results: Vec<JsonValue> = match mode {
                FtMode::Score => db.fulltextsearch(&p_local.duration, &p_local.query, p_local.limit)
                    .map_err(|e| rpc_err(-32002, e))?
                    .into_iter()
                    .map(|(id, score)| serde_json::json!({"id": id.to_string(), "score": score}))
                    .collect(),
                FtMode::Docs => db.search_fts(&p_local.duration, &p_local.query)
                    .map_err(|e| rpc_err(-32002, e))?,
                FtMode::Recent => db.fulltextsearch_recent(&p_local.duration, &p_local.query, p_local.limit)
                    .map_err(|e| rpc_err(-32002, e))?
                    .into_iter()
                    .map(|(id, ts, score)| serde_json::json!({
                        "id": id.to_string(), "timestamp": ts, "score": score,
                    }))
                    .collect(),
            };
            Ok(serde_json::json!({ "results": results }))
        });

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fanout_fut = async {
            match &cluster {
                Some(c) => Some(fanout::fan_out_v2(c, v2_method, v2_params).await),
                None    => None,
            }
        };

        let (local_res, fan) = tokio::join!(local_fut, fanout_fut);
        let local = local_res
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let bodies = merge::bodies_from(&local, fan.as_ref());
        let mut merged: Vec<JsonValue> = match mode {
            FtMode::Score  => merge::dedup_avg_score(bodies, "results"),
            FtMode::Docs   => merge::dedup_by_id(bodies, "results"),
            FtMode::Recent => merge::dedup_by_id_newest_first(bodies, "results"),
        };
        merged.truncate(p.limit);

        log::debug!("{method}: done (n={})", merged.len());
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "results":      merged,
            "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}
