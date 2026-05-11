//! Local durable job queue for async / offline LLM inference.
//!
//! Lives at `<dbpath>/llm/jobs.duckdb`.  **Per-node, not replicated**:
//! - Claim races between coordinators are resolved by the
//!   `InferenceLog` dedup layer (phase 4) keyed on `cache_key`, so
//!   replicating the queue itself adds complexity for no win.
//! - The result of the job lands in the replicated inference cache
//!   (phase 3) AND in the per-node `ResultQueue` under `result_id`,
//!   so callers poll `v2/results.pull` exactly like queued Bund
//!   script evaluations today.
//!
//! Schema:
//! ```sql
//! CREATE TABLE llm_jobs (
//!     job_id       TEXT PRIMARY KEY,  -- UUIDv7
//!     result_id    TEXT NOT NULL,     -- UUIDv7 for v2/results.pull
//!     kind         TEXT NOT NULL,     -- "complete"|"chat"|"analyze"
//!     request_json TEXT NOT NULL,     -- original v4/* request body
//!     state        TEXT NOT NULL,     -- pending|running|done|failed|cancelled
//!     owner_node   TEXT,              -- node that claimed it (NULL when pending)
//!     submitted_at BIGINT NOT NULL,
//!     started_at   BIGINT,
//!     finished_at  BIGINT,
//!     error        TEXT
//! );
//! ```

use crate::common::error::{err_msg, Result};
use crate::storageengine::StorageEngine;
use serde_json::Value as JsonValue;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS llm_jobs (
    job_id       TEXT   PRIMARY KEY,
    result_id    TEXT   NOT NULL,
    kind         TEXT   NOT NULL,
    request_json TEXT   NOT NULL,
    state        TEXT   NOT NULL,
    owner_node   TEXT,
    submitted_at BIGINT NOT NULL,
    started_at   BIGINT,
    finished_at  BIGINT,
    error        TEXT
);
CREATE INDEX IF NOT EXISTS llm_jobs_state_idx       ON llm_jobs(state);
CREATE INDEX IF NOT EXISTS llm_jobs_submitted_idx   ON llm_jobs(submitted_at);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Pending   => "pending",
            JobState::Running   => "running",
            JobState::Done      => "done",
            JobState::Failed    => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "pending"   => Some(JobState::Pending),
            "running"   => Some(JobState::Running),
            "done"      => Some(JobState::Done),
            "failed"    => Some(JobState::Failed),
            "cancelled" => Some(JobState::Cancelled),
            _           => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, JobState::Done | JobState::Failed | JobState::Cancelled)
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub job_id:       Uuid,
    pub result_id:    Uuid,
    pub kind:         String,
    pub request_json: JsonValue,
    pub state:        JobState,
    pub owner_node:   Option<Uuid>,
    pub submitted_at: u64,
    pub started_at:   Option<u64>,
    pub finished_at:  Option<u64>,
    pub error:        Option<String>,
}

/// Subset of fields callers need when enqueueing — the queue assigns
/// `job_id` / `result_id` / `submitted_at` and starts the row at `pending`.
#[derive(Debug, Clone)]
pub struct JobInsert {
    pub kind:         String,
    pub request_json: JsonValue,
    /// Optional caller-supplied result_id (lets the caller share the
    /// same id across multiple jobs that should land in one queue).
    /// When `None`, a fresh UUIDv7 is minted.
    pub result_id:    Option<Uuid>,
}

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub state: Option<JobState>,
    /// Soft cap on returned rows; `None` returns everything.
    pub limit: Option<usize>,
}

#[derive(Clone)]
pub struct JobQueue {
    engine: Arc<StorageEngine>,
    #[allow(dead_code)]
    path:   PathBuf,
}

