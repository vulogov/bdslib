//! Per-node log of in-flight + recent LLM inferences, used to dedup
//! identical work across the cluster.
//!
//! When a coordinator decides to fire an inference (cache miss + no
//! peer-side running entry), it records a `running` row here.  Other
//! coordinators querying `v2/llm.last_executed` for the same
//! `cache_key` see the `running` state and either wait for the
//! replicated cache entry to land or fall through (operator-tunable).
//!
//! Storage: a single DuckDB file at `<dbpath>/network/inference_log.duckdb`.
//! Not replicated — each node tracks its own work.  Cross-node
//! visibility is via the v2 RPC fan-out, mirroring how
//! `cluster::scheduler_log` + `v2/scheduler.last_seen` work today.
//!
//! Same correctness story as `scheduler_log`: the race window between
//! "I queried peers and saw nothing" and "I recorded my own running"
//! is sub-second per tick.  The accepted tradeoff is that two nodes
//! racing in that window CAN both fire — the inference cache (phase 3)
//! prevents the second one from re-doing work once the first replicates
//! its result.

use crate::common::error::{err_msg, Result};
use crate::storageengine::StorageEngine;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS inference_log (
    cache_key    TEXT   NOT NULL,
    started_at   BIGINT NOT NULL,
    finished_at  BIGINT,
    node_id      TEXT   NOT NULL,
    state        TEXT   NOT NULL
);
CREATE INDEX IF NOT EXISTS inference_log_key_idx     ON inference_log(cache_key);
CREATE INDEX IF NOT EXISTS inference_log_state_idx   ON inference_log(state);
CREATE INDEX IF NOT EXISTS inference_log_started_idx ON inference_log(started_at);
"#;

/// One row of the log.  `state` is one of `"running"`, `"done"`,
/// `"failed"`.  `finished_at` is non-null for terminal states only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceRow {
    pub cache_key:   String,
    pub started_at:  u64,
    pub finished_at: Option<u64>,
    pub node_id:     Uuid,
    pub state:       InferenceState,
}

impl InferenceRow {
    /// True when the row is in a terminal state AND its `started_at`
    /// is within `window_secs` of `now`.  Callers use this to decide
    /// whether a recently-completed inference still counts as "fresh"
    /// (the cache should already have the answer).
    pub fn is_recent(&self, window_secs: u64) -> bool {
        if window_secs == 0 { return false; }
        let now = now_secs();
        now.saturating_sub(self.started_at) <= window_secs
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceState {
    Running,
    Done,
    Failed,
}

impl InferenceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            InferenceState::Running => "running",
            InferenceState::Done    => "done",
            InferenceState::Failed  => "failed",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "done"    => Some(Self::Done),
            "failed"  => Some(Self::Failed),
            _         => None,
        }
    }
}

#[derive(Clone)]
pub struct InferenceLog {
    engine: Arc<StorageEngine>,
}

impl InferenceLog {
    /// Open or create the inference log at `<network_dir>/inference_log.duckdb`.
    /// `network_dir` is typically the one returned by
    /// `cluster::persistence::ensure_network_dir`.
    pub fn open(network_dir: &Path) -> Result<Self> {
        let path = network_dir.join("inference_log.duckdb");
        let engine = StorageEngine::new(&path, INIT_SQL, 4)?;
        Ok(Self { engine: Arc::new(engine) })
    }

    /// Record the start of an inference attempt.  Always inserts a new
    /// row — callers acquire a lease (see `vm::api::llm`) and that lease
    /// terminates by calling [`record_finished`] / [`record_failed`].
    pub fn record_start(&self, cache_key: &str, node_id: Uuid, now: u64) -> Result<()> {
        let sql = format!(
            "INSERT INTO inference_log (cache_key, started_at, finished_at, node_id, state) \
             VALUES ('{}', {}, NULL, '{}', 'running')",
            sql_escape(cache_key), now as i64, node_id,
        );
        self.engine.execute(&sql)
    }

    /// Flip the `running` row owned by this node to `done` (or
    /// `failed`) and stamp `finished_at`.  Updates the most-recent
    /// running row for `cache_key` + `node_id` — handles the (rare)
    /// case where the same node started two attempts back-to-back.
    pub fn record_finished(&self, cache_key: &str, node_id: Uuid, finished_at: u64) -> Result<()> {
        self.update_state(cache_key, node_id, InferenceState::Done, finished_at)
    }

    pub fn record_failed(&self, cache_key: &str, node_id: Uuid, finished_at: u64) -> Result<()> {
        self.update_state(cache_key, node_id, InferenceState::Failed, finished_at)
    }

