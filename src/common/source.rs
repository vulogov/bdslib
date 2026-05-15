//! Source / origin resolution for incoming records.
//!
//! Every record stored through `ShardsManager::add*` carries a
//! `source` tag in its `metadata` field.  `source` is the canonical
//! "where did this come from" axis — a host name, container id,
//! application name, or logical channel — that the signal layer,
//! graph layer, and LLM analyze surfaces all join on.
//!
//! This module owns the resolution chain that picks the right
//! `source` value for each ingested record:
//!
//! 1. Explicit API parameter (`add_with_source(doc, Some("foo"))`).
//! 2. Top-level field in the doc, walked in `cfg.source_keys` order.
//! 3. Same keys, walked inside `doc.data.*`.  Lets the syslog parser
//!    (which writes `host` into `data.host`) and JSON ingesters that
//!    nest their metadata inside `data` participate without a
//!    special-case branch.
//! 4. Fall through to `cfg.default_source` (`"global"` by default).
//!
//! Resolved values are validated: empty becomes the default,
//! oversize strings are truncated to `cfg.source_max_length` bytes
//! with a `log::debug!` line.  Validation is identical regardless of
//! which step in the chain produced the value — operators can't
//! sneak a 4 KB source string in through the `host` field.
//!
//! Storage: the resolved value is injected into the doc at the top
//! level as `doc["source"] = …`; `ObservabilityStorage::build_metadata`
//! folds every non-canonical top-level field into the `metadata`
//! JSON column, so the stored location ends up being `metadata.source`.
//! That's the canonical query / LLM-prompt-anchor path.
//!
//! Process-wide tuning is held in [`SourceConfig`], installed by
//! `bdsnode` at startup from the `data:` block of `bds.hjson` via
//! [`configure`].  Library callers that never invoke `configure` get
//! the documented defaults — same backwards-compat shape as the dev
//! data / retention / rebalancer configs.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use serde_json::Value as JsonValue;

/// Process-wide tuning for source resolution.  Initialise once at
/// startup from `bds.hjson` via [`configure`]; later reads via
/// [`get`].
#[derive(Debug, Clone)]
pub struct SourceConfig {
    /// Returned when no other resolution step yields a value.
    pub default_source: String,
    /// Walked in order at every resolution step.  Default
    /// `["source", "origin", "host"]`.
    pub source_keys: Vec<String>,
    /// Hard cap on the stored value's byte length.  Longer values are
    /// truncated to a UTF-8-safe boundary and a `log::debug!` line is
    /// emitted.
    pub source_max_length: usize,
    /// When `true`, the data path inserts a `Source:<name>` graph node
    /// the first time it sees a new value (idempotent via the
    /// deterministic UUIDv5 in `GraphStore`).  Set to `false` for
    /// deployments that manage their graph manually.
    pub auto_create_source_graph_node: bool,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            default_source: "global".to_owned(),
            source_keys: vec!["source".to_owned(), "origin".to_owned(), "host".to_owned()],
            source_max_length: 256,
            auto_create_source_graph_node: true,
        }
    }
}

static CONFIG: OnceLock<SourceConfig> = OnceLock::new();

/// Install a `SourceConfig`.  Subsequent calls are silently ignored —
/// the process-wide config is fixed once observed.  bdsnode calls
/// this exactly once at startup from the `data:` hjson block.
pub fn configure(cfg: SourceConfig) {
    let _ = CONFIG.set(cfg);
}

/// Borrow the active config.  Returns the default config when
/// [`configure`] has never been called (library callers without a
/// bdsnode shell).
pub fn get() -> &'static SourceConfig {
    CONFIG.get_or_init(SourceConfig::default)
}

// ─────────────────────────────────────────────────────────────────────────────
// Resolution
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve the canonical `source` for an ingested doc.  See the
/// module-level doc for the priority chain.
///
/// `explicit` is the value the caller passed (typically from an
/// `add_with_source` API parameter or a `--source` CLI flag); `None`
/// means "no explicit override, look at the doc".
pub fn resolve(explicit: Option<&str>, doc: &JsonValue, cfg: &SourceConfig) -> String {
    // 1. Explicit param wins outright.
    if let Some(s) = explicit {
        return validate(s, cfg);
    }
    // 2. Walk source_keys in priority order, checking top-level first
    //    and then data.* for each.  Source-of-any-flavour beats
    //    origin-of-any-flavour beats host-of-any-flavour; within one
    //    key, top-level beats data.*.
    for key in &cfg.source_keys {
        if let Some(s) = doc.get(key).and_then(|v| v.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return validate(t, cfg);
            }
        }
        if let Some(s) = doc.get("data").and_then(|d| d.get(key)).and_then(|v| v.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return validate(t, cfg);
            }
        }
    }
    // 3. Default.
    cfg.default_source.clone()
}

