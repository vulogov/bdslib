//! Cluster-replicated user store.
//!
//! Lives at `<dbpath>/users/users.duckdb` — a 4th fully-replicated
//! store alongside docs / signals / scripts.  Schema:
//!
//! ```sql
//! CREATE TABLE users (
//!   id              TEXT PRIMARY KEY,        -- UUIDv7 string
//!   username        TEXT UNIQUE NOT NULL,
//!   credential_hash TEXT NOT NULL,           -- argon2 PHC string for Password
//!                                            -- (or provider user-id for OAuth)
//!   auth_method     TEXT NOT NULL,           -- "password" | "oauth-google" | …
//!   metadata        JSON NOT NULL,           -- {display_name, email, …}
//!   created_at      BIGINT NOT NULL,
//!   updated_at      BIGINT NOT NULL,         -- LWW key for cluster AE
//!   disabled        BOOLEAN NOT NULL DEFAULT false
//! );
//! ```
//!
//! Replication is the same recipe as docs/signals/scripts — Phase 7.2
//! adds `v3/user.add`/`modify`/`delete` coordinators that fan out via
//! `cluster::replication::replicate_to_all`, with hint-on-failure and
//! tombstones for deletes.  This module is the per-node SQL layer
//! consumed by both the local v2 receivers and the AE pull path.
//!
//! Authentication is per-node — `verify(username, password)` does a
//! local SQL lookup + argon2 compare.  The store does NOT fan out on
//! authenticate; the cluster-aware fallback (when the local node hasn't
//! yet replicated a recently-added user) is implemented one layer up
//! in the bdsnode `v3/user.authenticate` handler (Phase 7.2).

use crate::cluster::credential::{AuthMethod, VerifierRegistry};
use crate::common::error::{err_msg, Result};
use crate::storageengine::StorageEngine;
use serde_json::Value as JsonValue;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

const INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id              TEXT    PRIMARY KEY,
    username        TEXT    UNIQUE NOT NULL,
    credential_hash TEXT    NOT NULL,
    auth_method     TEXT    NOT NULL,
    metadata        TEXT    NOT NULL,
    created_at      BIGINT  NOT NULL,
    updated_at      BIGINT  NOT NULL,
    disabled        BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX IF NOT EXISTS users_username_idx   ON users(username);
CREATE INDEX IF NOT EXISTS users_updated_at_idx ON users(updated_at);
"#;

/// One row of the `users` table — the public shape returned by every
/// store method except [`UserStorage::list_summaries`] (which omits
/// the credential hash).
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub id:              Uuid,
    pub username:        String,
    pub credential_hash: String,
    pub auth_method:     AuthMethod,
    pub metadata:        JsonValue,
    pub created_at:      u64,
    pub updated_at:      u64,
    pub disabled:        bool,
}

/// Hash-free projection used by admin listings (`bdscmd user list`,
/// `v3/user.list`).  Omitting `credential_hash` is deliberate — admins
/// must never see the hash even by accident, and serialising into this
/// type makes that a compile-time check.
#[derive(Debug, Clone)]
pub struct UserSummary {
    pub id:           Uuid,
    pub username:     String,
    pub auth_method:  AuthMethod,
    pub metadata:     JsonValue,
    pub created_at:   u64,
    pub updated_at:   u64,
    pub disabled:     bool,
}

/// Partial-update payload for [`UserStorage::modify`].  Each `Some`
/// field overwrites; `None` leaves the row's existing value.
#[derive(Debug, Default, Clone)]
pub struct UserPatch {
    /// New raw credential — will be re-hashed via the registered
    /// verifier for the row's existing `auth_method`.  To change the
    /// auth method itself, set both `new_auth_method` and `credential`.
    pub credential:      Option<String>,
    pub new_auth_method: Option<AuthMethod>,
    pub metadata:        Option<JsonValue>,
    pub disabled:        Option<bool>,
}

