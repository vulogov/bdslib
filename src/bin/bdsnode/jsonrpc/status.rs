use jsonrpsee::RpcModule;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/status", |_params, _ctx, _| async move {
            log::debug!("v2/status: start");

            let state = crate::status::get();

            let uptime_secs     = state.started_at.elapsed().as_secs();
            let timestamp       = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let logs_queue         = bdslib::pipe::len("ingest").unwrap_or(0);
            let json_file_queue    = bdslib::pipe::len("ingest_file").unwrap_or(0);
            let syslog_file_queue  = bdslib::pipe::len("ingest_file_syslog").unwrap_or(0);
            let json_file_name     = state.current_file
                .lock()
                .ok()
                .and_then(|g| g.clone());
            let syslog_file_name   = state.current_syslog_file
                .lock()
                .ok()
                .and_then(|g| g.clone());

            let (jsoncache_pct, jsoncache_len, jsoncache_capacity) =
                match bdslib::get_db() {
                    Ok(db) => (
                        db.jsoncache_utilization_pct(),
                        db.jsoncache_len() as u64,
                        db.jsoncache_capacity() as u64,
                    ),
                    Err(_) => (0, 0, 0),
                };

            let value = serde_json::json!({
                "node_id":           state.node_id,
                "hostname":          state.hostname,
                "uptime_secs":       uptime_secs,
                "timestamp":         timestamp,
                "logs_queue":        logs_queue,
                "json_file_queue":   json_file_queue,
                "json_file_name":    json_file_name,
                "syslog_file_queue": syslog_file_queue,
                "syslog_file_name":  syslog_file_name,
                "jsoncache_pct":      jsoncache_pct,
                "jsoncache_len":      jsoncache_len,
                "jsoncache_capacity": jsoncache_capacity,
            });

            log::debug!("v2/status: done");
            Ok::<serde_json::Value, jsonrpsee::types::ErrorObject>(value)
        })
        .unwrap();
}