/// Validate and normalise a raw source string.  Trims, truncates to
/// `cfg.source_max_length` on a UTF-8 boundary, falls back to the
/// default for empty / whitespace-only input.
pub fn validate(s: &str, cfg: &SourceConfig) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return cfg.default_source.clone();
    }
    if trimmed.len() <= cfg.source_max_length {
        return trimmed.to_owned();
    }
    // Truncate on a UTF-8 char boundary so we never split a
    // multi-byte sequence.  `char_indices` walks O(n); the loop runs
    // at most `source_max_length / 1` iterations, which is fine on
    // any realistic input.
    let mut end = cfg.source_max_length;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    log::debug!(
        "[source] truncated to {end} bytes (was {} bytes)",
        trimmed.len(),
    );
    let out = &trimmed[..end];
    // Sub-pathological case: the very first character is wider than
    // the entire budget (e.g. a 4-byte emoji vs `source_max_length=3`).
    // Truncation backs off to byte 0; we fall back to the default so
    // we never return an empty source.
    if out.is_empty() {
        return cfg.default_source.clone();
    }
    out.to_owned()
}

/// Inject the resolved source as a top-level `"source"` field on the
/// doc, replacing any existing top-level value with the canonical
/// resolution.  After this call, `ObservabilityStorage::build_metadata`
/// folds the field into `metadata.source` automatically.
pub fn inject(doc: &mut JsonValue, source: &str) {
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("source".to_owned(), JsonValue::String(source.to_owned()));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// First-observation memoization for the `Source:<name>` graph node
// auto-creation path.  Idempotent in the graph layer (deterministic
// UUIDv5), but pointless to re-issue per-record once we know we've
// already created the node for this source on this process — this
// memoization avoids tens of thousands of redundant graph writes
// under heavy ingest of a small source set.
// ─────────────────────────────────────────────────────────────────────────────

static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn seen_set() -> &'static Mutex<HashSet<String>> {
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Mark `name` as observed.  Returns `true` the FIRST time this
/// process sees the value, `false` thereafter.  The caller uses the
/// `true` return as the signal to create the `Source:<name>` graph
/// node; subsequent records for the same source skip the graph
/// write entirely.
///
/// Concurrent callers race on the underlying mutex; exactly one
/// caller for any given `name` observes `true`.  A poisoned mutex
/// fails open (returns `true`) — the graph layer's deterministic
/// node id makes the resulting redundant write a no-op.
pub fn try_mark_seen(name: &str) -> bool {
    let mut g = match seen_set().lock() {
        Ok(g)  => g,
        Err(_) => return true,
    };
    g.insert(name.to_owned())
}

/// Test helper — clear the seen set between unit tests.
#[cfg(test)]
pub fn clear_seen() {
    if let Ok(mut g) = seen_set().lock() {
        g.clear();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> SourceConfig { SourceConfig::default() }

    #[test]
    fn explicit_param_wins() {
        let doc = json!({"source": "manual-tag", "data": {"host": "worker-01"}});
        assert_eq!(resolve(Some("explicit"), &doc, &cfg()), "explicit");
    }

    #[test]
    fn top_level_source_wins_over_data_origin() {
        let doc = json!({"source": "A", "data": {"origin": "B"}});
        assert_eq!(resolve(None, &doc, &cfg()), "A");
    }

    #[test]
    fn top_level_source_beats_top_level_host() {
        let doc = json!({"host": "worker-01", "source": "api"});
        assert_eq!(resolve(None, &doc, &cfg()), "api");
    }

    #[test]
    fn data_source_beats_top_level_host() {
        let doc = json!({"host": "worker-01", "data": {"source": "api"}});
        assert_eq!(resolve(None, &doc, &cfg()), "api");
    }

    #[test]
    fn data_host_picked_up_for_syslog() {
        // Syslog parser writes host into data.host — no top-level
        // source/origin/host on the doc.
        let doc = json!({
            "timestamp": 100, "key": "sshd",
            "data": {"message": "ok", "host": "worker-01", "pid": "1234"},
        });
        assert_eq!(resolve(None, &doc, &cfg()), "worker-01");
    }

    #[test]
    fn falls_back_to_default_when_nothing_present() {
        let doc = json!({"timestamp": 100, "key": "k", "data": {"v": 1}});
        assert_eq!(resolve(None, &doc, &cfg()), "global");
    }

    #[test]
    fn empty_or_whitespace_treated_as_absent() {
        let doc = json!({"source": "   ", "origin": "", "host": "worker-01"});
        // Walk skips the blank "source" and empty "origin", picks "host".
        assert_eq!(resolve(None, &doc, &cfg()), "worker-01");
    }

    #[test]
    fn explicit_empty_falls_back_to_default() {
        let doc = json!({"source": "fallback"});
        assert_eq!(resolve(Some("   "), &doc, &cfg()), "global");
    }

    #[test]
    fn priority_order_origin_beats_host() {
        let doc = json!({"origin": "ingest-pipe", "host": "worker-01"});
        assert_eq!(resolve(None, &doc, &cfg()), "ingest-pipe");
    }

    #[test]
    fn truncates_to_max_length() {
        let mut c = SourceConfig::default();
        c.source_max_length = 8;
        let long = "abcdefghijklmnopqrstuvwxyz";
        let doc = json!({"source": long});
        let r = resolve(None, &doc, &c);
        assert_eq!(r.len(), 8);
        assert_eq!(r, "abcdefgh");
    }

    #[test]
    fn truncation_respects_utf8_boundary() {
        let mut c = SourceConfig::default();
        // "🌍" is 4 UTF-8 bytes; cap at 3 forces truncation INSIDE the
        // multi-byte sequence — we should back off to byte 0.
        c.source_max_length = 3;
        let doc = json!({"source": "🌍"});
        // The single emoji is 4 bytes; truncation to 3 backs off to 0
        // ⇒ empty ⇒ default.
        assert_eq!(resolve(None, &doc, &c), "global");
    }

    #[test]
    fn explicit_oversize_truncates() {
        let mut c = SourceConfig::default();
        c.source_max_length = 5;
        let doc = json!({});
        assert_eq!(resolve(Some("longstring"), &doc, &c), "longs");
    }

    #[test]
    fn custom_source_keys_order() {
        let mut c = SourceConfig::default();
        c.source_keys = vec!["app".into(), "host".into()];
        let doc = json!({"app": "billing", "host": "node-1"});
        assert_eq!(resolve(None, &doc, &c), "billing");
        let doc2 = json!({"host": "node-1"});
        assert_eq!(resolve(None, &doc2, &c), "node-1");
    }

    #[test]
    fn inject_sets_top_level_source() {
        let mut doc = json!({"timestamp": 100, "key": "k", "data": {"v": 1}});
        inject(&mut doc, "api-01");
        assert_eq!(doc["source"].as_str(), Some("api-01"));
    }

    #[test]
    fn inject_overwrites_existing_top_level() {
        let mut doc = json!({"source": "manual", "data": {}});
        inject(&mut doc, "resolved");
        assert_eq!(doc["source"].as_str(), Some("resolved"));
    }

    #[test]
    fn inject_is_noop_on_non_object_doc() {
        let mut doc = json!("scalar");
        inject(&mut doc, "x");
        assert_eq!(doc, json!("scalar"));
    }

    #[test]
    fn default_config_matches_proposal() {
        let c = SourceConfig::default();
        assert_eq!(c.default_source, "global");
        assert_eq!(c.source_keys, vec!["source", "origin", "host"]);
        assert_eq!(c.source_max_length, 256);
        assert!(c.auto_create_source_graph_node);
    }

    // The seen-set is process-wide, so these tests share state with
    // any other tests that touched it.  Each test clears first so the
    // "first observation returns true" check is reliable.

    #[test]
    fn try_mark_seen_first_call_returns_true_then_false() {
        clear_seen();
        let name = "first_call_test_source";
        assert!(try_mark_seen(name),  "first call should be true");
        assert!(!try_mark_seen(name), "second call should be false");
        assert!(!try_mark_seen(name), "subsequent calls stay false");
    }

    #[test]
    fn try_mark_seen_independent_names() {
        clear_seen();
        assert!(try_mark_seen("alpha"));
        assert!(try_mark_seen("beta"));
        assert!(!try_mark_seen("alpha"));
        assert!(try_mark_seen("gamma"));
    }
}