impl JobQueue {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .map_err(|e| err_msg(format!("llm::jobs: create dir {root:?}: {e}")))?;
        let path = root.join("jobs.duckdb");
        let engine = StorageEngine::new(&path, INIT_SQL, 4)?;
        Ok(Self { engine: Arc::new(engine), path })
    }

    pub fn count(&self) -> Result<u64> {
        let rows = self.engine.select_all("SELECT COUNT(*) FROM llm_jobs")?;
        Ok(rows.into_iter().next()
            .and_then(|r| r.into_iter().next())
            .and_then(|v| v.cast_int().ok())
            .map(|i| i.max(0) as u64)
            .unwrap_or(0))
    }

    pub fn count_in_state(&self, state: JobState) -> Result<u64> {
        let rows = self.engine.select_all(&format!(
            "SELECT COUNT(*) FROM llm_jobs WHERE state = '{}'", state.as_str()
        ))?;
        Ok(rows.into_iter().next()
            .and_then(|r| r.into_iter().next())
            .and_then(|v| v.cast_int().ok())
            .map(|i| i.max(0) as u64)
            .unwrap_or(0))
    }

    /// Enqueue a new job in the `pending` state.  Returns the assigned
    /// `(job_id, result_id)` so the caller can return them immediately
    /// to the operator without waiting for the runner.
    pub fn enqueue(&self, insert: JobInsert) -> Result<(Uuid, Uuid)> {
        let job_id    = Uuid::now_v7();
        let result_id = insert.result_id.unwrap_or_else(Uuid::now_v7);
        let request_s = serde_json::to_string(&insert.request_json)
            .map_err(|e| err_msg(format!("llm::jobs: request serialise: {e}")))?;
        let now = now_secs();
        let sql = format!(
            "INSERT INTO llm_jobs \
              (job_id, result_id, kind, request_json, state, owner_node, \
               submitted_at, started_at, finished_at, error) \
             VALUES ('{}', '{}', '{}', '{}', 'pending', NULL, {}, NULL, NULL, NULL)",
            job_id, result_id,
            sql_escape(&insert.kind),
            sql_escape(&request_s),
            now as i64,
        );
        self.engine.execute(&sql)?;
        Ok((job_id, result_id))
    }

    /// Claim the oldest `pending` job for `node_id`.  Atomic enough
    /// in the single-runner-per-node case (DuckDB serialises the
    /// UPDATE).  Returns `None` when no pending work exists.
    ///
    /// A second concurrent call from the same node may return the
    /// same job_id between the SELECT and UPDATE — the caller MUST
    /// check `state == "running" && owner_node == self` after the
    /// claim and skip if it lost the race (only relevant if you ever
    /// run more than one runner per node).
    pub fn claim_one(&self, node_id: Uuid) -> Result<Option<Job>> {
        let candidate = self.engine.select_all(
            "SELECT job_id FROM llm_jobs \
             WHERE state = 'pending' \
             ORDER BY submitted_at LIMIT 1"
        )?;
        let Some(row) = candidate.into_iter().next() else { return Ok(None); };
        let job_id_s = match row.into_iter().next() {
            Some(v) => v.cast_string().map_err(|e| err_msg(format!("job_id cast: {e}")))?,
            None    => return Ok(None),
        };
        let job_id = Uuid::parse_str(&job_id_s)
            .map_err(|e| err_msg(format!("job_id parse: {e}")))?;
        let now = now_secs();
        let sql = format!(
            "UPDATE llm_jobs \
             SET state = 'running', owner_node = '{node_id}', started_at = {} \
             WHERE job_id = '{job_id}' AND state = 'pending'",
            now as i64,
        );
        self.engine.execute(&sql)?;
        self.get(job_id).map(|j| j.filter(|j| j.state == JobState::Running
                                          && j.owner_node == Some(node_id)))
    }

    pub fn mark_done(&self, job_id: Uuid) -> Result<()> {
        self.terminal_update(job_id, JobState::Done, None)
    }

    pub fn mark_failed(&self, job_id: Uuid, error: &str) -> Result<()> {
        self.terminal_update(job_id, JobState::Failed, Some(error))
    }

    fn terminal_update(&self, job_id: Uuid, state: JobState, error: Option<&str>) -> Result<()> {
        let now = now_secs();
        let err_clause = match error {
            Some(e) => format!(", error = '{}'", sql_escape(e)),
            None    => ", error = NULL".to_owned(),
        };
        let sql = format!(
            "UPDATE llm_jobs SET state = '{}', finished_at = {} {} \
             WHERE job_id = '{job_id}' AND state IN ('pending','running')",
            state.as_str(), now as i64, err_clause,
        );
        self.engine.execute(&sql)
    }

    /// Cancel a job.  Returns `true` if the cancellation took effect
    /// (the job was pending or running), `false` if it was already
    /// terminal.
    pub fn cancel(&self, job_id: Uuid) -> Result<bool> {
        let current = match self.get(job_id)? {
            Some(j) => j,
            None    => return Ok(false),
        };
        if current.state.is_terminal() {
            return Ok(false);
        }
        // We can only stop pending jobs from being started; a running
        // job continues until the provider call returns, then the
        // runner sees the cancelled flag (in this column) and skips
        // the cache_store / results_push and writes 'cancelled' as
        // its terminal state instead of 'done'.
        let now = now_secs();
        let sql = format!(
            "UPDATE llm_jobs SET state = 'cancelled', finished_at = {} \
             WHERE job_id = '{job_id}' AND state IN ('pending','running')",
            now as i64,
        );
        self.engine.execute(&sql)?;
        Ok(true)
    }

    pub fn get(&self, job_id: Uuid) -> Result<Option<Job>> {
        let sql = format!(
            "SELECT job_id, result_id, kind, request_json, state, owner_node, \
                    submitted_at, started_at, finished_at, error \
             FROM llm_jobs WHERE job_id = '{job_id}'"
        );
        let mut rows = self.engine.select_all(&sql)?;
        match rows.pop() {
            Some(r) => Ok(Some(row_to_job(r)?)),
            None    => Ok(None),
        }
    }

    pub fn list(&self, filter: ListFilter) -> Result<Vec<Job>> {
        let where_clause = match filter.state {
            Some(s) => format!(" WHERE state = '{}'", s.as_str()),
            None    => String::new(),
        };
        let limit_clause = match filter.limit {
            Some(n) => format!(" LIMIT {n}"),
            None    => String::new(),
        };
        let sql = format!(
            "SELECT job_id, result_id, kind, request_json, state, owner_node, \
                    submitted_at, started_at, finished_at, error \
             FROM llm_jobs{where_clause} ORDER BY submitted_at DESC{limit_clause}"
        );
        let rows = self.engine.select_all(&sql)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(row_to_job(r)?);
        }
        Ok(out)
    }

    /// Drop terminal rows older than `cutoff_secs` from now.  Pending /
    /// running rows are never pruned regardless of age — the runner
    /// will tick them through to terminal first.
    pub fn prune_terminal_older_than(&self, cutoff_secs: u64) -> Result<u64> {
        let before = self.count()?;
        let cutoff = now_secs().saturating_sub(cutoff_secs) as i64;
        let sql = format!(
            "DELETE FROM llm_jobs \
             WHERE state IN ('done','failed','cancelled') \
               AND finished_at IS NOT NULL AND finished_at < {cutoff}"
        );
        self.engine.execute(&sql)?;
        let after = self.count()?;
        Ok(before.saturating_sub(after))
    }
}

