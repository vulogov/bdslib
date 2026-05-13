use crate::common::error::{err_msg, Result};
use crate::common::sql::sql_escape;
use crate::common::timerange::to_unix_secs;
use crate::common::uuid::generate_v7;
use crate::StorageEngine;
use rust_dynamic::value::Value as DynamicValue;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

// Schema for fresh catalogs.  Old catalogs (pre-retention) get patched up
// by `migrate_evicting_column()` after the table is opened — DuckDB
// rejects `ALTER TABLE ADD COLUMN` when secondary indexes exist on the
// table, so the migration drops + recreates them around the ALTER.
//
// `evicting` is BOOLEAN (no NOT NULL) so the migration's ADD COLUMN works
// on tables that already hold rows.  SELECTs use COALESCE(evicting, FALSE)
// to treat any stray NULLs as "not evicting".
const INIT_SQL: &str = "
    CREATE TABLE IF NOT EXISTS shards (
        shard_id TEXT   NOT NULL PRIMARY KEY,
        path     TEXT   NOT NULL,
        start_ts BIGINT NOT NULL,
        end_ts   BIGINT NOT NULL,
        evicting BOOLEAN DEFAULT FALSE
    );
    CREATE INDEX IF NOT EXISTS idx_shards_start_ts ON shards (start_ts);
    CREATE INDEX IF NOT EXISTS idx_shards_end_ts   ON shards (end_ts);
";

/// Metadata record for a single shard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardInfo {
    pub shard_id: Uuid,
    pub path: String,
    pub start_time: SystemTime,
    pub end_time: SystemTime,
}

/// Thread-safe storage for shard metadata backed by [`StorageEngine`].
///
/// Each shard covers the half-open interval `[start_time, end_time)`.
/// `ShardInfoEngine` is `Clone`; all clones share the same underlying
/// connection pool.
#[derive(Clone)]
pub struct ShardInfoEngine {
    engine: Arc<StorageEngine>,
}

impl ShardInfoEngine {
    /// Open or create a shard-info database at `path`.
    ///
    /// The required table is created automatically if it does not exist.
    /// Pass `":memory:"` for an ephemeral in-process store.
    pub fn new(path: &str, pool_size: u32) -> Result<Self> {
        let engine = StorageEngine::new(path, INIT_SQL, pool_size)?;
        migrate_evicting_column(&engine)?;
        Ok(Self {
            engine: Arc::new(engine),
        })
    }

    /// Store metadata for a new shard and return its generated UUIDv7.
    ///
    /// `path` is the filesystem location of the shard data.
    /// `start_time` must be strictly before `end_time`.
    pub fn add_shard(
        &self,
        path: &str,
        start_time: SystemTime,
        end_time: SystemTime,
    ) -> Result<Uuid> {
        if start_time >= end_time {
            return Err(err_msg("start_time must be strictly before end_time"));
        }
        let id = generate_v7();
        let start_ts = to_unix_secs(start_time)?;
        let end_ts = to_unix_secs(end_time)?;
        self.engine.execute(&format!(
            "INSERT INTO shards (shard_id, path, start_ts, end_ts, evicting) \
             VALUES ('{id}', '{}', {start_ts}, {end_ts}, FALSE)",
            sql_escape(path),
        ))?;
        Ok(id)
    }

    /// Return all shards whose interval `[start_time, end_time)` contains `timestamp`.
    ///
    /// Results are ordered by `start_time` ascending. Returns an empty `Vec`
    /// when no shard covers `timestamp`.
    pub fn shards_at(&self, timestamp: SystemTime) -> Result<Vec<ShardInfo>> {
        let ts = to_unix_secs(timestamp)?;
        let rows = self.engine.select_all(&format!(
            "SELECT shard_id, path, start_ts, end_ts \
             FROM shards \
             WHERE start_ts <= {ts} AND end_ts > {ts} \
             ORDER BY start_ts ASC"
        ))?;
        rows.into_iter().map(row_to_shard_info).collect()
    }

