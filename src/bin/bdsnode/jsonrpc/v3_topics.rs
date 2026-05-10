//! `v3/topics`, `v3/topics.all` — cluster-wide LDA topic analysis.
//!
//! LDA outputs aren't directly mergeable across peers (the model is
//! corpus-relative).  Strategy:
//!
//! - **`v3/topics`** (one key): pick the peer with the largest corpus
//!   for that key and return its topic summary.  Replicated stores
//!   typically converge so all peers report similar summaries; picking
//!   the largest avoids partial-corpus distortions.
//! - **`v3/topics.all`** (every key): per-key, do the same — the merged
//!   `topics[]` array contains one entry per distinct key, each picked
//!   from whichever peer reported the largest corpus.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::fanout;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

pub fn register(module: &mut RpcModule<()>) {
    register_topics(module);
    register_topics_all(module);
}

fn lda_corpus_size(v: &JsonValue) -> u64 {
    // Look for the obvious size fields; fall back to 0.
    v.get("n_records").and_then(|x| x.as_u64())
        .or_else(|| v.get("count").and_then(|x| x.as_u64()))
        .or_else(|| v.get("total_records").and_then(|x| x.as_u64()))
        .unwrap_or(0)
}

fn register_topics(module: &mut RpcModule<()>) {
    module.register_async_method("v3/topics", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));

        let raw_local = raw.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let key = raw_local.get("key").and_then(|v| v.as_str())
                .ok_or_else(|| rpc_err(-32602, "missing 'key'"))?.to_owned();
            let dur = raw_local.get("duration").and_then(|v| v.as_str())
                .ok_or_else(|| rpc_err(-32602, "missing 'duration'"))?.to_owned();
            humantime::parse_duration(&dur)
                .map_err(|e| rpc_err(-32600, format!("invalid duration {dur:?}: {e}")))?;
            let cfg = bdslib::LdaConfig {
                k:     raw_local.get("k").and_then(|v| v.as_u64()).unwrap_or(3) as usize,
                alpha: raw_local.get("alpha").and_then(|v| v.as_f64()).unwrap_or(0.1),
                beta:  raw_local.get("beta").and_then(|v| v.as_f64()).unwrap_or(0.01),
                seed:  raw_local.get("seed").and_then(|v| v.as_u64()).unwrap_or(42),
                iters: raw_local.get("iters").and_then(|v| v.as_u64()).unwrap_or(200) as usize,
                top_n: raw_local.get("top_n").and_then(|v| v.as_u64()).unwrap_or(10) as usize,
            };
            let summary = bdslib::TopicSummary::query_window(&key, &dur, cfg)
                .map_err(|e| rpc_err(-32004, e))?;
            serde_json::to_value(&summary)
                .map_err(|e| rpc_err(-32004, format!("serialise: {e}")))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/topics", raw).await),
            None    => None,
        };

        // Pick the response with the largest corpus.
        let mut best: &JsonValue = &local;
        let mut best_n = lda_corpus_size(&local);
        if let Some(f) = &fan {
            for r in f.ok_results() {
                let n = lda_corpus_size(r);
                if n > best_n { best = r; best_n = n; }
            }
        }
        let mut out = best.clone();
        if let Some(obj) = out.as_object_mut() {
            obj.insert("cluster_meta".into(), v3_cluster_meta(fan));
            obj.insert("corpus_size".into(),  JsonValue::from(best_n));
        }
        Ok::<JsonValue, ErrorObject>(out)
    }).unwrap();
}

fn register_topics_all(module: &mut RpcModule<()>) {
    module.register_async_method("v3/topics.all", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));

        let raw_local = raw.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let dur = raw_local.get("duration").and_then(|v| v.as_str())
                .ok_or_else(|| rpc_err(-32602, "missing 'duration'"))?.to_owned();
            humantime::parse_duration(&dur)
                .map_err(|e| rpc_err(-32600, format!("invalid duration {dur:?}: {e}")))?;
            let cfg = bdslib::LdaConfig {
                k:     raw_local.get("k").and_then(|v| v.as_u64()).unwrap_or(3) as usize,
                alpha: raw_local.get("alpha").and_then(|v| v.as_f64()).unwrap_or(0.1),
                beta:  raw_local.get("beta").and_then(|v| v.as_f64()).unwrap_or(0.01),
                seed:  raw_local.get("seed").and_then(|v| v.as_u64()).unwrap_or(42),
                iters: raw_local.get("iters").and_then(|v| v.as_u64()).unwrap_or(200) as usize,
                top_n: raw_local.get("top_n").and_then(|v| v.as_u64()).unwrap_or(10) as usize,
            };
            let summaries = bdslib::TopicSummary::query_all_keys(&dur, cfg)
                .map_err(|e| rpc_err(-32004, e))?;
            let topics: Vec<JsonValue> = summaries.into_iter()
                .map(|s| serde_json::to_value(&s).unwrap_or(JsonValue::Null))
                .collect();
            Ok(serde_json::json!({ "topics": topics }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/topics.all", raw).await),
            None    => None,
        };

        // Per-key, pick the entry with the largest corpus across local+peers.
        let mut best_per_key: HashMap<String, (u64, JsonValue)> = HashMap::new();
        let mut bodies: Vec<&JsonValue> = vec![&local];
        if let Some(f) = &fan { bodies.extend(f.ok_results()); }
        for body in &bodies {
            if let Some(arr) = body.get("topics").and_then(|v| v.as_array()) {
                for item in arr {
                    let key = match item.get("key").and_then(|v| v.as_str()) {
                        Some(s) => s.to_owned(),
                        None    => continue,
                    };
                    let n = lda_corpus_size(item);
                    match best_per_key.get(&key) {
                        Some((cur_n, _)) if n <= *cur_n => {}
                        _ => { best_per_key.insert(key, (n, item.clone())); }
                    }
                }
            }
        }
        let mut topics: Vec<JsonValue> = best_per_key.into_values().map(|(_, v)| v).collect();
        topics.sort_by(|a, b| {
            let ka = a.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let kb = b.get("key").and_then(|v| v.as_str()).unwrap_or("");
            ka.cmp(kb)
        });

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "topics":       topics,
            "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}