/// Cluster-replicated user store.  Cheap to clone — the underlying
/// `StorageEngine` is `Arc`-backed.
#[derive(Clone)]
pub struct UserStorage {
    engine:    Arc<StorageEngine>,
    /// Verifier registry shared with the rest of the cluster layer.
    /// `Arc` so multiple stores (or test instances) can share one
    /// argon2 setup.
    verifiers: Arc<VerifierRegistry>,
    /// Filesystem path of the backing DuckDB file — recorded for
    /// debug log lines.
    #[allow(dead_code)]
    path:      PathBuf,
}

impl UserStorage {
    /// Open or create the user store at `<root>/users.duckdb`.  The
    /// caller is expected to pass a directory that already exists
    /// (typically `<dbpath>/users/` created by `Cluster::init`).
    pub fn open(root: &Path, verifiers: Arc<VerifierRegistry>) -> Result<Self> {
        std::fs::create_dir_all(root)
            .map_err(|e| err_msg(format!("user_store: create dir {root:?}: {e}")))?;
        let path = root.join("users.duckdb");
        let engine = StorageEngine::new(&path, INIT_SQL, 4)?;
        Ok(Self { engine: Arc::new(engine), verifiers, path })
    }

    /// True when the table has zero rows — used by the bdsweb open-
    /// access banner and by `v3/user.add`'s first-user bootstrap
    /// short-circuit (which lets the very first add through without
    /// HMAC).
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.count()? == 0)
    }

    /// Total row count.
    pub fn count(&self) -> Result<u64> {
        let rows = self.engine.select_all("SELECT COUNT(*) FROM users")?;
        Ok(rows.into_iter().next()
            .and_then(|r| r.into_iter().next())
            .and_then(|v| v.cast_int().ok())
            .map(|i| i.max(0) as u64)
            .unwrap_or(0))
    }

    /// Add a user.  `id` is caller-supplied so cluster replication can
    /// give every replica the same UUID.  `raw_credential` is the
    /// plaintext password (or whatever the caller verifier accepts —
    /// for OAuth it's the provider user-id we'll compare on each login).
    /// The hash is produced by the registered verifier for `method`;
    /// if no verifier is registered, the call fails — no silent
    /// fallback to a different scheme.
    ///
    /// Idempotent on UUID conflict: if the same `id` already exists,
    /// returns `Ok(())` and does nothing — matches the v2 receiver
    /// pattern used by docs/signals/scripts so hint replay can re-fire
    /// safely.  Username uniqueness is enforced by the SQL UNIQUE
    /// constraint; conflicting username on a fresh UUID returns Err.
    pub fn add(
        &self,
        id:             Uuid,
        username:       &str,
        raw_credential: &str,
        method:         AuthMethod,
        metadata:       JsonValue,
        now_secs:       u64,
    ) -> Result<()> {
        // Idempotent receiver — replication or hint replay can re-hit
        // this call with the same id; treat as no-op.
        if self.get(id)?.is_some() {
            return Ok(());
        }

        let verifier = self.verifiers.for_method(&method)
            .ok_or_else(|| err_msg(format!(
                "no credential verifier registered for {method:?}"
            )))?;
        let credential_hash = verifier.store(raw_credential)?;
        self.insert_raw(id, username, &credential_hash, method, metadata,
                        now_secs, now_secs, false)
    }

    /// Add a row with an already-hashed credential — used by the
    /// anti-entropy pull path to backfill a peer's exact stored
    /// credential without re-hashing (which would change the hash and
    /// break login compatibility for that user across nodes).
    /// Idempotent on UUID.  Caller is responsible for supplying a
    /// hash format the registered verifier can verify against.
    pub fn add_with_hash(
        &self,
        id:               Uuid,
        username:         &str,
        credential_hash:  &str,
        method:           AuthMethod,
        metadata:         JsonValue,
        created_at:       u64,
        updated_at:       u64,
        disabled:         bool,
    ) -> Result<()> {
        if self.get(id)?.is_some() {
            return Ok(());
        }
        self.insert_raw(id, username, credential_hash, method, metadata,
                        created_at, updated_at, disabled)
    }

    fn insert_raw(
        &self,
        id:              Uuid,
        username:        &str,
        credential_hash: &str,
        method:          AuthMethod,
        metadata:        JsonValue,
        created_at:      u64,
        updated_at:      u64,
        disabled:        bool,
    ) -> Result<()> {
        let metadata_s = serde_json::to_string(&metadata)
            .map_err(|e| err_msg(format!("metadata serialise: {e}")))?;

        let sql = format!(
            "INSERT INTO users \
               (id, username, credential_hash, auth_method, metadata, \
                created_at, updated_at, disabled) \
             VALUES ('{}', '{}', '{}', '{}', '{}', {}, {}, {})",
            id,
            sql_escape(username),
            sql_escape(credential_hash),
            sql_escape(&method.to_wire()),
            sql_escape(&metadata_s),
            created_at as i64,
            updated_at as i64,
            disabled,
        );
        self.engine.execute(&sql)
    }

    /// Apply a partial update.  `if_newer == true` — incoming
    /// `now_secs` must be strictly greater than the row's stored
    /// `updated_at`, otherwise the call is a no-op (this is the LWW
    /// hook used by anti-entropy to avoid clobbering concurrent
    /// edits).  `if_newer == false` overwrites unconditionally —
    /// reserved for the local coordinator path where the operator's
    /// intent IS the latest.
    pub fn modify(
        &self,
        id:        Uuid,
        patch:     &UserPatch,
        if_newer:  bool,
        now_secs:  u64,
    ) -> Result<()> {
        let existing = match self.get(id)? {
            Some(u) => u,
            None    => return Err(err_msg(format!("user {id} not found"))),
        };
        if if_newer && now_secs <= existing.updated_at {
            return Ok(());
        }

        // Resolve the post-update auth method first so we hash the new
        // credential under the right verifier.  Default: keep existing.
        let new_method = patch.new_auth_method.clone().unwrap_or_else(|| existing.auth_method.clone());
        let new_hash = match &patch.credential {
            Some(raw) => {
                let v = self.verifiers.for_method(&new_method)
                    .ok_or_else(|| err_msg(format!(
                        "no credential verifier registered for {new_method:?}"
                    )))?;
                v.store(raw)?
            }
            None => existing.credential_hash.clone(),
        };
        let new_meta = patch.metadata.clone().unwrap_or(existing.metadata.clone());
        let new_meta_s = serde_json::to_string(&new_meta)
            .map_err(|e| err_msg(format!("metadata serialise: {e}")))?;
        let new_disabled = patch.disabled.unwrap_or(existing.disabled);

        let sql = format!(
            "UPDATE users SET \
                credential_hash = '{}', \
                auth_method     = '{}', \
                metadata        = '{}', \
                disabled        = {}, \
                updated_at      = {} \
             WHERE id = '{}'",
            sql_escape(&new_hash),
            sql_escape(&new_method.to_wire()),
            sql_escape(&new_meta_s),
            new_disabled,
            now_secs as i64,
            id,
        );
        self.engine.execute(&sql)
    }

    /// Hard-delete the row.  The cluster coordinator (Phase 7.2) is
    /// responsible for writing a tombstone before delete-replicating
    /// to peers so AE doesn't resurrect this row from a stale peer.
    pub fn delete(&self, id: Uuid) -> Result<()> {
        let sql = format!("DELETE FROM users WHERE id = '{}'", id);
        self.engine.execute(&sql)
    }

    /// Fetch by UUID.  `Ok(None)` for unknown ids.
    pub fn get(&self, id: Uuid) -> Result<Option<UserRecord>> {
        let sql = format!(
            "SELECT id, username, credential_hash, auth_method, metadata, \
                    created_at, updated_at, disabled \
             FROM users WHERE id = '{}'",
            id,
        );
        let rows = self.engine.select_all(&sql)?;
        Ok(rows.into_iter().next().map(row_to_record).transpose()?.flatten())
    }

    /// Fetch by username.  `Ok(None)` for unknown usernames.  Used by
    /// the login path.
    pub fn get_by_username(&self, username: &str) -> Result<Option<UserRecord>> {
        let sql = format!(
            "SELECT id, username, credential_hash, auth_method, metadata, \
                    created_at, updated_at, disabled \
             FROM users WHERE username = '{}'",
            sql_escape(username),
        );
        let rows = self.engine.select_all(&sql)?;
        Ok(rows.into_iter().next().map(row_to_record).transpose()?.flatten())
    }

    /// Anti-entropy id list — used by the AE pull path to diff
    /// against peers.  Returns every UUID in the table, sorted.
    pub fn list_ids(&self) -> Result<Vec<Uuid>> {
        let rows = self.engine.select_all("SELECT id FROM users ORDER BY id ASC")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            if let Some(s) = r.into_iter().next().and_then(|v| v.cast_string().ok()) {
                if let Ok(uu) = Uuid::parse_str(&s) {
                    out.push(uu);
                }
            }
        }
        Ok(out)
    }

    /// Hash-free admin listing.  `v3/user.list` and the bdsweb user-
    /// management page consume this; nothing returned here is
    /// sensitive (the credential hash is dropped at the SQL boundary).
    pub fn list_summaries(&self) -> Result<Vec<UserSummary>> {
        let sql = "SELECT id, username, auth_method, metadata, \
                          created_at, updated_at, disabled \
                   FROM users ORDER BY username ASC";
        let rows = self.engine.select_all(sql)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id_s = r.first().and_then(|v| v.clone().cast_string().ok())
                .ok_or_else(|| err_msg("user row missing id"))?;
            let id   = Uuid::parse_str(&id_s)
                .map_err(|e| err_msg(format!("invalid user id {id_s:?}: {e}")))?;
            out.push(UserSummary {
                id,
                username:    r.get(1).and_then(|v| v.clone().cast_string().ok()).unwrap_or_default(),
                auth_method: AuthMethod::from_wire(
                    &r.get(2).and_then(|v| v.clone().cast_string().ok()).unwrap_or_default()
                ),
                metadata:    r.get(3).and_then(|v| v.clone().cast_string().ok())
                                .and_then(|s| serde_json::from_str(&s).ok())
                                .unwrap_or(JsonValue::Null),
                created_at:  r.get(4).and_then(|v| v.clone().cast_int().ok()).map(|i| i.max(0) as u64).unwrap_or(0),
                updated_at:  r.get(5).and_then(|v| v.clone().cast_int().ok()).map(|i| i.max(0) as u64).unwrap_or(0),
                disabled:    r.get(6).and_then(|v| v.clone().cast_bool().ok()).unwrap_or(false),
            });
        }
        Ok(out)
    }

    /// The login path: look up `username`, dispatch the row's
    /// `auth_method` to its registered verifier, and return the user
    /// record on success.  Returns:
    ///
    /// - `Ok(Some(user))` on successful authentication.
    /// - `Ok(None)` on unknown user, wrong password, disabled account,
    ///   or unsupported auth_method.  All four cases collapse to
    ///   "credentials don't work" — the caller should NOT distinguish
    ///   them in user-facing messages (information leak).
    /// - `Err(_)` only when the SQL layer or verifier itself errors
    ///   (e.g. argon2 hash is corrupt) — treat as a failed login at
    ///   the call site.
    pub fn verify(&self, username: &str, raw_credential: &str) -> Result<Option<UserRecord>> {
        let user = match self.get_by_username(username)? {
            Some(u) => u,
            None    => return Ok(None),
        };
        if user.disabled {
            return Ok(None);
        }
        let verifier = match self.verifiers.for_method(&user.auth_method) {
            Some(v) => v,
            None    => return Ok(None), // unknown method — refuse
        };
        if verifier.verify(&user.credential_hash, raw_credential)? {
            Ok(Some(user))
        } else {
            Ok(None)
        }
    }
}