    /// Return all registered shards ordered by `start_time` ascending.
    pub fn list_all(&self) -> Result<Vec<ShardInfo>> {
        let rows = self.engine.select_all(
            "SELECT shard_id, path, start_ts, end_ts FROM shards ORDER BY start_ts ASC",
        )?;
        rows.into_iter().map(row_to_shard_info).collect()
    }

    /// Return all shards whose interval overlaps the half-open window `[start, end)`.
    ///
    /// A shard overlaps the window when `shard.end_ts > start AND shard.start_ts < end`.
    /// Results are ordered by `start_time` ascending.
    pub fn shards_in_range(
        &self,
        start: SystemTime,
        end: SystemTime,
    ) -> Result<Vec<ShardInfo>> {
        let start_ts = to_unix_secs(start)?;
        let end_ts = to_unix_secs(end)?;
        let rows = self.engine.select_all(&format!(
            "SELECT shard_id, path, start_ts, end_ts \
             FROM shards \
             WHERE end_ts > {start_ts} AND start_ts < {end_ts} \
             ORDER BY start_ts ASC"
        ))?;
        rows.into_iter().map(row_to_shard_info).collect()
    }

    /// Return `true` if at least one shard covers `timestamp`.
    pub fn shard_exists_at(&self, timestamp: SystemTime) -> Result<bool> {
        let ts = to_unix_secs(timestamp)?;
        let rows = self.engine.select_all(&format!(
            "SELECT COUNT(*) FROM shards WHERE start_ts <= {ts} AND end_ts > {ts}"
        ))?;
        let count = rows
            .first()
            .and_then(|r| r.first())
            .ok_or_else(|| err_msg("COUNT query returned no rows"))?
            .cast_int()
            .map_err(|e| err_msg(e.to_string()))?;
        Ok(count > 0)
    }

    // ── retention / eviction surface ──────────────────────────────────────────
    //
    // The `evicting` boolean column is the single source of truth for "this
    // shard is being torn down — don't open it, don't write to it, don't
    // resurrect it from disk on the next startup".  The retention task
    // flips it true BEFORE deleting any on-disk state so a concurrent
    // ingest that races us either succeeds against the doomed shard
    // (writes are lost on the delete) or sees `evicting=true` and falls
    // back to a future shard.  Crash recovery (`cleanup_orphan_evicting`)
    // discovers any leftover `evicting=true` rows from a prior aborted
    // sweep and finishes the job.

    /// Look up one shard by id.  Returns `None` when no row matches.
    pub fn get_by_id(&self, shard_id: Uuid) -> Result<Option<ShardInfo>> {
        let rows = self.engine.select_all(&format!(
            "SELECT shard_id, path, start_ts, end_ts \
             FROM shards WHERE shard_id = '{shard_id}'"
        ))?;
        match rows.into_iter().next() {
            Some(r) => Ok(Some(row_to_shard_info(r)?)),
            None    => Ok(None),
        }
    }

    /// Flip `evicting=true` for `shard_id`.  Idempotent — succeeds when the
    /// row is already marked.  Errors when the row does not exist.
    pub fn mark_evicting(&self, shard_id: Uuid) -> Result<()> {
        self.engine.execute(&format!(
            "UPDATE shards SET evicting = TRUE WHERE shard_id = '{shard_id}'"
        ))?;
        Ok(())
    }

    /// Delete a row by `shard_id`.  Idempotent — silent no-op when the row
    /// is already gone.
    pub fn delete_by_id(&self, shard_id: Uuid) -> Result<()> {
        self.engine.execute(&format!(
            "DELETE FROM shards WHERE shard_id = '{shard_id}'"
        ))?;
        Ok(())
    }

    /// Return every shard whose `end_ts` is strictly less than `cutoff_ts`
    /// AND that is NOT currently being evicted.  Caller picks the order
    /// to delete them in (the retention sweeper goes oldest-first to drop
    /// the most stale data first, then walks forward if it has budget).
    pub fn list_evictable(&self, cutoff_ts: i64) -> Result<Vec<ShardInfo>> {
        // COALESCE: rows from pre-retention catalogs may carry NULL in
        // `evicting` until the startup migration runs — treat them as
        // "not evicting" for safety.
        let rows = self.engine.select_all(&format!(
            "SELECT shard_id, path, start_ts, end_ts \
             FROM shards \
             WHERE end_ts < {cutoff_ts} AND COALESCE(evicting, FALSE) = FALSE \
             ORDER BY end_ts ASC"
        ))?;
        rows.into_iter().map(row_to_shard_info).collect()
    }

