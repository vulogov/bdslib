use askama::Template;
use axum::{extract::State, response::Html};
use serde_json::json;

use crate::{
    client::{fmt_ts, rpc, str_val, u64_val},
    error::AppError,
    state::AppState,
};

#[derive(Debug)]
pub struct ShardRow {
    pub label:           String,
    pub primary_count:   u64,
    pub secondary_count: u64,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    node_id:              String,
    hostname:             String,
    uptime_secs:          u64,
    logs_queue:           u64,
    json_file_queue:      u64,
    syslog_file_queue:    u64,
    total_count:          u64,
    min_ts:               String,
    max_ts:               String,
    total_shards:         usize,
    shards:               Vec<ShardRow>,
    shard_labels_json:    String,
    shard_primary_json:   String,
    shard_secondary_json: String,
    jsoncache_pct:        u64,
    jsoncache_len:        u64,
    jsoncache_capacity:   u64,
}

const RECENT_SHARDS: usize = 5;

pub async fn handler(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let (status_v, count_v, timeline_v, shards_v) = tokio::try_join!(
        rpc(&state, "v2/status",   json!({})),
        rpc(&state, "v2/count",    json!({})),
        rpc(&state, "v2/timeline", json!({})),
        rpc(&state, "v2/shards",   json!({})),
    )?;

    let shard_arr = shards_v.as_array().cloned().unwrap_or_default();
    let total_shards = shard_arr.len();

    // Show only the 5 most recent shards (last entries, chronologically newest).
    let recent = if shard_arr.len() > RECENT_SHARDS {
        &shard_arr[shard_arr.len() - RECENT_SHARDS..]
    } else {
        &shard_arr[..]
    };

    let mut shards         = Vec::with_capacity(recent.len());
    let mut labels         = Vec::with_capacity(recent.len());
    let mut primary_cnts   = Vec::with_capacity(recent.len());
    let mut secondary_cnts = Vec::with_capacity(recent.len());

    for s in recent {
        let start = u64_val(s, "start_ts");
        let p     = u64_val(s, "primary_count");
        let sec   = u64_val(s, "secondary_count");
        let label = fmt_ts(start);
        labels.push(label.clone());
        primary_cnts.push(p);
        secondary_cnts.push(sec);
        shards.push(ShardRow { label, primary_count: p, secondary_count: sec });
    }

    let tmpl = DashboardTemplate {
        node_id:              str_val(&status_v, "node_id"),
        hostname:             str_val(&status_v, "hostname"),
        uptime_secs:          u64_val(&status_v, "uptime_secs"),
        logs_queue:           u64_val(&status_v, "logs_queue"),
        json_file_queue:      u64_val(&status_v, "json_file_queue"),
        syslog_file_queue:    u64_val(&status_v, "syslog_file_queue"),
        total_count:          u64_val(&count_v,   "count"),
        min_ts:               fmt_ts(u64_val(&timeline_v, "min_ts")),
        max_ts:               fmt_ts(u64_val(&timeline_v, "max_ts")),
        total_shards,
        shards,
        shard_labels_json:    serde_json::to_string(&labels)?,
        shard_primary_json:   serde_json::to_string(&primary_cnts)?,
        shard_secondary_json: serde_json::to_string(&secondary_cnts)?,
        jsoncache_pct:        u64_val(&status_v, "jsoncache_pct"),
        jsoncache_len:        u64_val(&status_v, "jsoncache_len"),
        jsoncache_capacity:   u64_val(&status_v, "jsoncache_capacity"),
    };

    Ok(Html(tmpl.render()?))
}