/// Map a single SQL row from the `users` table to a `UserRecord`.
/// Wrapped to keep `get` / `get_by_username` slim.
fn row_to_record(row: Vec<rust_dynamic::value::Value>) -> Result<Option<UserRecord>> {
    if row.is_empty() {
        return Ok(None);
    }
    let id_s = row.first().and_then(|v| v.clone().cast_string().ok())
        .ok_or_else(|| err_msg("user row missing id column"))?;
    let id   = Uuid::parse_str(&id_s)
        .map_err(|e| err_msg(format!("invalid user id {id_s:?}: {e}")))?;
    let metadata_s = row.get(4).and_then(|v| v.clone().cast_string().ok()).unwrap_or_default();
    let metadata = if metadata_s.is_empty() {
        JsonValue::Null
    } else {
        serde_json::from_str(&metadata_s)
            .map_err(|e| err_msg(format!("metadata parse: {e}")))?
    };
    Ok(Some(UserRecord {
        id,
        username:        row.get(1).and_then(|v| v.clone().cast_string().ok()).unwrap_or_default(),
        credential_hash: row.get(2).and_then(|v| v.clone().cast_string().ok()).unwrap_or_default(),
        auth_method:     AuthMethod::from_wire(
            &row.get(3).and_then(|v| v.clone().cast_string().ok()).unwrap_or_default()
        ),
        metadata,
        created_at:      row.get(5).and_then(|v| v.clone().cast_int().ok()).map(|i| i.max(0) as u64).unwrap_or(0),
        updated_at:      row.get(6).and_then(|v| v.clone().cast_int().ok()).map(|i| i.max(0) as u64).unwrap_or(0),
        disabled:        row.get(7).and_then(|v| v.clone().cast_bool().ok()).unwrap_or(false),
    }))
}

