use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::{client::rpc, error::AppError, state::AppState};

/// Returns `{"bdsweb": "<ver>", "bdsnode": "<ver>"}` for the footer to display.
///
/// The bdsweb version is compiled in via `CARGO_PKG_VERSION`. The bdsnode
/// version is fetched from `v2/status`; on RPC failure the field falls back
/// to `"unknown"` so the footer still renders.
pub async fn version(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let bdsnode_ver = match rpc(&state, "v2/status", json!({})).await {
        Ok(v) => v.get("version")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_owned(),
        Err(_) => "unknown".to_owned(),
    };
    Ok(Json(json!({
        "bdsweb":  env!("CARGO_PKG_VERSION"),
        "bdsnode": bdsnode_ver,
    })))
}
