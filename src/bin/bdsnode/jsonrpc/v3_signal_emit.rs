//! `v3/signal.emit` — fully-replicated signal emit.

use super::params::{rpc_err, v3_cluster_meta};
use super::v3_replicated::replicate_to_all;
use bdslib::cluster::fanout::FanOutResults;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct Params {
    #[serde(default)] #[allow(dead_code)] session: String,
    name:      String,
    severity:  String,
    timestamp: u64,
    #[serde(default)] metadata: serde_json::Map<String, JsonValue>,
    #[serde(default)] id: Option<String>,
}

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v3/signal.emit", |params, _ctx, _| async move {
        log::debug!("v3/signal.emit: start");
        let p: Params = params.parse()?;

        let id = match p.id.as_deref() {
            Some(s) => Uuid::parse_str(s).map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?,
            None    => Uuid::now_v7(),
        };

        let id_local = id;
        let name = p.name.clone();
        let severity = p.severity.clone();
        let ts = p.timestamp;
        let meta_local = p.metadata.clone();
        tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            if db.signal_get(id_local).map_err(|e| rpc_err(-32011, e))?.is_some() {
                return Ok::<(), ErrorObject>(());
            }
            db.signal_emit_with_id(id_local, &name, &severity, ts, meta_local)
                .map_err(|e| rpc_err(-32011, e))
        })
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let outcome = match &cluster {
            Some(c) => {
                let v2_params = serde_json::json!({
                    "id":        id.to_string(),
                    "name":      p.name,
                    "severity":  p.severity,
                    "timestamp": p.timestamp,
                    "metadata":  p.metadata,
                });
                Some(replicate_to_all(c.clone(), "v2/signal.emit", v2_params).await)
            }
            None => None,
        };

        let outcome_json = outcome.as_ref().map(|o| o.to_json())
            .unwrap_or_else(|| serde_json::json!({"peers_attempted":0,"peers_succeeded":0,"hints_queued":0}));

        log::debug!("v3/signal.emit: done id={id}");
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "id":           id.to_string(),
            "outcome":      outcome_json,
            "cluster_meta": v3_cluster_meta(None::<FanOutResults>),
        }))
    }).unwrap();
}
