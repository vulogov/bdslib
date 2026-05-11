//! Cluster-replicated inference cache.
//!
//! Lives at `<dbpath>/llm/cache.duckdb` — a 5th fully-replicated store
//! alongside docs / signals / scripts / users.  Schema:
//!
//! ```sql
//! CREATE TABLE inference_cache (
//!     id            TEXT    PRIMARY KEY,        -- UUIDv7 (for AE)
//!     cache_key     TEXT    UNIQUE NOT NULL,    -- sha256 hex of canonical request
//!     provider      TEXT    NOT NULL,
//!     model         TEXT    NOT NULL,
//!     kind          TEXT    NOT NULL,           -- "complete" | "chat" | "analyze:rca" | …
//!     request_json  JSON    NOT NULL,           -- redacted canonical request
//!     response_json JSON    NOT NULL,           -- {text, tokens_in?, tokens_out?, finish_reason?}
//!     source_meta   JSON,                       -- ContextSource snapshot or null
//!     created_at    BIGINT  NOT NULL,
//!     expires_at    BIGINT  NOT NULL,           -- LWW horizon
//!     updated_at    BIGINT  NOT NULL,           -- LWW key for AE
//!     hits          BIGINT  NOT NULL DEFAULT 0
//! );
//! ```
//!
//! Lookups are by `cache_key` (the content hash); AE walks `id`
//! (UUIDv7) so the existing antientropy machinery in
//! `bdsnode/server/cluster.rs` works unchanged once the cache is
//! plumbed in as a known store name.
//!
//! Replication / dedup / API integration land in Phase 3.b and 3.c —
//! this module is just the per-node SQL layer.

use crate::common::error::{err_msg, Result};
use crate::storageengine::StorageEngine;
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS inference_cache (
    id            TEXT   PRIMARY KEY,
    cache_key     TEXT   UNIQUE NOT NULL,
    provider      TEXT   NOT NULL,
    model         TEXT   NOT NULL,
    kind          TEXT   NOT NULL,
    request_json  TEXT   NOT NULL,
    response_json TEXT   NOT NULL,
    source_meta   TEXT,
    created_at    BIGINT NOT NULL,
    expires_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL,
    hits          BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS inference_cache_key_idx     ON inference_cache(cache_key);
CREATE INDEX IF NOT EXISTS inference_cache_expires_idx ON inference_cache(expires_at);
CREATE INDEX IF NOT EXISTS inference_cache_updated_idx ON inference_cache(updated_at);
"#;

/// Fields that get rewritten to `"***"` before the request_json reaches
/// disk — keeps API keys and HMAC signatures out of the replicated DB.
/// Match is case-insensitive on the key name (not the value).
const REDACTED_FIELDS: &[&str] = &["api_key", "_hmac", "authorization", "secret"];

/// One row of the cache table, after deserialisation.
#[derive(Debug, Clone)]
pub struct CachedEntry {
    pub id:            Uuid,
    pub cache_key:     String,
    pub provider:      String,
    pub model:         String,
    pub kind:          String,
    pub request_json:  JsonValue,
    pub response_json: JsonValue,
    pub source_meta:   Option<JsonValue>,
    pub created_at:    u64,
    pub expires_at:    u64,
    pub updated_at:    u64,
    pub hits:          u64,
}

/// A new entry to insert.  `id` is caller-supplied so cluster
/// replication can give every replica the same UUID (matches the
/// users / docs / signals pattern).
#[derive(Debug, Clone)]
pub struct CacheInsert {
    pub id:            Uuid,
    pub cache_key:     String,
    pub provider:      String,
    pub model:         String,
    pub kind:          String,
    pub request_json:  JsonValue,
    pub response_json: JsonValue,
    pub source_meta:   Option<JsonValue>,
    pub created_at:    u64,
    pub expires_at:    u64,
}

/// Aggregate counters for `v4/llm.cache.stats`.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub rows:        u64,
    pub total_hits:  u64,
    pub bytes_rough: u64,  // sum(length(response_json)); cheap approximation
}

