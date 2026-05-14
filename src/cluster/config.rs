//! Parsing for the `cluster` block of `bds.hjson`.
//!
//! The cluster block is **opt-in**: when absent or `enabled: false`,
//! [`ClusterConfig::from_hjson_str`] returns a config with `enabled = false`
//! and no other validation is performed.

use crate::common::error::{err_msg, Result};

/// Resolved cluster configuration.  Defaults match the values documented in
/// `Documentation/CLUSTER.md`.
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    pub enabled:                  bool,
    pub shared_secret:            String,
    pub bootstrap:                Option<String>,
    pub bind_url:                 String,
    pub gossip_interval_secs:     u64,
    pub suspect_timeout_secs:     u64,
    pub dead_timeout_secs:        u64,
    pub full_mode_threshold:      usize,
    pub replication_factor:       usize,
    pub full_replication_stores:  Vec<String>,
    pub antientropy_interval_secs: u64,
    pub hint_replay_interval_secs: u64,
    pub hint_max_age_secs:        u64,
    pub peer_rpc_timeout_secs:    u64,
    pub max_fingerprints_per_peer: usize,
    /// Floating-bootstrap mode.
    /// - `true`  (default): on startup, try `bootstrap` plus every
    ///   peer in the persisted `peers.json` in parallel.  After total
    ///   failure, retry every `bootstrap_retry_interval`.
    /// - `false` (strict): try only `bootstrap`.  Persisted peers are
    ///   never used as bootstrap candidates.  Periodic retries (also
    ///   `bootstrap_retry_interval`) only re-attempt the configured
    ///   URL — never fall back to peers.json.
    pub floating_bootstrap:        bool,
    /// Cadence for re-attempting bootstrap when no Alive peers exist.
    pub bootstrap_retry_interval_secs: u64,
    /// How recently a stored script must have been executed by **any**
    /// node for the cluster-aware Scheduler to suppress this node's
    /// fire of the same script.  Standalone nodes ignore this knob.
    /// Default: 5 minutes.  Set generously: it should comfortably
    /// exceed the largest plausible inter-node clock skew + tick jitter.
    pub scheduler_dedup_window_secs: u64,
    /// TTL of the session cookie issued by `v3/user.authenticate` and
    /// the bdsweb /login form.  Default: 8h.  Tokens are stateless
    /// HMAC-signed (`cluster::session`) so no per-node state grows
    /// with the TTL.
    pub session_ttl_secs:        u64,
    /// Per-IP rate limit for the login path
    /// (`POST /login` + `v3/user.authenticate`).  Default: 10/minute,
    /// burst 3.  Set to 0 to disable rate limiting entirely
    /// (NOT recommended on internet-facing deployments).
    pub auth_rate_limit_per_minute: u32,
    /// When `true` (default), `cluster::fanout::fan_out_v2` adapts the
    /// per-peer RPC deadline using the observed p95 from the
    /// `fanout.peer.<id>` perf series.  The dynamic deadline is
    /// `min(peer_rpc_timeout, p95 × multiplier).max(peer_rpc_timeout / 10)`
    /// — never exceeds the configured timeout, never goes below 10% of
    /// it.  Falls back to the static timeout when the peer has fewer
    /// than 20 recent samples.  Self-stabilising: timeouts get
    /// recorded into p95 too, so a chronically broken peer's p95
    /// converges to the static timeout and the heuristic stops biting.
    pub adaptive_peer_timeout_enabled: bool,
    /// Multiplier applied to the per-peer p95 when computing the
    /// dynamic deadline.  Default `3.0` — tight enough to bail fast
    /// on stragglers, loose enough to survive normal jitter.
    pub adaptive_peer_timeout_multiplier: f64,
}

