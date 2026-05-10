//! Hinted handoff storage for v3/add fan-out failures.
//!
//! When a coordinator's fire-and-forget replication call to a peer fails
//! (transport error, timeout, peer in Suspect/Dead state), the original
//! payload is enqueued here.  The hint replay task in
//! `bdsnode/server/cluster.rs` periodically drains hints whose target peer
//! is now Alive and retries them via `v2/add` / `v2/add.batch`.
//!
//! Storage: a single DuckDB file at `<dbpath>/network/hints.duckdb`.

use crate::common::error::Result;
use crate::storageengine::StorageEngine;
use rust_dynamic::value::Value as DynVal;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const INIT_SQL: &str = r#"
CREATE SEQUENCE IF NOT EXISTS hints_seq START 1;
CREATE TABLE IF NOT EXISTS hints (
    seq        BIGINT  PRIMARY KEY DEFAULT nextval('hints_seq'),
    peer_id    TEXT    NOT NULL,
    method     TEXT    NOT NULL,
    params     BLOB    NOT NULL,
    created_at BIGINT  NOT NULL
);
CREATE INDEX IF NOT EXISTS hints_peer_idx       ON hints(peer_id);
CREATE INDEX IF NOT EXISTS hints_created_at_idx ON hints(created_at);
"#;

#[derive(Debug, Clone)]
pub struct Hint {
    pub seq:        i64,
    pub peer_id:    Uuid,
    pub method:     String,
    pub params:     Vec<u8>,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct HintStorage {
    engine: Arc<StorageEngine>,
}

impl HintStorage {
    /// Open or create the hints DuckDB file.  `network_dir` is typically
    /// `<dbpath>/network/` (created by `persistence::ensure_network_dir`).
    pub fn open(network_dir: &Path) -> Result<Self> {
        let path = network_dir.join("hints.duckdb");
        let engine = StorageEngine::new(&path, INIT_SQL, 4)?;
        Ok(Self { engine: Arc::new(engine) })
    }

    pub fn enqueue(&self, peer_id: Uuid, method: &str, params_bytes: &[u8]) -> Result<()> {
        let now = now_secs();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, params_bytes);
        let sql = format!(
            "INSERT INTO hints (peer_id, method, params, created_at) \
             VALUES ('{}', '{}', from_base64('{}'), {})",
            peer_id, sql_escape(method), b64, now,
        );
        self.engine.execute(&sql)
    }

