//! Per-node log of scheduled-script executions.
//!
//! When a node decides to fire a stored script via the cron Scheduler,
//! it records the (script_id, executed_at, node_id) triple here.  The
//! cluster-aware Scheduler tick reads its own log and queries every
//! Alive peer's log via the `v2/scheduler.last_seen` JSON-RPC method
//! before firing a script, suppressing the fire if any node executed
//! the same script within the configured `scheduler_dedup_window`.
//!
//! Storage: a single DuckDB file at `<dbpath>/network/scheduler_log.duckdb`.
//! One row per execution.  Old rows are pruned on every read so the file
//! stays bounded; the prune horizon is twice the dedup window so peers
//! that came back online recently can still see the relevant history.
//!
//! The race window between "this node checked + saw nothing" and "this
//! node recorded its execution" is sub-second per tick, while the
//! configured dedup window is typically minutes.  Two nodes ticking at
//! the exact same instant CAN both fire — that's an explicitly accepted
//! tradeoff for not requiring a distributed lock.

use crate::common::error::Result;
use crate::storageengine::StorageEngine;
use rust_dynamic::value::Value as DynVal;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS scheduler_log (
    script_id    TEXT   NOT NULL,
    executed_at  BIGINT NOT NULL,
    node_id      TEXT   NOT NULL
);
CREATE INDEX IF NOT EXISTS scheduler_log_script_idx     ON scheduler_log(script_id);
CREATE INDEX IF NOT EXISTS scheduler_log_executed_idx   ON scheduler_log(executed_at);
"#;

#[derive(Clone)]
pub struct SchedulerLog {
    engine: Arc<StorageEngine>,
}

impl SchedulerLog {
    /// Open or create the scheduler-log DuckDB file.  `network_dir` is
    /// typically `<dbpath>/network/` (created by
    /// `persistence::ensure_network_dir`).
    pub fn open(network_dir: &Path) -> Result<Self> {
        let path = network_dir.join("scheduler_log.duckdb");
        let engine = StorageEngine::new(&path, INIT_SQL, 4)?;
        Ok(Self { engine: Arc::new(engine) })
    }

    /// Record an execution of `script_id` by `node_id` at `executed_at`
    /// (Unix seconds).  The Scheduler calls this immediately before
    /// dispatching the script to the worker pool.
    pub fn record(&self, script_id: Uuid, node_id: Uuid, executed_at: u64) -> Result<()> {
        let sql = format!(
            "INSERT INTO scheduler_log (script_id, executed_at, node_id) \
             VALUES ('{}', {}, '{}')",
            script_id, executed_at as i64, node_id,
        );
        self.engine.execute(&sql)
    }

    /// Most recent local execution timestamp (Unix seconds) for
    /// `script_id`, or `None` if this node has never run it.
    pub fn last_executed(&self, script_id: Uuid) -> Result<Option<u64>> {
        let sql = format!(
            "SELECT MAX(executed_at) FROM scheduler_log WHERE script_id = '{}'",
            script_id,
        );
        let rows = self.engine.select_all(&sql)?;
        let Some(row) = rows.into_iter().next() else { return Ok(None); };
        let v = match row.first() {
            Some(v) => v,
            None    => return Ok(None),
        };
        if matches!(v.data, rust_dynamic::types::Val::Null) {
            return Ok(None);
        }
        Ok(v.clone().cast_int().ok().map(|i| i.max(0) as u64))
    }

    /// Drop rows older than `cutoff_secs` seconds before now.  Idempotent
    /// and cheap to call on every read.  Returns the number of rows
    /// deleted (best-effort; errors are surfaced because they typically
    /// mean the DB itself is broken).
    pub fn prune_older_than(&self, cutoff_secs: u64) -> Result<()> {
        let now = now_secs() as i64;
        let cutoff = now.saturating_sub(cutoff_secs as i64);
        let sql = format!("DELETE FROM scheduler_log WHERE executed_at < {}", cutoff);
        self.engine.execute(&sql)
    }

    /// Total row count — diagnostic only (surfaced through cluster
    /// status if anyone wants it).
    pub fn len(&self) -> Result<usize> {
        let rows = self.engine.select_all("SELECT COUNT(*) FROM scheduler_log")?;
        Ok(rows.into_iter().next()
            .and_then(|r| r.into_iter().next())
            .and_then(|v| v.cast_int().ok())
            .map(|i| i.max(0) as usize)
            .unwrap_or(0))
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// silences "unused import" when this file is built but the Val variant isn't referenced
#[allow(dead_code)] fn _force_dynval(_: &DynVal) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_log() -> (TempDir, SchedulerLog) {
        let tmp = TempDir::new().unwrap();
        let log = SchedulerLog::open(tmp.path()).unwrap();
        (tmp, log)
    }

    #[test]
    fn last_executed_is_none_initially() {
        let (_tmp, log) = open_log();
        let id = Uuid::now_v7();
        assert_eq!(log.last_executed(id).unwrap(), None);
    }

    #[test]
    fn record_then_last_executed_returns_max() {
        let (_tmp, log) = open_log();
        let script = Uuid::now_v7();
        let node   = Uuid::now_v7();
        log.record(script, node, 100).unwrap();
        log.record(script, node, 250).unwrap();
        log.record(script, node, 175).unwrap();
        assert_eq!(log.last_executed(script).unwrap(), Some(250));
    }

    #[test]
    fn last_executed_per_script_is_independent() {
        let (_tmp, log) = open_log();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let node = Uuid::now_v7();
        log.record(a, node, 100).unwrap();
        log.record(b, node, 999).unwrap();
        assert_eq!(log.last_executed(a).unwrap(), Some(100));
        assert_eq!(log.last_executed(b).unwrap(), Some(999));
    }

    #[test]
    fn prune_drops_old_rows() {
        let (_tmp, log) = open_log();
        let s = Uuid::now_v7();
        let n = Uuid::now_v7();
        let now = now_secs();
        // Record an execution well in the past.
        log.record(s, n, now.saturating_sub(3600)).unwrap();
        // Prune anything older than 60s.
        log.prune_older_than(60).unwrap();
        assert_eq!(log.last_executed(s).unwrap(), None,
            "row older than cutoff should have been pruned");
    }
}