// ─────────────────────────────────────────────────────────────────────
// Process-wide singleton (same pattern as cache::manager())
// ─────────────────────────────────────────────────────────────────────

static GLOBAL: OnceLock<JobQueue> = OnceLock::new();

pub fn init(queue: JobQueue) { let _ = GLOBAL.set(queue); }
pub fn queue() -> Option<&'static JobQueue> { GLOBAL.get() }

// ─────────────────────────────────────────────────────────────────────
// Row → Job plumbing
// ─────────────────────────────────────────────────────────────────────

fn row_to_job(row: Vec<rust_dynamic::value::Value>) -> Result<Job> {
    let mut it = row.into_iter();
    let job_id_s    = it.next().ok_or_else(|| err_msg("missing job_id"))?
                       .cast_string().map_err(|e| err_msg(format!("job_id cast: {e}")))?;
    let job_id      = Uuid::parse_str(&job_id_s)
                       .map_err(|e| err_msg(format!("job_id parse: {e}")))?;
    let result_id_s = it.next().ok_or_else(|| err_msg("missing result_id"))?
                       .cast_string().map_err(|e| err_msg(format!("result_id cast: {e}")))?;
    let result_id   = Uuid::parse_str(&result_id_s)
                       .map_err(|e| err_msg(format!("result_id parse: {e}")))?;
    let kind        = it.next().ok_or_else(|| err_msg("missing kind"))?
                       .cast_string().map_err(|e| err_msg(format!("kind cast: {e}")))?;
    let request_s   = it.next().ok_or_else(|| err_msg("missing request_json"))?
                       .cast_string().map_err(|e| err_msg(format!("request_json cast: {e}")))?;
    let request_json: JsonValue = serde_json::from_str(&request_s)
        .map_err(|e| err_msg(format!("request_json parse: {e}")))?;
    let state_s     = it.next().ok_or_else(|| err_msg("missing state"))?
                       .cast_string().map_err(|e| err_msg(format!("state cast: {e}")))?;
    let state       = JobState::from_wire(&state_s)
                       .ok_or_else(|| err_msg(format!("unknown state {state_s:?}")))?;
    let owner_node  = match it.next() {
        Some(v) if matches!(v.data, rust_dynamic::types::Val::Null) => None,
        Some(v) => {
            let s = v.cast_string().map_err(|e| err_msg(format!("owner_node cast: {e}")))?;
            if s.is_empty() { None } else {
                Some(Uuid::parse_str(&s).map_err(|e| err_msg(format!("owner_node parse: {e}")))?)
            }
        }
        None    => None,
    };
    let submitted_at = it.next().ok_or_else(|| err_msg("missing submitted_at"))?
                       .cast_int().map_err(|e| err_msg(format!("submitted_at cast: {e}")))?;
    let started_at   = nullable_int(it.next())?;
    let finished_at  = nullable_int(it.next())?;
    let error        = match it.next() {
        Some(v) if matches!(v.data, rust_dynamic::types::Val::Null) => None,
        Some(v) => v.cast_string().ok().filter(|s| !s.is_empty()),
        None    => None,
    };
    Ok(Job {
        job_id, result_id, kind, request_json, state, owner_node,
        submitted_at: submitted_at.max(0) as u64,
        started_at,
        finished_at,
        error,
    })
}