    fn update_state(
        &self,
        cache_key:   &str,
        node_id:     Uuid,
        new_state:   InferenceState,
        finished_at: u64,
    ) -> Result<()> {
        // DuckDB doesn't support UPDATE … LIMIT directly in every
        // build; the (cache_key, node_id, state='running') triple is
        // unique enough in practice that updating every matching row
        // is fine.  If a node truly started two parallel attempts for
        // the same key, both transition together.
        let sql = format!(
            "UPDATE inference_log \
             SET state = '{}', finished_at = {} \
             WHERE cache_key = '{}' AND node_id = '{}' AND state = 'running'",
            new_state.as_str(), finished_at as i64,
            sql_escape(cache_key), node_id,
        );
        self.engine.execute(&sql)
    }

    /// Most-recent row for `cache_key` (any state), or `None` when
    /// this node has no record of it.
    pub fn most_recent(&self, cache_key: &str) -> Result<Option<InferenceRow>> {
        let sql = format!(
            "SELECT cache_key, started_at, finished_at, node_id, state \
             FROM inference_log \
             WHERE cache_key = '{}' \
             ORDER BY started_at DESC \
             LIMIT 1",
            sql_escape(cache_key),
        );
        let mut rows = self.engine.select_all(&sql)?;
        match rows.pop() {
            Some(r) => Ok(Some(row_to_inference_row(r)?)),
            None    => Ok(None),
        }
    }

    /// Most-recent row for `cache_key` whose `started_at` is within
    /// `window_secs`.  Returns `None` when the only rows are older
    /// than the window — useful for "did anyone run this recently?"
    /// without dragging year-old completions into the dedup decision.
    pub fn recent_within(&self, cache_key: &str, window_secs: u64) -> Result<Option<InferenceRow>> {
        if window_secs == 0 {
            return Ok(None);
        }
        let cutoff = now_secs().saturating_sub(window_secs) as i64;
        let sql = format!(
            "SELECT cache_key, started_at, finished_at, node_id, state \
             FROM inference_log \
             WHERE cache_key = '{}' AND started_at >= {} \
             ORDER BY started_at DESC \
             LIMIT 1",
            sql_escape(cache_key), cutoff,
        );
        let mut rows = self.engine.select_all(&sql)?;
        match rows.pop() {
            Some(r) => Ok(Some(row_to_inference_row(r)?)),
            None    => Ok(None),
        }
    }

    /// Drop rows older than `cutoff_secs` seconds.  Idempotent and
    /// cheap to call on every tick.
    pub fn prune_older_than(&self, cutoff_secs: u64) -> Result<()> {
        let cutoff = now_secs().saturating_sub(cutoff_secs) as i64;
        let sql = format!("DELETE FROM inference_log WHERE started_at < {cutoff}");
        self.engine.execute(&sql)
    }

    /// Total row count — diagnostic only.
    pub fn len(&self) -> Result<usize> {
        let rows = self.engine.select_all("SELECT COUNT(*) FROM inference_log")?;
        Ok(rows.into_iter().next()
            .and_then(|r| r.into_iter().next())
            .and_then(|v| v.cast_int().ok())
            .map(|i| i.max(0) as usize)
            .unwrap_or(0))
    }
}

fn row_to_inference_row(row: Vec<rust_dynamic::value::Value>) -> Result<InferenceRow> {
    let mut it = row.into_iter();
    let cache_key  = it.next().ok_or_else(|| err_msg("missing cache_key"))?
                       .cast_string().map_err(|e| err_msg(format!("cache_key cast: {e}")))?;
    let started_at = it.next().ok_or_else(|| err_msg("missing started_at"))?
                       .cast_int().map_err(|e| err_msg(format!("started_at cast: {e}")))?;
    let fin_v = it.next().ok_or_else(|| err_msg("missing finished_at"))?;
    let finished_at = if matches!(fin_v.data, rust_dynamic::types::Val::Null) {
        None
    } else {
        let n = fin_v.cast_int().map_err(|e| err_msg(format!("finished_at cast: {e}")))?;
        Some(n.max(0) as u64)
    };
    let node_id_s  = it.next().ok_or_else(|| err_msg("missing node_id"))?
                       .cast_string().map_err(|e| err_msg(format!("node_id cast: {e}")))?;
    let node_id    = Uuid::parse_str(&node_id_s)
                       .map_err(|e| err_msg(format!("node_id parse: {e}")))?;
    let state_s    = it.next().ok_or_else(|| err_msg("missing state"))?
                       .cast_string().map_err(|e| err_msg(format!("state cast: {e}")))?;
    let state      = InferenceState::from_wire(&state_s)
                       .ok_or_else(|| err_msg(format!("unknown state {state_s:?}")))?;
    Ok(InferenceRow {
        cache_key,
        started_at: started_at.max(0) as u64,
        finished_at,
        node_id,
        state,
    })
}