/// SQL string-literal escape — single quotes only.  We do NOT use
/// prepared statements here because StorageEngine doesn't expose them
/// for arbitrary text; the rest of the codebase uses the same pattern
/// (see `cluster::hints::sql_escape`).
fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn open_store() -> (TempDir, UserStorage) {
        let tmp = TempDir::new().unwrap();
        let reg = Arc::new(VerifierRegistry::default());
        let store = UserStorage::open(tmp.path(), reg).unwrap();
        (tmp, store)
    }

    #[test]
    fn fresh_store_is_empty() {
        let (_tmp, store) = open_store();
        assert!(store.is_empty().unwrap());
        assert_eq!(store.count().unwrap(), 0);
        assert!(store.list_ids().unwrap().is_empty());
    }

    #[test]
    fn add_then_get_round_trips() {
        let (_tmp, store) = open_store();
        let id = Uuid::now_v7();
        store.add(id, "alice", "hunter2", AuthMethod::Password,
                  json!({"display_name": "Alice"}), 1000).unwrap();
        let u = store.get(id).unwrap().expect("alice present");
        assert_eq!(u.username, "alice");
        assert_eq!(u.auth_method, AuthMethod::Password);
        assert_eq!(u.metadata["display_name"], "Alice");
        assert_eq!(u.created_at, 1000);
        assert_eq!(u.updated_at, 1000);
        assert!(!u.disabled);
        assert!(u.credential_hash.starts_with("$argon2id"),
            "credential_hash should be argon2 PHC string, got {:?}", u.credential_hash);
    }

    #[test]
    fn add_is_idempotent_on_uuid() {
        let (_tmp, store) = open_store();
        let id = Uuid::now_v7();
        store.add(id, "alice", "hunter2", AuthMethod::Password, json!({}), 1000).unwrap();
        // Second add with same id is a no-op (matches receiver semantics
        // for replication / hint replay).
        store.add(id, "alice", "different", AuthMethod::Password, json!({}), 2000).unwrap();
        let u = store.get(id).unwrap().unwrap();
        assert_eq!(u.created_at, 1000, "created_at unchanged");
        assert_eq!(u.updated_at, 1000, "updated_at unchanged");
    }

    #[test]
    fn add_rejects_duplicate_username_with_fresh_uuid() {
        let (_tmp, store) = open_store();
        store.add(Uuid::now_v7(), "alice", "p", AuthMethod::Password, json!({}), 1).unwrap();
        let res = store.add(Uuid::now_v7(), "alice", "p", AuthMethod::Password, json!({}), 2);
        assert!(res.is_err(), "duplicate username should error: {res:?}");
    }

    #[test]
    fn verify_succeeds_for_correct_password() {
        let (_tmp, store) = open_store();
        store.add(Uuid::now_v7(), "alice", "hunter2",
                  AuthMethod::Password, json!({}), 1).unwrap();
        let u = store.verify("alice", "hunter2").unwrap().expect("alice present");
        assert_eq!(u.username, "alice");
    }

    #[test]
    fn verify_returns_none_for_wrong_password() {
        let (_tmp, store) = open_store();
        store.add(Uuid::now_v7(), "alice", "hunter2",
                  AuthMethod::Password, json!({}), 1).unwrap();
        assert!(store.verify("alice", "hunter3").unwrap().is_none());
    }

    #[test]
    fn verify_returns_none_for_unknown_user() {
        let (_tmp, store) = open_store();
        assert!(store.verify("ghost", "anything").unwrap().is_none());
    }

    #[test]
    fn verify_returns_none_for_disabled_user() {
        let (_tmp, store) = open_store();
        let id = Uuid::now_v7();
        store.add(id, "alice", "p", AuthMethod::Password, json!({}), 1).unwrap();
        store.modify(id, &UserPatch { disabled: Some(true), ..Default::default() }, false, 2).unwrap();
        assert!(store.verify("alice", "p").unwrap().is_none(),
            "disabled user must NOT authenticate even with the right password");
    }

    #[test]
    fn modify_password_replaces_hash() {
        let (_tmp, store) = open_store();
        let id = Uuid::now_v7();
        store.add(id, "alice", "old", AuthMethod::Password, json!({}), 1).unwrap();
        store.modify(id, &UserPatch { credential: Some("new".into()), ..Default::default() }, false, 2).unwrap();
        assert!(store.verify("alice", "old").unwrap().is_none(), "old password no longer works");
        assert!(store.verify("alice", "new").unwrap().is_some(), "new password works");
    }

    #[test]
    fn modify_if_newer_skips_when_not_newer() {
        let (_tmp, store) = open_store();
        let id = Uuid::now_v7();
        store.add(id, "alice", "p", AuthMethod::Password, json!({"v": 1}), 100).unwrap();
        // Stale incoming update at ts <= existing — must be a no-op.
        store.modify(id, &UserPatch { metadata: Some(json!({"v": 2})), ..Default::default() }, true, 100).unwrap();
        let u = store.get(id).unwrap().unwrap();
        assert_eq!(u.metadata["v"], 1, "stale update must NOT clobber");
        assert_eq!(u.updated_at, 100);
    }

    #[test]
    fn modify_if_newer_applies_when_strictly_newer() {
        let (_tmp, store) = open_store();
        let id = Uuid::now_v7();
        store.add(id, "alice", "p", AuthMethod::Password, json!({"v": 1}), 100).unwrap();
        store.modify(id, &UserPatch { metadata: Some(json!({"v": 2})), ..Default::default() }, true, 200).unwrap();
        let u = store.get(id).unwrap().unwrap();
        assert_eq!(u.metadata["v"], 2);
        assert_eq!(u.updated_at, 200);
    }

    #[test]
    fn delete_removes_row() {
        let (_tmp, store) = open_store();
        let id = Uuid::now_v7();
        store.add(id, "alice", "p", AuthMethod::Password, json!({}), 1).unwrap();
        store.delete(id).unwrap();
        assert!(store.get(id).unwrap().is_none());
        assert!(store.is_empty().unwrap());
    }

    #[test]
    fn list_ids_and_summaries_omit_hash() {
        let (_tmp, store) = open_store();
        store.add(Uuid::now_v7(), "alice", "p", AuthMethod::Password, json!({}), 1).unwrap();
        store.add(Uuid::now_v7(), "bob",   "p", AuthMethod::Password, json!({}), 2).unwrap();
        assert_eq!(store.list_ids().unwrap().len(), 2);

        let sums = store.list_summaries().unwrap();
        assert_eq!(sums.len(), 2);
        // UserSummary has no credential_hash field — compile-time guarantee
        // that admin listings can't accidentally leak hashes.  Just verify
        // ordering by username.
        assert_eq!(sums[0].username, "alice");
        assert_eq!(sums[1].username, "bob");
    }
}
