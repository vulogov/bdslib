use super::params::rpc_err;
use jsonrpsee::RpcModule;

fn default_limit() -> usize { 10 }

#[derive(serde::Deserialize)]
struct SearchParams {
    #[allow(dead_code)]
    session: String,
    query: String,
    duration: String,
    #[serde(default = "default_limit")]
    limit: usize,
    /// Optional pre-computed embedding vector.  When present, v2/search
    /// skips its own ONNX `embed()` call and feeds the vector straight
    /// into `vectorsearch_with_vec`.  The coordinator (`v3/search`)
    /// populates this so the cluster embeds the query exactly once
    /// instead of N+1 times.
    #[serde(default)]
    query_vec: Option<Vec<f32>>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/search", |params, _ctx, _| async move {
            log::debug!("v2/search: start");
            let p: SearchParams = params.parse()?;

            let result = tokio::task::spawn_blocking(move || {
                log::debug!(
                    "v2/search: session={} query={:?} duration={} limit={} prevec={}",
                    p.session, p.query, p.duration, p.limit, p.query_vec.is_some()
                );

                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let query_json = serde_json::json!(p.query);
                let hits = match p.query_vec {
                    Some(qv) => {
                        let fp = bdslib::json_fingerprint(&query_json);
                        db.vectorsearch_with_vec(&p.duration, &fp, &qv, p.limit)
                            .map_err(|e| rpc_err(-32004, e))?
                    }
                    None => db
                        .vectorsearch(&p.duration, &query_json, p.limit)
                        .map_err(|e| rpc_err(-32004, e))?,
                };

                let results: Vec<serde_json::Value> = hits
                    .into_iter()
                    .map(|(id, ts, score)| {
                        serde_json::json!({
                            "id":        id.to_string(),
                            "timestamp": ts,
                            "score":     score,
                        })
                    })
                    .collect();

                Ok::<serde_json::Value, jsonrpsee::types::ErrorObject>(
                    serde_json::json!({ "results": results }),
                )
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;

            log::debug!("v2/search: done");
            result
        })
        .unwrap();
}
