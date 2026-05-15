//! `v2/configuration` — read-only JSON dump of this node's
//! configuration.
//!
//! Returns:
//! - `config_path` — the resolved hjson path (`BDS_CONFIG` env or
//!   the `--config` CLI flag, mirrored into the env at startup by
//!   `main.rs`).
//! - `exists` — true when the file at that path was readable.
//! - `loaded_at_unix` — seconds-since-epoch when this handler read
//!   the file.  The file is re-read on every call, so live config
//!   edits are visible without a restart (the running tasks may
//!   still be using cached values — those are surfaced under their
//!   own `v2/*.settings` endpoints).
//! - `config` — the parsed hjson tree (everything the operator wrote).
//! - `defaults` — the library defaults for the major sections.  Keys
//!   present in `config` override these at runtime; keys absent from
//!   `config` use the value shown here.  Hand-authored from the
//!   `*Config::disabled()` / `Default::default()` impls in
//!   `src/cluster/config.rs`, `src/rebalancer.rs`, `src/retention.rs`,
//!   etc.  Not exhaustive — the canonical reference is BDSCONFIG.md.
//!
//! Unauthenticated v2/* — same trust boundary as `v2/status`.

use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::{json, Value as JsonValue};
use std::time::{SystemTime, UNIX_EPOCH};

use super::params::rpc_err;

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/configuration", |_params, _ctx, _| async move {
            log::debug!("v2/configuration: start");
            let out = read_configuration().map_err(|e| rpc_err(-32001, e))?;
            log::debug!("v2/configuration: done");
            Ok::<JsonValue, ErrorObject>(out)
        })
        .unwrap();
}

fn read_configuration() -> Result<JsonValue, String> {
    let config_path = std::env::var("BDS_CONFIG").ok();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let (exists, parsed): (bool, JsonValue) = match &config_path {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(raw) => {
                // serde_hjson predates serde 1.0's `Serialize`, so we
                // can't target `serde_json::Value` directly.  Parse to
                // `serde_hjson::Value`, walk it, build the JSON tree.
                let v: serde_hjson::Value = serde_hjson::from_str(&raw)
                    .map_err(|e| format!("hjson parse error in {p}: {e}"))?;
                (true, hjson_to_json(&v))
            }
            Err(_) => (false, JsonValue::Null),
        },
        None => (false, JsonValue::Null),
    };

    Ok(json!({
        "config_path":    config_path,
        "exists":         exists,
        "loaded_at_unix": now,
        "config":         parsed,
        "defaults":       defaults_block(),
    }))
}

/// Walk an `serde_hjson::Value` tree and rebuild it as `serde_json::Value`.
/// Needed because serde_hjson predates serde 1.0's `Serialize`, so the
/// usual `serde_json::to_value(&v)` short-cut won't compile.
fn hjson_to_json(v: &serde_hjson::Value) -> JsonValue {
    use serde_hjson::Value as H;
    match v {
        H::Null         => JsonValue::Null,
        H::Bool(b)      => JsonValue::Bool(*b),
        H::I64(i)       => JsonValue::from(*i),
        H::U64(u)       => JsonValue::from(*u),
        H::F64(f)       => serde_json::Number::from_f64(*f)
                              .map(JsonValue::Number)
                              .unwrap_or(JsonValue::Null),
        H::String(s)    => JsonValue::String(s.clone()),
        H::Array(items) => JsonValue::Array(items.iter().map(hjson_to_json).collect()),
        H::Object(map)  => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map.iter() {
                out.insert(k.clone(), hjson_to_json(val));
            }
            JsonValue::Object(out)
        }
    }
}

/// Library defaults for the major top-level config sections.  Mirrors
/// the `*Config::disabled()` / `Default::default()` impls.  Keep in
/// sync when those impls change — there's a unit test below pinning
/// the values, and BDSCONFIG.md is the authoritative reference.
fn defaults_block() -> JsonValue {
    json!({
        "cluster": {
            "enabled":                       false,
            "shared_secret":                 "",
            "bootstrap":                     null,
            "bind_url":                      "",
            "gossip_interval_secs":          5,
            "suspect_timeout_secs":          30,
            "dead_timeout_secs":             120,
            "full_mode_threshold":           3,
            "replication_factor":            3,
            "full_replication_stores":       ["docs","signals","scripts","users","llm_cache","graph"],
            "antientropy_interval_secs":     300,
            "hint_replay_interval_secs":     10,
            "hint_max_age_secs":             86_400,
            "peer_rpc_timeout_secs":         2,
            "max_fingerprints_per_peer":     100_000,
            "floating_bootstrap":            true,
            "bootstrap_retry_interval_secs": 60,
            "scheduler_dedup_window_secs":   300,
            "session_ttl_secs":              8 * 3600,
            "auth_rate_limit_per_minute":    10,
            "adaptive_peer_timeout_enabled": true,
            "adaptive_peer_timeout_multiplier": 3.0,
        },
        "rebalancer": {
            "enabled":                     false,
            "interval_secs":               bdslib::rebalancer::DEFAULT_INTERVAL_SECS,
            "batch_size":                  bdslib::rebalancer::DEFAULT_BATCH_SIZE,
            "max_per_run":                 bdslib::rebalancer::DEFAULT_MAX_PER_RUN,
            "min_replication_factor":      null,
            "pause_if_ingest_lag_p95_ms":  bdslib::rebalancer::DEFAULT_LAG_PAUSE_MS,
        },
        "retention": {
            "enabled":                false,
            "duration_secs":          30 * 24 * 60 * 60,
            "interval_secs":          300,
            "max_evictions_per_run":  50,
            "dry_run":                false,
            "quorum_check_enabled":   false,
            "quorum_min_peers":       1,
        },
        "self_healing": {
            "enabled":                   false,
            "interval_secs":             60,
            "consistency_interval_secs": 300,
            "recreate_failed_shards":    false,
            "failed_shard_recreate_after_secs": 3600,
        },
        "generate_realistic_data": {
            "enabled":       false,
            "interval_secs": 3600,
            "duration":      "6h",
            "total":         2000,
            "scenarios":     5,
            "noise_ratio":   0.7,
            "anomaly_ratio": 0.02,
            "seed":          null,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_block_matches_library_constants() {
        let d = defaults_block();
        let reb = d.get("rebalancer").unwrap();
        assert_eq!(reb["interval_secs"].as_u64().unwrap(),
                   bdslib::rebalancer::DEFAULT_INTERVAL_SECS);
        assert_eq!(reb["batch_size"].as_u64().unwrap() as usize,
                   bdslib::rebalancer::DEFAULT_BATCH_SIZE);
        assert_eq!(reb["max_per_run"].as_u64().unwrap() as usize,
                   bdslib::rebalancer::DEFAULT_MAX_PER_RUN);

        let ret = d.get("retention").unwrap();
        assert_eq!(ret["duration_secs"].as_u64().unwrap(), 30 * 24 * 60 * 60);
        assert_eq!(ret["max_evictions_per_run"].as_u64().unwrap(), 50);
    }
}