#[derive(Clone)]
pub struct InferenceCache {
    engine: Arc<StorageEngine>,
    #[allow(dead_code)]
    path:   PathBuf,
}

impl InferenceCache {
    /// Open or create the cache at `<root>/cache.duckdb`.  Caller passes
    /// a directory that already exists (typically `<dbpath>/llm/` —
    /// `Cluster::init` will mkdir it once Phase 3.c lands).
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .map_err(|e| err_msg(format!("llm::cache: create dir {root:?}: {e}")))?;
        let path = root.join("cache.duckdb");
        let engine = StorageEngine::new(&path, INIT_SQL, 4)?;
        Ok(Self { engine: Arc::new(engine), path })
    }

    pub fn count(&self) -> Result<u64> {
        let rows = self.engine.select_all("SELECT COUNT(*) FROM inference_cache")?;
        Ok(rows.into_iter().next()
            .and_then(|r| r.into_iter().next())
            .and_then(|v| v.cast_int().ok())
            .map(|i| i.max(0) as u64)
            .unwrap_or(0))
    }

    /// Insert a new cache entry.  Idempotent on `id` — replication or
    /// hint replay can re-fire safely, matching the users / docs
    /// receiver pattern.  Conflicting `cache_key` on a new id is also
    /// silently accepted (treated as "already cached" — we keep the
    /// older row).
    pub fn put(&self, entry: CacheInsert) -> Result<()> {
        if self.get_by_id(entry.id)?.is_some() {
            return Ok(());
        }
        if self.get_by_key(&entry.cache_key)?.is_some() {
            return Ok(());
        }
        let request_s = serde_json::to_string(&redact_request(&entry.request_json))
            .map_err(|e| err_msg(format!("llm::cache: request serialise: {e}")))?;
        let response_s = serde_json::to_string(&entry.response_json)
            .map_err(|e| err_msg(format!("llm::cache: response serialise: {e}")))?;
        let source_s = match &entry.source_meta {
            Some(v) => format!("'{}'", sql_escape(&serde_json::to_string(v)
                .map_err(|e| err_msg(format!("llm::cache: source_meta serialise: {e}")))?)),
            None => "NULL".to_owned(),
        };
        let now = now_secs();
        let sql = format!(
            "INSERT INTO inference_cache \
              (id, cache_key, provider, model, kind, request_json, response_json, \
               source_meta, created_at, expires_at, updated_at, hits) \
             VALUES ('{id}', '{key}', '{provider}', '{model}', '{kind}', \
                     '{req}', '{resp}', {src}, {created}, {expires}, {updated}, 0)",
            id       = entry.id,
            key      = sql_escape(&entry.cache_key),
            provider = sql_escape(&entry.provider),
            model    = sql_escape(&entry.model),
            kind     = sql_escape(&entry.kind),
            req      = sql_escape(&request_s),
            resp     = sql_escape(&response_s),
            src      = source_s,
            created  = entry.created_at as i64,
            expires  = entry.expires_at as i64,
            updated  = now as i64,
        );
        self.engine.execute(&sql)
    }

    /// Get a cached entry by its content-derived cache_key.  Returns
    /// `None` when no row matches OR when the matching row has already
    /// expired (lazy expiry; the sweeper handles row removal).
    pub fn get_by_key(&self, cache_key: &str) -> Result<Option<CachedEntry>> {
        let sql = format!(
            "SELECT id, cache_key, provider, model, kind, request_json, response_json, \
                    source_meta, created_at, expires_at, updated_at, hits \
             FROM inference_cache WHERE cache_key = '{}'",
            sql_escape(cache_key)
        );
        let mut rows = self.engine.select_all(&sql)?;
        let row = match rows.pop() {
            Some(r) => r,
            None    => return Ok(None),
        };
        let entry = row_to_entry(row)?;
        if entry.expires_at != 0 && entry.expires_at <= now_secs() {
            return Ok(None);
        }
        Ok(Some(entry))
    }

    /// Get a cached entry by its UUID — used by the anti-entropy
    /// pull_one path.
    pub fn get_by_id(&self, id: Uuid) -> Result<Option<CachedEntry>> {
        let sql = format!(
            "SELECT id, cache_key, provider, model, kind, request_json, response_json, \
                    source_meta, created_at, expires_at, updated_at, hits \
             FROM inference_cache WHERE id = '{id}'"
        );
        let mut rows = self.engine.select_all(&sql)?;
        match rows.pop() {
            Some(r) => Ok(Some(row_to_entry(r)?)),
            None    => Ok(None),
        }
    }

    /// Increment the `hits` counter for a row.  Best-effort: failures
    /// are logged-and-swallowed by the caller (the cached response was
    /// already returned to the user successfully).
    pub fn bump_hits(&self, id: Uuid) -> Result<()> {
        let sql = format!(
            "UPDATE inference_cache SET hits = hits + 1 WHERE id = '{id}'"
        );
        self.engine.execute(&sql)
    }

    /// Hard-delete a row by id.  Tombstone bookkeeping is the caller's
    /// responsibility (we go through `cluster::tombstones` at the
    /// coordinator surface, not here).
    pub fn delete(&self, id: Uuid) -> Result<()> {
        let sql = format!("DELETE FROM inference_cache WHERE id = '{id}'");
        self.engine.execute(&sql)
    }

    /// Drop every row whose `expires_at` is in the past.  Returns the
    /// number of deleted rows.  Safe to call on a hot cache.
    pub fn purge_expired(&self) -> Result<u64> {
        let now = now_secs() as i64;
        let before = self.count()?;
        let sql = format!(
            "DELETE FROM inference_cache WHERE expires_at != 0 AND expires_at <= {now}"
        );
        self.engine.execute(&sql)?;
        let after = self.count()?;
        Ok(before.saturating_sub(after))
    }

    /// Hard-delete rows matching the optional filters.  Empty filter
    /// set deletes the whole cache.  Returns the number of deleted
    /// rows.  Used by `v4/llm.cache.purge`.
    pub fn purge(&self, filter: PurgeFilter) -> Result<u64> {
        let mut clauses: Vec<String> = Vec::new();
        if let Some(ts) = filter.older_than_created {
            clauses.push(format!("created_at < {}", ts as i64));
        }
        if let Some(p) = filter.provider {
            clauses.push(format!("provider = '{}'", sql_escape(&p)));
        }
        if let Some(k) = filter.kind {
            clauses.push(format!("kind = '{}'", sql_escape(&k)));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let before = self.count()?;
        self.engine.execute(&format!("DELETE FROM inference_cache{where_clause}"))?;
        let after = self.count()?;
        Ok(before.saturating_sub(after))
    }

    /// IDs + `updated_at` for every live row.  Mirrors the shape the
    /// existing `v2/<store>.list_ids` receivers return so the AE
    /// machinery in `bdsnode/server/cluster.rs` can sweep this store
    /// alongside docs / signals / scripts / users.
    pub fn list_ids(&self) -> Result<Vec<(Uuid, u64)>> {
        let rows = self.engine.select_all(
            "SELECT id, updated_at FROM inference_cache ORDER BY updated_at"
        )?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let mut it = r.into_iter();
            let id_v   = it.next().ok_or_else(|| err_msg("list_ids: missing id"))?;
            let ts_v   = it.next().ok_or_else(|| err_msg("list_ids: missing updated_at"))?;
            let id_s = id_v.cast_string().map_err(|e| err_msg(format!("list_ids: id cast: {e}")))?;
            let id   = Uuid::parse_str(&id_s).map_err(|e| err_msg(format!("list_ids: id parse: {e}")))?;
            let ts   = ts_v.cast_int().map_err(|e| err_msg(format!("list_ids: ts cast: {e}")))?
                          .max(0) as u64;
            out.push((id, ts));
        }
        Ok(out)
    }

    pub fn stats(&self) -> Result<CacheStats> {
        // SUM(BIGINT) returns HUGEINT in DuckDB; cast back to BIGINT so
        // rust_dynamic's cast_int can read it.
        let rows = self.engine.select_all(
            "SELECT COUNT(*), \
                    CAST(COALESCE(SUM(hits),0) AS BIGINT), \
                    CAST(COALESCE(SUM(LENGTH(response_json)),0) AS BIGINT) \
             FROM inference_cache"
        )?;
        let row = match rows.into_iter().next() {
            Some(r) => r,
            None    => return Ok(CacheStats::default()),
        };
        let mut it = row.into_iter();
        let rows_v  = it.next().and_then(|v| v.cast_int().ok()).unwrap_or(0).max(0) as u64;
        let hits_v  = it.next().and_then(|v| v.cast_int().ok()).unwrap_or(0).max(0) as u64;
        let bytes_v = it.next().and_then(|v| v.cast_int().ok()).unwrap_or(0).max(0) as u64;
        Ok(CacheStats { rows: rows_v, total_hits: hits_v, bytes_rough: bytes_v })
    }
}