impl ClusterConfig {
    /// Construct a disabled config.  Used when `cluster` is absent from the
    /// hjson file or `cluster.enabled = false`.
    pub fn disabled() -> Self {
        Self {
            enabled:                  false,
            shared_secret:            String::new(),
            bootstrap:                None,
            bind_url:                 String::new(),
            gossip_interval_secs:     5,
            suspect_timeout_secs:     30,
            dead_timeout_secs:        120,
            full_mode_threshold:      3,
            replication_factor:       3,
            full_replication_stores:  vec!["docs".into(), "signals".into(), "scripts".into(),
                                          "users".into(), "llm_cache".into()],
            antientropy_interval_secs: 300,
            hint_replay_interval_secs: 10,
            hint_max_age_secs:        86_400,
            peer_rpc_timeout_secs:    2,
            max_fingerprints_per_peer: 100_000,
            floating_bootstrap:       true,
            bootstrap_retry_interval_secs: 60,
            scheduler_dedup_window_secs: 300,
            session_ttl_secs:           8 * 3600,
            auth_rate_limit_per_minute: 10,
            adaptive_peer_timeout_enabled: true,
            adaptive_peer_timeout_multiplier: 3.0,
        }
    }

    /// Parse the `cluster` sub-object from a raw hjson string.  Returns a
    /// disabled config when the key is missing or `enabled` is false; returns
    /// `Err` when the block is present-and-enabled but missing required fields
    /// (`shared_secret`, `bind_url`).
    pub fn from_hjson_str(raw: &str) -> Result<Self> {
        let val: serde_hjson::Value = serde_hjson::from_str(raw)
            .map_err(|e| err_msg(format!("hjson parse error: {e}")))?;
        let obj = match val.as_object() {
            Some(o) => o,
            None => return Ok(Self::disabled()),
        };
        let block = match obj.get("cluster").and_then(|v| v.as_object()) {
            Some(b) => b,
            None => return Ok(Self::disabled()),
        };

        let enabled = block.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        if !enabled {
            return Ok(Self::disabled());
        }

        let shared_secret = block.get("shared_secret")
            .and_then(|v| v.as_str())
            .ok_or_else(|| err_msg("cluster.shared_secret is required when cluster.enabled = true"))?
            .to_string();
        if shared_secret.len() < 16 {
            return Err(err_msg(
                "cluster.shared_secret must be at least 16 characters",
            ));
        }

        let bind_url = block.get("bind_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| err_msg("cluster.bind_url is required when cluster.enabled = true"))?
            .to_string();

        let bootstrap = block.get("bootstrap")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        let parse_dur = |key: &str, default: u64| -> Result<u64> {
            match block.get(key).and_then(|v| v.as_str()) {
                Some(s) => humantime::parse_duration(s)
                    .map(|d| d.as_secs())
                    .map_err(|e| err_msg(format!("cluster.{key} ({s:?}): {e}"))),
                None => Ok(default),
            }
        };
        let parse_usize = |key: &str, default: usize| -> usize {
            block.get(key).and_then(|v| v.as_f64()).map(|f| f as usize).unwrap_or(default)
        };

        let full_replication_stores = block.get("full_replication_stores")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_else(|| vec!["docs".into(), "signals".into(), "scripts".into(), "users".into()]);

        let floating_bootstrap = block.get("floating_bootstrap")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let bootstrap_retry_interval_secs = parse_dur("bootstrap_retry_interval", 60)?;

        // Strict mode requires a configured bootstrap target — otherwise the
        // node has nothing to try, ever (and persisted peers are excluded
        // from the candidate set in strict mode).
        if !floating_bootstrap && bootstrap.is_none() {
            return Err(err_msg(
                "cluster.bootstrap is required when cluster.floating_bootstrap = false \
                 (strict mode has no fallback candidates)",
            ));
        }

        Ok(Self {
            enabled: true,
            shared_secret,
            bootstrap,
            bind_url,
            gossip_interval_secs:      parse_dur("gossip_interval",     5)?,
            suspect_timeout_secs:      parse_dur("suspect_timeout",     30)?,
            dead_timeout_secs:         parse_dur("dead_timeout",        120)?,
            full_mode_threshold:       parse_usize("full_mode_threshold", 3),
            replication_factor:        parse_usize("replication_factor",  3),
            full_replication_stores,
            antientropy_interval_secs: parse_dur("antientropy_interval", 300)?,
            hint_replay_interval_secs: parse_dur("hint_replay_interval", 10)?,
            hint_max_age_secs:         parse_dur("hint_max_age",         86_400)?,
            peer_rpc_timeout_secs:     parse_dur("peer_rpc_timeout",     2)?,
            max_fingerprints_per_peer: parse_usize("max_fingerprints_per_peer", 100_000),
            floating_bootstrap,
            bootstrap_retry_interval_secs,
            scheduler_dedup_window_secs: parse_dur("scheduler_dedup_window", 300)?,
            session_ttl_secs:           parse_dur("session_ttl", 8 * 3600)?,
            auth_rate_limit_per_minute: block.get("auth_rate_limit_per_minute")
                .and_then(|v| v.as_f64())
                .map(|n| n as u32)
                .unwrap_or(10),
            adaptive_peer_timeout_enabled: block.get("adaptive_peer_timeout_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            adaptive_peer_timeout_multiplier: block.get("adaptive_peer_timeout_multiplier")
                .and_then(|v| v.as_f64())
                .filter(|n| n.is_finite() && *n > 0.0)
                .unwrap_or(3.0),
        })
    }

    pub fn from_path(config_path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(config_path)
            .map_err(|e| err_msg(format!("cannot read config '{config_path}': {e}")))?;
        Self::from_hjson_str(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_block_missing() {
        let cfg = ClusterConfig::from_hjson_str(r#"{ "dbpath": "/tmp/x" }"#).unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn disabled_when_enabled_false() {
        let cfg = ClusterConfig::from_hjson_str(
            r#"{ "cluster": { "enabled": false } }"#
        ).unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn requires_secret_and_bind_when_enabled() {
        assert!(ClusterConfig::from_hjson_str(
            r#"{ "cluster": { "enabled": true } }"#
        ).is_err());
        assert!(ClusterConfig::from_hjson_str(
            r#"{ "cluster": { "enabled": true, "shared_secret": "short" } }"#
        ).is_err());
        assert!(ClusterConfig::from_hjson_str(
            r#"{ "cluster": { "enabled": true, "shared_secret": "thisisalongenoughsecret" } }"#
        ).is_err());
    }

    #[test]
    fn strict_mode_requires_bootstrap() {
        // floating_bootstrap=false + bootstrap=None → error
        let raw = r#"{
            "cluster": {
                "enabled":            true,
                "shared_secret":      "thisisalongenoughsecret-32",
                "bind_url":           "http://127.0.0.1:9711",
                "floating_bootstrap": false
            }
        }"#;
        let err = ClusterConfig::from_hjson_str(raw).unwrap_err().to_string();
        assert!(err.contains("strict"), "got: {err}");

        // floating_bootstrap=false + bootstrap=Some(…) → ok
        let raw = r#"{
            "cluster": {
                "enabled":            true,
                "shared_secret":      "thisisalongenoughsecret-32",
                "bind_url":           "http://127.0.0.1:9711",
                "bootstrap":          "http://10.0.0.5:9000",
                "floating_bootstrap": false
            }
        }"#;
        let cfg = ClusterConfig::from_hjson_str(raw).unwrap();
        assert!(!cfg.floating_bootstrap);

        // floating_bootstrap=true + bootstrap=None → ok (default behaviour)
        let raw = r#"{
            "cluster": {
                "enabled":            true,
                "shared_secret":      "thisisalongenoughsecret-32",
                "bind_url":           "http://127.0.0.1:9711"
            }
        }"#;
        let cfg = ClusterConfig::from_hjson_str(raw).unwrap();
        assert!(cfg.floating_bootstrap);   // default
        assert_eq!(cfg.bootstrap_retry_interval_secs, 60);
    }

    #[test]
    fn parses_full_block() {
        let raw = r#"{
            "cluster": {
                "enabled":               true,
                "shared_secret":         "thisisalongenoughsecret-32",
                "bootstrap":             "http://10.0.0.5:9000",
                "bind_url":              "http://10.0.0.7:9000",
                "gossip_interval":       "10s",
                "replication_factor":    5
            }
        }"#;
        let cfg = ClusterConfig::from_hjson_str(raw).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.bootstrap.as_deref(), Some("http://10.0.0.5:9000"));
        assert_eq!(cfg.bind_url, "http://10.0.0.7:9000");
        assert_eq!(cfg.gossip_interval_secs, 10);
        assert_eq!(cfg.replication_factor, 5);
        assert_eq!(cfg.full_mode_threshold, 3);  // default
    }
}
