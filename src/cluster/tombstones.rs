//! Tombstone storage for fully-replicated stores (docs, signals, scripts).
//!
//! Phase 4 ships v3 deletes that have to propagate to peers and survive
//! anti-entropy.  Without tombstones an anti-entropy pull-sync would
//! resurrect a deleted record from any peer that hadn't yet applied the
//! delete (or from a long-down peer that came back online).
//!
//! Layout: a single DuckDB at `<dbpath>/network/tombstones.duckdb` with one
//! row per `(store, id)` deletion.  Tombstones are pruned after
//! `cluster.hint_max_age` (default 24h) — long enough to give every peer a
//! chance to learn about the deletion without growing unbounded.

use crate::common::error::Result;
use crate::storageengine::StorageEngine;
use rust_dynamic::value::Value as DynVal;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS tombstones (
    store      TEXT   NOT NULL,
    id         TEXT   NOT NULL,
    deleted_at BIGINT NOT NULL,
    PRIMARY KEY (store, id)
);
CREATE INDEX IF NOT EXISTS tombstones_store_idx      ON tombstones(store);
CREATE INDEX IF NOT EXISTS tombstones_deleted_at_idx ON tombstones(deleted_at);
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub store:      String,
    pub id:         Uuid,
    pub deleted_at: i64,
}

#[derive(Clone)]
pub struct TombstoneStorage {
    engine: Arc<StorageEngine>,
}

impl TombstoneStorage {
    pub fn open(network_dir: &Path) -> Result<Self> {
        let path = network_dir.join("tombstones.duckdb");
        let engine = StorageEngine::new(&path, INIT_SQL, 4)?;
        Ok(Self { engine: Arc::new(engine) })
    }

    /// Mark `(store, id)` as deleted.  Idempotent — re-marking an existing
    /// tombstone updates `deleted_at` to the maximum of old and new (so
    /// older delete-replays don't overwrite the canonical deletion time).
    pub fn mark_deleted(&self, store: &str, id: Uuid, deleted_at: i64) -> Result<()> {
        // ON CONFLICT preserves the larger deleted_at — last delete wins.
        let sql = format!(
            "INSERT INTO tombstones (store, id, deleted_at) VALUES ('{}', '{}', {}) \
             ON CONFLICT (store, id) DO UPDATE SET deleted_at = greatest(tombstones.deleted_at, EXCLUDED.deleted_at)",
            sql_escape(store), id, deleted_at,
        );
        self.engine.execute(&sql)
    }

    pub fn is_deleted(&self, store: &str, id: Uuid) -> Result<bool> {
        let rows = self.engine.select_all(&format!(
            "SELECT deleted_at FROM tombstones WHERE store = '{}' AND id = '{}'",
            sql_escape(store), id,
        ))?;
        Ok(!rows.is_empty())
    }

    /// All tombstones for `store` (used by anti-entropy diff and by
    /// `v2/*.list_ids` so peers can learn about deletions they missed).
    pub fn list_for_store(&self, store: &str) -> Result<Vec<Tombstone>> {
        let rows = self.engine.select_all(&format!(
            "SELECT store, id, deleted_at FROM tombstones WHERE store = '{}' ORDER BY deleted_at DESC",
            sql_escape(store),
        ))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id_str = cast_string(&r, 1);
            let id = match Uuid::parse_str(&id_str) {
                Ok(u)  => u,
                Err(_) => continue,
            };
            out.push(Tombstone {
                store:      cast_string(&r, 0),
                id,
                deleted_at: cast_i64(&r, 2),
            });
        }
        Ok(out)
    }

    pub fn len(&self) -> Result<u64> {
        let rows = self.engine.select_all("SELECT COUNT(*) FROM tombstones")?;
        Ok(rows.first().and_then(|r| r.first()).map(|v| {
            v.cast_int().unwrap_or(0) as u64
        }).unwrap_or(0))
    }

    /// Drop tombstones older than `max_age_secs`.  Returns the deleted count.
    pub fn prune_old(&self, max_age_secs: u64) -> Result<u64> {
        let cutoff = now_secs().saturating_sub(max_age_secs as i64);
        let before = self.len()?;
        self.engine.execute(&format!("DELETE FROM tombstones WHERE deleted_at < {cutoff}"))?;
        let after = self.len()?;
        Ok(before.saturating_sub(after))
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sql_escape(s: &str) -> String { s.replace('\'', "''") }

fn cast_i64(row: &[DynVal], i: usize) -> i64 {
    row.get(i).and_then(|v| v.cast_int().ok()).unwrap_or(0)
}

fn cast_string(row: &[DynVal], i: usize) -> String {
    row.get(i).and_then(|v| v.cast_string().ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn mark_query_dedups_by_id() {
        let tmp = TempDir::new().unwrap();
        let t = TombstoneStorage::open(tmp.path()).unwrap();
        let id = Uuid::now_v7();

        assert!(!t.is_deleted("docs", id).unwrap());
        t.mark_deleted("docs", id, 100).unwrap();
        assert!(t.is_deleted("docs", id).unwrap());
        // Re-mark with smaller ts → keeps larger ts.
        t.mark_deleted("docs", id, 50).unwrap();
        let entries = t.list_for_store("docs").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].deleted_at, 100);
        // Re-mark with larger ts → updates.
        t.mark_deleted("docs", id, 200).unwrap();
        let entries = t.list_for_store("docs").unwrap();
        assert_eq!(entries[0].deleted_at, 200);
    }

    #[test]
    fn store_namespaces_are_independent() {
        let tmp = TempDir::new().unwrap();
        let t = TombstoneStorage::open(tmp.path()).unwrap();
        let id = Uuid::now_v7();
        t.mark_deleted("docs", id, 100).unwrap();
        assert!(t.is_deleted("docs", id).unwrap());
        assert!(!t.is_deleted("signals", id).unwrap());
        assert!(!t.is_deleted("scripts", id).unwrap());
    }

    #[test]
    fn prune_old_drops_expired() {
        let tmp = TempDir::new().unwrap();
        let t = TombstoneStorage::open(tmp.path()).unwrap();
        let old = Uuid::now_v7();
        let new = Uuid::now_v7();
        t.mark_deleted("docs", old, now_secs() - 86_400).unwrap();
        t.mark_deleted("docs", new, now_secs()).unwrap();
        assert_eq!(t.prune_old(60).unwrap(), 1);
        assert_eq!(t.len().unwrap(), 1);
    }
}