/// Filters for [`InferenceCache::purge`].  All are optional and ANDed
/// together; passing `Default::default()` purges everything.
#[derive(Debug, Clone, Default)]
pub struct PurgeFilter {
    pub older_than_created: Option<u64>,  // unix seconds — rows strictly older
    pub provider:           Option<String>,
    pub kind:               Option<String>,
}

// ─────────────────────────────────────────────────────────────────────
// Process-wide CacheManager
// ─────────────────────────────────────────────────────────────────────

/// Wraps an [`InferenceCache`] together with the runtime `enabled` /
/// `ttl_secs` knobs from `bds.hjson`.  Stored process-wide via
/// [`init`] and looked up by `vm::api::llm::*` helpers.
pub struct CacheManager {
    cache:    InferenceCache,
    enabled:  bool,
    ttl_secs: u64,
}

impl CacheManager {
    pub fn new(cache: InferenceCache, enabled: bool, ttl_secs: u64) -> Self {
        Self { cache, enabled, ttl_secs }
    }

    pub fn cache(&self)    -> &InferenceCache { &self.cache }
    pub fn enabled(&self)  -> bool            { self.enabled }
    pub fn ttl_secs(&self) -> u64             { self.ttl_secs }

    /// Compute the absolute `expires_at` for an entry minted now.
    /// `ttl_secs == 0` is "never expires" — returns 0.
    pub fn expires_at_for_now(&self) -> u64 {
        if self.ttl_secs == 0 { 0 } else { now_secs().saturating_add(self.ttl_secs) }
    }
}