    /// Return every shard currently marked `evicting=true`.  Used by
    /// startup crash-recovery: any row left here is the residue of an
    /// aborted sweep and must be finished off.
    pub fn list_evicting(&self) -> Result<Vec<ShardInfo>> {
        let rows = self.engine.select_all(
            "SELECT shard_id, path, start_ts, end_ts \
             FROM shards WHERE COALESCE(evicting, FALSE) = TRUE \
             ORDER BY end_ts ASC"
        )?;
        rows.into_iter().map(row_to_shard_info).collect()
    }

    /// Is this specific shard marked for eviction?  Cheap — single
    /// indexed primary-key lookup.  Used by `ShardsCache::shard()` to
    /// short-circuit racing opens.
    pub fn is_evicting(&self, shard_id: Uuid) -> Result<bool> {
        let rows = self.engine.select_all(&format!(
            "SELECT COALESCE(evicting, FALSE) FROM shards WHERE shard_id = '{shard_id}'"
        ))?;
        match rows.first().and_then(|r| r.first()) {
            Some(v) => v.cast_bool().map_err(|e| err_msg(e.to_string())),
            None    => Ok(false),
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// One-shot migration that ensures the `evicting` column exists on an
/// already-populated catalog.  No-op when the column is already there.
///
/// DuckDB's `ALTER TABLE ADD COLUMN` is rejected when secondary indexes
/// depend on the table — fresh installs hit this because `INIT_SQL`
/// creates `idx_shards_start_ts` / `idx_shards_end_ts` before any ALTER
/// can run.  Production catalogs hit it for the same reason.  The fix
/// is straightforward: drop indexes → ALTER → recreate indexes.
fn migrate_evicting_column(engine: &StorageEngine) -> Result<()> {
    let cols = engine.select_all("PRAGMA table_info('shards')")?;
    let has_evicting = cols.iter().any(|row| {
        row.get(1)
            .and_then(|v| v.cast_string().ok())
            .as_deref() == Some("evicting")
    });
    if has_evicting {
        return Ok(());
    }
    // Drop dependent indexes, add the column, recreate the indexes.
    // Wrapped in a single execute_many so a crash mid-flight leaves the
    // catalog readable on next startup (DuckDB rolls back the
    // transaction).
    engine.execute_many(&[
        "DROP INDEX IF EXISTS idx_shards_start_ts".into(),
        "DROP INDEX IF EXISTS idx_shards_end_ts".into(),
        "ALTER TABLE shards ADD COLUMN evicting BOOLEAN DEFAULT FALSE".into(),
        "UPDATE shards SET evicting = FALSE WHERE evicting IS NULL".into(),
        "CREATE INDEX idx_shards_start_ts ON shards (start_ts)".into(),
        "CREATE INDEX idx_shards_end_ts   ON shards (end_ts)".into(),
    ])?;
    Ok(())
}

fn row_to_shard_info(row: Vec<DynamicValue>) -> Result<ShardInfo> {
    let shard_id_str = row[0].cast_string().map_err(|e| err_msg(e.to_string()))?;
    let shard_id = Uuid::parse_str(&shard_id_str)
        .map_err(|e| err_msg(format!("invalid UUID in shards table: {e}")))?;
    let path = row[1].cast_string().map_err(|e| err_msg(e.to_string()))?;
    let start_ts = row[2].cast_int().map_err(|e| err_msg(e.to_string()))?;
    let end_ts = row[3].cast_int().map_err(|e| err_msg(e.to_string()))?;
    Ok(ShardInfo {
        shard_id,
        path,
        start_time: UNIX_EPOCH + Duration::from_secs(start_ts as u64),
        end_time: UNIX_EPOCH + Duration::from_secs(end_ts as u64),
    })
}