    /// Drain up to `limit` hints for `peer_id`, oldest first.  The caller
    /// is expected to retry each hint and call `delete_seqs` on success.
    pub fn drain_for_peer(&self, peer_id: Uuid, limit: usize) -> Result<Vec<Hint>> {
        let sql = format!(
            "SELECT seq, peer_id, method, params, created_at \
             FROM hints WHERE peer_id = '{}' ORDER BY seq ASC LIMIT {}",
            peer_id, limit
        );
        let rows = self.engine.select_all(&sql)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(Hint {
                seq:        cast_i64(&r, 0),
                peer_id,
                method:     cast_string(&r, 2),
                params:     cast_blob(&r, 3),
                created_at: cast_i64(&r, 4),
            });
        }
        Ok(out)
    }

    /// Distinct peer IDs that currently have at least one hint enqueued.
    pub fn peers_with_hints(&self) -> Result<Vec<Uuid>> {
        let rows = self.engine.select_all("SELECT DISTINCT peer_id FROM hints")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            if let Ok(id) = Uuid::parse_str(&cast_string(&r, 0)) {
                out.push(id);
            }
        }
        Ok(out)
    }

    pub fn delete_seqs(&self, seqs: &[i64]) -> Result<()> {
        if seqs.is_empty() { return Ok(()); }
        let list = seqs.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        self.engine.execute(&format!("DELETE FROM hints WHERE seq IN ({list})"))
    }

    /// Drop hints older than `max_age_secs`.  Returns the deleted count.
    pub fn prune_expired(&self, max_age_secs: u64) -> Result<u64> {
        let cutoff = now_secs().saturating_sub(max_age_secs as i64);
        let before = self.len()?;
        self.engine.execute(&format!("DELETE FROM hints WHERE created_at < {cutoff}"))?;
        let after  = self.len()?;
        Ok(before.saturating_sub(after))
    }

    pub fn len(&self) -> Result<u64> {
        let rows = self.engine.select_all("SELECT COUNT(*) FROM hints")?;
        Ok(rows.first().and_then(|r| r.first()).map(|v| {
            v.cast_int().unwrap_or(0) as u64
        }).unwrap_or(0))
    }

    /// Hints currently queued for `peer_id`.
    pub fn count_for_peer(&self, peer_id: Uuid) -> Result<u64> {
        let rows = self.engine.select_all(&format!(
            "SELECT COUNT(*) FROM hints WHERE peer_id = '{peer_id}'"
        ))?;
        Ok(rows.first().and_then(|r| r.first()).map(|v| {
            v.cast_int().unwrap_or(0) as u64
        }).unwrap_or(0))
    }

    /// Per-peer breakdown of the hint backlog: `(peer_id, n)` for every
    /// peer that currently has at least one hint.
    pub fn count_per_peer(&self) -> Result<Vec<(Uuid, u64)>> {
        let rows = self.engine.select_all(
            "SELECT peer_id, COUNT(*) FROM hints GROUP BY peer_id"
        )?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id_str = cast_string(&r, 0);
            let id = match Uuid::parse_str(&id_str) {
                Ok(u)  => u,
                Err(_) => continue,
            };
            out.push((id, cast_i64(&r, 1) as u64));
        }
        Ok(out)
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn cast_i64(row: &[DynVal], i: usize) -> i64 {
    row.get(i).and_then(|v| v.cast_int().ok()).unwrap_or(0)
}

fn cast_string(row: &[DynVal], i: usize) -> String {
    row.get(i).and_then(|v| v.cast_string().ok()).unwrap_or_default()
}

fn cast_blob(row: &[DynVal], i: usize) -> Vec<u8> {
    row.get(i).and_then(|v| v.cast_bin().ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn enqueue_drain_delete_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let h = HintStorage::open(tmp.path()).unwrap();
        let peer = Uuid::now_v7();

        assert_eq!(h.len().unwrap(), 0);
        h.enqueue(peer, "v2/add", b"params-1").unwrap();
        h.enqueue(peer, "v2/add", b"params-2").unwrap();
        assert_eq!(h.len().unwrap(), 2);

        let drained = h.drain_for_peer(peer, 10).unwrap();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].method, "v2/add");
        assert_eq!(drained[0].params, b"params-1");
        assert_eq!(drained[1].params, b"params-2");

        h.delete_seqs(&[drained[0].seq]).unwrap();
        assert_eq!(h.len().unwrap(), 1);
        h.delete_seqs(&[drained[1].seq]).unwrap();
        assert_eq!(h.len().unwrap(), 0);
    }

    #[test]
    fn peers_with_hints_dedups() {
        let tmp = TempDir::new().unwrap();
        let h = HintStorage::open(tmp.path()).unwrap();
        let p1 = Uuid::now_v7();
        let p2 = Uuid::now_v7();
        h.enqueue(p1, "v2/add", b"x").unwrap();
        h.enqueue(p1, "v2/add", b"y").unwrap();
        h.enqueue(p2, "v2/add", b"z").unwrap();
        let mut peers = h.peers_with_hints().unwrap();
        peers.sort();
        let mut expected = vec![p1, p2];
        expected.sort();
        assert_eq!(peers, expected);
    }

    #[test]
    fn prune_expired_drops_old() {
        let tmp = TempDir::new().unwrap();
        let h = HintStorage::open(tmp.path()).unwrap();
        let peer = Uuid::now_v7();
        h.enqueue(peer, "v2/add", b"old").unwrap();
        // backdate the row 24h
        h.engine.execute("UPDATE hints SET created_at = created_at - 86400").unwrap();
        h.enqueue(peer, "v2/add", b"new").unwrap();
        let dropped = h.prune_expired(60).unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(h.len().unwrap(), 1);
    }
}