static GLOBAL: OnceLock<CacheManager> = OnceLock::new();

/// First-write-wins initialisation, mirroring `manager::init` for
/// providers.  Safe to call multiple times.
pub fn init(manager: CacheManager) {
    let _ = GLOBAL.set(manager);
}

/// Process-wide cache manager — `None` until [`init`] has been called.
pub fn manager() -> Option<&'static CacheManager> {
    GLOBAL.get()
}

// ─────────────────────────────────────────────────────────────────────
// Cache key derivation
// ─────────────────────────────────────────────────────────────────────

/// Compute the cache key for a canonical request.
///
/// `request` should be a JSON object the caller has already populated
/// with whatever fields make this request *semantically equivalent* to
/// another (provider, model, messages, options, context fingerprints).
/// The function redacts sensitive fields, serialises with sorted keys
/// (the default behaviour of serde_json::Map without `preserve_order`),
/// and returns the hex SHA-256.
///
/// Two callers building the SAME canonical request → SAME key.  Order
/// of insertion into the input Map doesn't matter — serde_json sorts
/// nested objects.
pub fn cache_key(request: &JsonValue) -> String {
    let redacted = redact_request(request);
    let canonical = serde_json::to_string(&redacted)
        .unwrap_or_else(|_| String::new());
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

/// Recursively replace values under sensitive keys with `"***"`.
/// Returns a deep-cloned tree so the input is not mutated.
pub fn redact_request(v: &JsonValue) -> JsonValue {
    match v {
        JsonValue::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, child) in map {
                let kl = k.to_lowercase();
                if REDACTED_FIELDS.iter().any(|f| *f == kl) {
                    out.insert(k.clone(), json!("***"));
                } else {
                    out.insert(k.clone(), redact_request(child));
                }
            }
            JsonValue::Object(out)
        }
        JsonValue::Array(arr) => {
            JsonValue::Array(arr.iter().map(redact_request).collect())
        }
        _ => v.clone(),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Row → CachedEntry plumbing
// ─────────────────────────────────────────────────────────────────────

fn row_to_entry(row: Vec<rust_dynamic::value::Value>) -> Result<CachedEntry> {
    let mut it = row.into_iter();
    let id_s = it.next().ok_or_else(|| err_msg("row missing id"))?
                .cast_string().map_err(|e| err_msg(format!("id cast: {e}")))?;
    let id = Uuid::parse_str(&id_s).map_err(|e| err_msg(format!("id parse: {e}")))?;
    let key      = it.next().ok_or_else(|| err_msg("missing cache_key"))?
                     .cast_string().map_err(|e| err_msg(format!("cache_key cast: {e}")))?;
    let provider = it.next().ok_or_else(|| err_msg("missing provider"))?
                     .cast_string().map_err(|e| err_msg(format!("provider cast: {e}")))?;
    let model    = it.next().ok_or_else(|| err_msg("missing model"))?
                     .cast_string().map_err(|e| err_msg(format!("model cast: {e}")))?;
    let kind     = it.next().ok_or_else(|| err_msg("missing kind"))?
                     .cast_string().map_err(|e| err_msg(format!("kind cast: {e}")))?;
    let request_s  = it.next().ok_or_else(|| err_msg("missing request_json"))?
                       .cast_string().map_err(|e| err_msg(format!("request_json cast: {e}")))?;
    let response_s = it.next().ok_or_else(|| err_msg("missing response_json"))?
                       .cast_string().map_err(|e| err_msg(format!("response_json cast: {e}")))?;
    let source_meta = match it.next() {
        Some(v) if matches!(v.data, rust_dynamic::types::Val::Null) => None,
        Some(v) => {
            let s = v.cast_string().map_err(|e| err_msg(format!("source_meta cast: {e}")))?;
            if s.is_empty() { None } else {
                Some(serde_json::from_str(&s)
                    .map_err(|e| err_msg(format!("source_meta parse: {e}")))?)
            }
        }
        None => None,
    };
    let created    = it.next().ok_or_else(|| err_msg("missing created_at"))?
                       .cast_int().map_err(|e| err_msg(format!("created_at cast: {e}")))?;
    let expires    = it.next().ok_or_else(|| err_msg("missing expires_at"))?
                       .cast_int().map_err(|e| err_msg(format!("expires_at cast: {e}")))?;
    let updated    = it.next().ok_or_else(|| err_msg("missing updated_at"))?
                       .cast_int().map_err(|e| err_msg(format!("updated_at cast: {e}")))?;
    let hits       = it.next().ok_or_else(|| err_msg("missing hits"))?
                       .cast_int().map_err(|e| err_msg(format!("hits cast: {e}")))?;
    let request_json: JsonValue = serde_json::from_str(&request_s)
        .map_err(|e| err_msg(format!("request_json parse: {e}")))?;
    let response_json: JsonValue = serde_json::from_str(&response_s)
        .map_err(|e| err_msg(format!("response_json parse: {e}")))?;
    Ok(CachedEntry {
        id,
        cache_key:    key,
        provider,
        model,
        kind,
        request_json,
        response_json,
        source_meta,
        created_at:   created.max(0) as u64,
        expires_at:   expires.max(0) as u64,
        updated_at:   updated.max(0) as u64,
        hits:         hits.max(0) as u64,
    })
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

    fn open_cache() -> (TempDir, InferenceCache) {
        let tmp = TempDir::new().unwrap();
        let cache = InferenceCache::open(tmp.path()).unwrap();
        (tmp, cache)
    }

    fn make_insert(key: &str, kind: &str, expires_at: u64) -> CacheInsert {
        CacheInsert {
            id:            Uuid::now_v7(),
            cache_key:     key.to_owned(),
            provider:      "ollama".to_owned(),
            model:         "llama3.2".to_owned(),
            kind:          kind.to_owned(),
            request_json:  json!({"messages": [{"role": "user", "content": "hi"}]}),
            response_json: json!({"text": "hello back", "tokens_in": 5, "tokens_out": 2}),
            source_meta:   Some(json!({"kind": "supplied", "row_count": 0})),
            created_at:    now_secs(),
            expires_at,
        }
    }

    #[test]
    fn put_and_get_by_key_round_trip() {
        let (_tmp, c) = open_cache();
        let entry = make_insert("abc123", "complete", 0);
        c.put(entry.clone()).unwrap();

        let got = c.get_by_key("abc123").unwrap().expect("hit");
        assert_eq!(got.id, entry.id);
        assert_eq!(got.provider, "ollama");
        assert_eq!(got.model,    "llama3.2");
        assert_eq!(got.kind,     "complete");
        assert_eq!(got.response_json["text"], json!("hello back"));
        assert_eq!(got.source_meta.unwrap()["kind"], json!("supplied"));
        assert_eq!(got.hits, 0);
    }

    #[test]
    fn put_is_idempotent_on_same_id() {
        let (_tmp, c) = open_cache();
        let e = make_insert("idem1", "complete", 0);
        c.put(e.clone()).unwrap();
        c.put(e.clone()).unwrap();  // second put — no error, no duplicate
        assert_eq!(c.count().unwrap(), 1);
    }

    #[test]
    fn put_with_duplicate_cache_key_silently_ignored() {
        let (_tmp, c) = open_cache();
        let mut e1 = make_insert("dup-key", "complete", 0);
        c.put(e1.clone()).unwrap();
        // Different UUID, same cache_key (rare but possible during a
        // narrow race window between two replicas computing the same
        // result concurrently).  Receiver keeps the earlier row.
        e1.id = Uuid::now_v7();
        c.put(e1).unwrap();
        assert_eq!(c.count().unwrap(), 1);
    }

    #[test]
    fn expired_rows_are_invisible_to_get_by_key() {
        let (_tmp, c) = open_cache();
        let stale = make_insert("stale-key", "complete", now_secs() - 60);
        c.put(stale).unwrap();
        assert!(c.get_by_key("stale-key").unwrap().is_none(),
            "expired row should not be returned");
        // ...but a get_by_id still finds it (used by AE / debug paths).
        let listed = c.list_ids().unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[test]
    fn expires_zero_means_never_expires() {
        let (_tmp, c) = open_cache();
        c.put(make_insert("forever", "complete", 0)).unwrap();
        assert!(c.get_by_key("forever").unwrap().is_some());
    }

    #[test]
    fn bump_hits_increments_counter() {
        let (_tmp, c) = open_cache();
        let e = make_insert("hot", "complete", 0);
        let id = e.id;
        c.put(e).unwrap();
        c.bump_hits(id).unwrap();
        c.bump_hits(id).unwrap();
        assert_eq!(c.get_by_id(id).unwrap().unwrap().hits, 2);
    }

    #[test]
    fn purge_expired_drops_only_past_due_rows() {
        let (_tmp, c) = open_cache();
        c.put(make_insert("k-live",  "complete", now_secs() + 3600)).unwrap();
        c.put(make_insert("k-stale", "complete", now_secs() - 60)).unwrap();
        c.put(make_insert("k-forever","complete", 0)).unwrap();
        let dropped = c.purge_expired().unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(c.count().unwrap(), 2);
        assert!(c.get_by_key("k-live").unwrap().is_some());
        assert!(c.get_by_key("k-forever").unwrap().is_some());
    }

    #[test]
    fn purge_with_provider_filter_only_drops_matching() {
        let (_tmp, c) = open_cache();
        let mut a = make_insert("a", "complete", 0);
        let mut b = make_insert("b", "complete", 0);
        a.provider = "ollama".into();
        b.provider = "openai".into();
        c.put(a).unwrap(); c.put(b).unwrap();
        let dropped = c.purge(PurgeFilter {
            provider: Some("openai".into()), ..Default::default()
        }).unwrap();
        assert_eq!(dropped, 1);
        assert!(c.get_by_key("a").unwrap().is_some());
        assert!(c.get_by_key("b").unwrap().is_none());
    }

    #[test]
    fn purge_empty_filter_drops_everything() {
        let (_tmp, c) = open_cache();
        c.put(make_insert("a", "complete", 0)).unwrap();
        c.put(make_insert("b", "complete", 0)).unwrap();
        let dropped = c.purge(PurgeFilter::default()).unwrap();
        assert_eq!(dropped, 2);
        assert_eq!(c.count().unwrap(), 0);
    }

    #[test]
    fn delete_removes_row_and_returns_no_error_for_missing_id() {
        let (_tmp, c) = open_cache();
        let e = make_insert("k", "complete", 0);
        let id = e.id;
        c.put(e).unwrap();
        c.delete(id).unwrap();
        assert!(c.get_by_id(id).unwrap().is_none());
        // Idempotent — deleting again is fine.
        c.delete(id).unwrap();
    }

    #[test]
    fn list_ids_returns_uuids_and_updated_ats() {
        let (_tmp, c) = open_cache();
        c.put(make_insert("a", "complete", 0)).unwrap();
        c.put(make_insert("b", "complete", 0)).unwrap();
        let ids = c.list_ids().unwrap();
        assert_eq!(ids.len(), 2);
        for (_uuid, ts) in &ids {
            assert!(*ts > 0, "updated_at should be set");
        }
    }

    #[test]
    fn stats_counts_rows_and_hits_and_response_bytes() {
        let (_tmp, c) = open_cache();
        let e = make_insert("k", "complete", 0);
        let id = e.id;
        c.put(e).unwrap();
        c.bump_hits(id).unwrap();
        let s = c.stats().unwrap();
        assert_eq!(s.rows, 1);
        assert_eq!(s.total_hits, 1);
        assert!(s.bytes_rough > 0, "response_json length should contribute");
    }

    #[test]
    fn cache_key_is_stable_across_field_order() {
        let a = json!({"provider": "ollama", "model": "llama3.2",
                       "messages": [{"role":"user","content":"hi"}]});
        let b = json!({"messages": [{"role":"user","content":"hi"}],
                       "model": "llama3.2", "provider": "ollama"});
        assert_eq!(cache_key(&a), cache_key(&b),
            "object key order must not affect the cache key");
    }

    #[test]
    fn cache_key_changes_with_payload() {
        let a = json!({"provider": "ollama", "messages": [{"role":"user","content":"hi"}]});
        let b = json!({"provider": "ollama", "messages": [{"role":"user","content":"bye"}]});
        assert_ne!(cache_key(&a), cache_key(&b));
    }

    #[test]
    fn redact_request_replaces_sensitive_keys_at_any_depth() {
        let v = json!({
            "provider": "ollama",
            "api_key":  "secret-1",
            "nested":   { "_hmac": "abc", "Authorization": "Bearer x", "ok": "stay" },
            "list":     [ { "secret": "s", "model": "m" } ],
        });
        let r = redact_request(&v);
        assert_eq!(r["provider"],         json!("ollama"));
        assert_eq!(r["api_key"],          json!("***"));
        assert_eq!(r["nested"]["_hmac"],  json!("***"));
        // Case-insensitive on the key NAME.
        assert_eq!(r["nested"]["Authorization"], json!("***"));
        assert_eq!(r["nested"]["ok"],     json!("stay"));
        assert_eq!(r["list"][0]["secret"], json!("***"));
        assert_eq!(r["list"][0]["model"],  json!("m"));
    }

    #[test]
    fn cache_key_redacts_before_hashing_so_secret_changes_dont_invalidate() {
        let a = json!({"provider": "openai", "api_key": "sk-aaaa", "prompt": "hi"});
        let b = json!({"provider": "openai", "api_key": "sk-bbbb", "prompt": "hi"});
        assert_eq!(cache_key(&a), cache_key(&b),
            "rotating an api_key should not invalidate the cache");
    }
}