fn nullable_int(v: Option<rust_dynamic::value::Value>) -> Result<Option<u64>> {
    match v {
        Some(v) if matches!(v.data, rust_dynamic::types::Val::Null) => Ok(None),
        Some(v) => v.cast_int().map(|n| Some(n.max(0) as u64))
                    .map_err(|e| err_msg(format!("int cast: {e}"))),
        None    => Ok(None),
    }
}

fn sql_escape(s: &str) -> String { s.replace('\'', "''") }

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn open_queue() -> (TempDir, JobQueue) {
        let tmp = TempDir::new().unwrap();
        let q = JobQueue::open(tmp.path()).unwrap();
        (tmp, q)
    }

    fn ins(kind: &str) -> JobInsert {
        JobInsert {
            kind:         kind.to_owned(),
            request_json: json!({"prompt": "hello"}),
            result_id:    None,
        }
    }

    #[test]
    fn enqueue_returns_two_uuids_and_creates_pending_row() {
        let (_tmp, q) = open_queue();
        let (job_id, result_id) = q.enqueue(ins("complete")).unwrap();
        assert_ne!(job_id, result_id);

        let row = q.get(job_id).unwrap().unwrap();
        assert_eq!(row.state, JobState::Pending);
        assert_eq!(row.kind, "complete");
        assert_eq!(row.result_id, result_id);
        assert!(row.owner_node.is_none());
        assert!(row.started_at.is_none());
        assert!(row.finished_at.is_none());
        assert!(row.error.is_none());
        assert!(row.submitted_at > 0);
    }

    #[test]
    fn enqueue_with_explicit_result_id_uses_it() {
        let (_tmp, q) = open_queue();
        let rid = Uuid::now_v7();
        let (_jid, result_id) = q.enqueue(JobInsert {
            kind: "complete".into(), request_json: json!({}), result_id: Some(rid),
        }).unwrap();
        assert_eq!(result_id, rid);
    }

    #[test]
    fn claim_one_flips_oldest_pending_to_running() {
        let (_tmp, q) = open_queue();
        let (a, _) = q.enqueue(ins("complete")).unwrap();
        // Sleep one second so submitted_at differs reliably; older job
        // should be picked first.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let (b, _) = q.enqueue(ins("complete")).unwrap();
        let node = Uuid::now_v7();
        let claimed = q.claim_one(node).unwrap().unwrap();
        assert_eq!(claimed.job_id, a);
        assert_eq!(claimed.state, JobState::Running);
        assert_eq!(claimed.owner_node, Some(node));
        assert!(claimed.started_at.is_some());

        // Second claim picks the other one.
        let next = q.claim_one(node).unwrap().unwrap();
        assert_eq!(next.job_id, b);
    }

    #[test]
    fn claim_one_returns_none_when_no_pending() {
        let (_tmp, q) = open_queue();
        assert!(q.claim_one(Uuid::now_v7()).unwrap().is_none());

        // After enqueue + claim, nothing else pending.
        q.enqueue(ins("complete")).unwrap();
        let _ = q.claim_one(Uuid::now_v7()).unwrap();
        assert!(q.claim_one(Uuid::now_v7()).unwrap().is_none());
    }

    #[test]
    fn mark_done_transitions_running_row() {
        let (_tmp, q) = open_queue();
        let (jid, _) = q.enqueue(ins("complete")).unwrap();
        let node = Uuid::now_v7();
        q.claim_one(node).unwrap();
        q.mark_done(jid).unwrap();
        let row = q.get(jid).unwrap().unwrap();
        assert_eq!(row.state, JobState::Done);
        assert!(row.finished_at.is_some());
        assert!(row.error.is_none());
    }

    #[test]
    fn mark_failed_records_error_message() {
        let (_tmp, q) = open_queue();
        let (jid, _) = q.enqueue(ins("complete")).unwrap();
        q.claim_one(Uuid::now_v7()).unwrap();
        q.mark_failed(jid, "provider went boom").unwrap();
        let row = q.get(jid).unwrap().unwrap();
        assert_eq!(row.state, JobState::Failed);
        assert_eq!(row.error.as_deref(), Some("provider went boom"));
    }

    #[test]
    fn mark_done_on_unknown_id_is_noop() {
        let (_tmp, q) = open_queue();
        q.mark_done(Uuid::now_v7()).unwrap();
        // No error; no row created
        assert_eq!(q.count().unwrap(), 0);
    }

    #[test]
    fn mark_done_does_not_re_enter_terminal_state() {
        let (_tmp, q) = open_queue();
        let (jid, _) = q.enqueue(ins("complete")).unwrap();
        q.claim_one(Uuid::now_v7()).unwrap();
        q.mark_done(jid).unwrap();
        let first_finished = q.get(jid).unwrap().unwrap().finished_at;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Second mark_done should be a no-op (state is already 'done',
        // doesn't match the WHERE state IN ('pending','running') guard).
        q.mark_done(jid).unwrap();
        let second_finished = q.get(jid).unwrap().unwrap().finished_at;
        assert_eq!(first_finished, second_finished,
            "terminal state should not be re-stamped");
    }

    #[test]
    fn cancel_pending_job_transitions_to_cancelled() {
        let (_tmp, q) = open_queue();
        let (jid, _) = q.enqueue(ins("complete")).unwrap();
        assert!(q.cancel(jid).unwrap());
        let row = q.get(jid).unwrap().unwrap();
        assert_eq!(row.state, JobState::Cancelled);
        assert!(row.finished_at.is_some());

        // Re-cancelling is a no-op and returns false (already terminal).
        assert!(!q.cancel(jid).unwrap());
    }

    #[test]
    fn cancel_running_job_transitions_to_cancelled() {
        let (_tmp, q) = open_queue();
        let (jid, _) = q.enqueue(ins("complete")).unwrap();
        q.claim_one(Uuid::now_v7()).unwrap();
        assert!(q.cancel(jid).unwrap());
        assert_eq!(q.get(jid).unwrap().unwrap().state, JobState::Cancelled);
    }

    #[test]
    fn cancel_unknown_job_returns_false() {
        let (_tmp, q) = open_queue();
        assert!(!q.cancel(Uuid::now_v7()).unwrap());
    }

    #[test]
    fn list_filter_by_state_returns_only_matching() {
        let (_tmp, q) = open_queue();
        let (a, _) = q.enqueue(ins("complete")).unwrap();
        let (b, _) = q.enqueue(ins("complete")).unwrap();
        q.claim_one(Uuid::now_v7()).unwrap();      // a → running
        q.mark_done(a).unwrap();

        let pending = q.list(ListFilter { state: Some(JobState::Pending), limit: None }).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].job_id, b);

        let done = q.list(ListFilter { state: Some(JobState::Done), limit: None }).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].job_id, a);

        let all = q.list(ListFilter::default()).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn list_limit_caps_returned_rows() {
        let (_tmp, q) = open_queue();
        for _ in 0..5 { q.enqueue(ins("complete")).unwrap(); }
        let two = q.list(ListFilter { state: None, limit: Some(2) }).unwrap();
        assert_eq!(two.len(), 2);
    }

    #[test]
    fn count_in_state_returns_per_state_totals() {
        let (_tmp, q) = open_queue();
        let (a, _) = q.enqueue(ins("complete")).unwrap();
        q.enqueue(ins("complete")).unwrap();
        q.claim_one(Uuid::now_v7()).unwrap();      // a → running
        q.mark_done(a).unwrap();
        assert_eq!(q.count_in_state(JobState::Pending).unwrap(), 1);
        assert_eq!(q.count_in_state(JobState::Done).unwrap(),    1);
        assert_eq!(q.count_in_state(JobState::Running).unwrap(), 0);
    }

    #[test]
    fn prune_drops_only_terminal_older_than_cutoff() {
        let (_tmp, q) = open_queue();
        let (a, _) = q.enqueue(ins("complete")).unwrap();
        q.enqueue(ins("complete")).unwrap();        // pending — preserve
        q.claim_one(Uuid::now_v7()).unwrap();
        q.mark_done(a).unwrap();
        // Force the finished_at into the past via a raw UPDATE.
        let past = (now_secs() as i64).saturating_sub(7200);
        q.engine.execute(&format!(
            "UPDATE llm_jobs SET finished_at = {past} WHERE job_id = '{a}'"
        )).unwrap();
        let dropped = q.prune_terminal_older_than(3600).unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(q.count().unwrap(), 1);
        assert!(q.get(a).unwrap().is_none(), "old done row should be pruned");
    }

    #[test]
    fn json_state_round_trips_through_storage() {
        for s in [JobState::Pending, JobState::Running, JobState::Done,
                  JobState::Failed,  JobState::Cancelled] {
            assert_eq!(JobState::from_wire(s.as_str()), Some(s));
        }
        assert_eq!(JobState::from_wire("nonsense"), None);
    }
}
