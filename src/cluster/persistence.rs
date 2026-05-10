//! On-disk persistence for the cluster layer.
//!
//! All cluster files live under `<dbpath>/network/`:
//!
//! | File          | Purpose                                                      |
//! |---------------|--------------------------------------------------------------|
//! | `node_id`     | This node's stable UUIDv7. Created on first start.           |
//! | `peers.json`  | Last-known peer table (for restart recovery).                |
//!
//! Hint storage (Phase 3) and anti-entropy bookkeeping (Phase 4) will be added
//! to the same directory in later phases.

use crate::cluster::peer_table::Peer;
use crate::common::error::{err_msg, Result};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Directory layout helper.
pub fn network_dir(dbpath: &str) -> PathBuf {
    PathBuf::from(dbpath).join("network")
}

pub fn ensure_network_dir(dbpath: &str) -> Result<PathBuf> {
    let dir = network_dir(dbpath);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| err_msg(format!("create {}: {e}", dir.display())))?;
    }
    Ok(dir)
}

/// Read `<network_dir>/node_id` if it exists, otherwise generate a fresh
/// UUIDv7 and write it.  The same id is reused on every subsequent start.
pub fn load_or_init_node_id(network_dir: &Path) -> Result<Uuid> {
    let path = network_dir.join("node_id");
    if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| err_msg(format!("read {}: {e}", path.display())))?;
        Uuid::parse_str(raw.trim())
            .map_err(|e| err_msg(format!("invalid UUID in {}: {e}", path.display())))
    } else {
        let id = Uuid::now_v7();
        std::fs::write(&path, id.to_string())
            .map_err(|e| err_msg(format!("write {}: {e}", path.display())))?;
        Ok(id)
    }
}

pub fn save_peers(network_dir: &Path, peers: &[Peer]) -> Result<()> {
    let path = network_dir.join("peers.json");
    let json = serde_json::to_string_pretty(peers)
        .map_err(|e| err_msg(format!("serialize peers: {e}")))?;
    // Atomic-ish write: tmp + rename.
    let tmp = network_dir.join("peers.json.tmp");
    std::fs::write(&tmp, json)
        .map_err(|e| err_msg(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| err_msg(format!("rename {} -> {}: {e}", tmp.display(), path.display())))?;
    Ok(())
}

pub fn load_peers(network_dir: &Path) -> Result<Vec<Peer>> {
    let path = network_dir.join("peers.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| err_msg(format!("read {}: {e}", path.display())))?;
    serde_json::from_str(&raw)
        .map_err(|e| err_msg(format!("parse {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::peer_table::{Peer, PeerState};
    use tempfile::TempDir;

    #[test]
    fn node_id_is_stable_across_loads() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let a = load_or_init_node_id(&dir).unwrap();
        let b = load_or_init_node_id(&dir).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn peers_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let mut p = Peer::new(Uuid::now_v7(), "http://x:9000".into());
        p.state     = PeerState::Alive;
        p.last_seen = 100;
        save_peers(&dir, &[p.clone()]).unwrap();
        let loaded = load_peers(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].node_id, p.node_id);
        assert_eq!(loaded[0].state,   PeerState::Alive);
    }

    #[test]
    fn load_peers_returns_empty_when_missing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        assert!(load_peers(&dir).unwrap().is_empty());
    }
}