fn sql_escape(s: &str) -> String { s.replace('\'', "''") }

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_log() -> (TempDir, InferenceLog) {
        let tmp = TempDir::new().unwrap();
        let log = InferenceLog::open(tmp.path()).unwrap();
        (tmp, log)
    }

    #[test]
    fn empty_log_returns_none() {
        let (_tmp, log) = open_log();
        assert!(log.most_recent("k").unwrap().is_none());
        assert!(log.recent_within("k", 60).unwrap().is_none());
        assert_eq!(log.len().unwrap(), 0);
    }

    #[test]
    fn record_start_inserts_running_row() {
        let (_tmp, log) = open_log();
        let node = Uuid::now_v7();
        log.record_start("abc", node, 100).unwrap();
        let row = log.most_recent("abc").unwrap().expect("row");
        assert_eq!(row.cache_key, "abc");
        assert_eq!(row.state, InferenceState::Running);
        assert_eq!(row.started_at, 100);
        assert_eq!(row.finished_at, None);
        assert_eq!(row.node_id, node);
    }

    #[test]
    fn record_finished_flips_running_to_done() {
        let (_tmp, log) = open_log();
        let node = Uuid::now_v7();
        log.record_start("k", node, 100).unwrap();
        log.record_finished("k", node, 110).unwrap();
        let row = log.most_recent("k").unwrap().expect("row");
        assert_eq!(row.state, InferenceState::Done);
        assert_eq!(row.finished_at, Some(110));
    }

    #[test]
    fn record_failed_flips_running_to_failed() {
        let (_tmp, log) = open_log();
        let node = Uuid::now_v7();
        log.record_start("k", node, 100).unwrap();
        log.record_failed("k", node, 105).unwrap();
        let row = log.most_recent("k").unwrap().unwrap();
        assert_eq!(row.state, InferenceState::Failed);
    }

    #[test]
    fn record_finished_with_no_running_row_is_noop() {
        let (_tmp, log) = open_log();
        let node = Uuid::now_v7();
        // Never started — finished update affects 0 rows; no error.
        log.record_finished("k", node, 100).unwrap();
        assert!(log.most_recent("k").unwrap().is_none());
    }

    #[test]
    fn most_recent_returns_latest_by_started_at() {
        let (_tmp, log) = open_log();
        let n1 = Uuid::now_v7();
        let n2 = Uuid::now_v7();
        log.record_start("k", n1, 100).unwrap();
        log.record_start("k", n2, 200).unwrap();
        let row = log.most_recent("k").unwrap().unwrap();
        assert_eq!(row.started_at, 200);
        assert_eq!(row.node_id, n2);
    }

    #[test]
    fn recent_within_window_filters_old_rows() {
        let (_tmp, log) = open_log();
        let n = Uuid::now_v7();
        let now = now_secs();
        // Both rows have the same key; one is fresh, one is ancient.
        log.record_start("k", n, now.saturating_sub(7200)).unwrap();
        log.record_start("k", n, now.saturating_sub(5)).unwrap();
        let row = log.recent_within("k", 60).unwrap().unwrap();
        assert!(row.started_at >= now.saturating_sub(60));
    }

    #[test]
    fn recent_within_returns_none_when_only_old_rows() {
        let (_tmp, log) = open_log();
        let n = Uuid::now_v7();
        log.record_start("k", n, now_secs().saturating_sub(3600)).unwrap();
        assert!(log.recent_within("k", 60).unwrap().is_none());
    }

    #[test]
    fn recent_within_zero_window_returns_none() {
        let (_tmp, log) = open_log();
        let n = Uuid::now_v7();
        log.record_start("k", n, now_secs()).unwrap();
        assert!(log.recent_within("k", 0).unwrap().is_none(),
            "zero window means 'disabled' — never match");
    }

    #[test]
    fn prune_drops_old_rows() {
        let (_tmp, log) = open_log();
        let n = Uuid::now_v7();
        log.record_start("a", n, now_secs().saturating_sub(3600)).unwrap();
        log.record_start("b", n, now_secs()).unwrap();
        log.prune_older_than(60).unwrap();
        assert!(log.most_recent("a").unwrap().is_none());
        assert!(log.most_recent("b").unwrap().is_some());
    }

    #[test]
    fn is_recent_window_check() {
        let row = InferenceRow {
            cache_key:   "x".into(),
            started_at:  now_secs(),
            finished_at: None,
            node_id:     Uuid::now_v7(),
            state:       InferenceState::Done,
        };
        assert!( row.is_recent(60));
        assert!(!row.is_recent(0), "window 0 always returns false");
    }
}
