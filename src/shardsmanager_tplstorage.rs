//! Template-storage helpers on [`ShardsManager`].
//!
//! Templates are stored inside the per-shard [`DocumentStorage`] at
//! `{shard_path}/tplstorage`, making them time-partitioned the same way
//! telemetry records are.
//!
//! ## Routing
//!
//! **Writes** (`tpl_add`, `tpl_update_*`) require the template metadata to
//! contain a numeric `"timestamp"` field (Unix seconds).  The timestamp is
//! used to route the record to the correct [`Shard`] via [`ShardsCache`],
//! matching the behaviour of [`ShardsManager::add`].
//!
//! **Point reads / deletes** (`tpl_get_*`, `tpl_delete`) scan all registered
//! shards in catalog order until the record is found (equivalent to
//! [`ShardsManager::delete_by_id`]).
//!
//! **Range queries** (`tpl_list`, `tpl_search_text`, `tpl_search_json`,
//! `tpl_reindex`) accept a `duration` lookback window and query only the
//! shards that overlap `[now − duration, now]`, mirroring
//! [`ShardsManager::search_fts`] and related methods.

use crate::common::error::{err_msg, Result};
use crate::shardsmanager::ShardsManager;
use serde_json::Value as JsonValue;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

// ── helpers ───────────────────────────────────────────────────────────────────

fn extract_timestamp(metadata: &JsonValue) -> Result<SystemTime> {
    let secs = metadata
        .get("timestamp")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| err_msg("template metadata must contain a numeric 'timestamp' field"))?;
    Ok(UNIX_EPOCH + Duration::from_secs(secs))
}

fn lookback_window(duration: &str) -> Result<(SystemTime, SystemTime)> {
    let dur = humantime::parse_duration(duration)
        .map_err(|e| err_msg(format!("invalid duration '{duration}': {e}")))?;
    let now = SystemTime::now();
    let start = now.checked_sub(dur).unwrap_or(UNIX_EPOCH);
    let end = now + Duration::from_secs(1);
    Ok((start, end))
}

// ── ShardsManager impl ────────────────────────────────────────────────────────

impl ShardsManager {
    // ── writes ────────────────────────────────────────────────────────────────

    /// Store a template in the shard that covers its `"timestamp"` field.
    ///
    /// `metadata` must contain a numeric `"timestamp"` field (Unix seconds).
    /// Both metadata and body are automatically embedded and indexed in the
    /// shard's tplstorage vector index.  Returns the UUIDv7 of the stored
    /// template.
    pub fn tpl_add(&self, metadata: JsonValue, body: &[u8]) -> Result<Uuid> {
        let ts = extract_timestamp(&metadata)?;
        let shard = self.cache.shard(ts)?;
        let id = shard.tpl_add(metadata, body)?;
        shard.tplstorage.sync()?;
        Ok(id)
    }

    /// Replace the metadata for template `id` and re-embed it.
    ///
    /// Scans all registered shards to locate `id`.  Returns an error if the
    /// template is not found.  The updated metadata must still carry the
    /// original `"timestamp"` so the record remains in the correct shard.
    pub fn tpl_update_metadata(&self, id: Uuid, metadata: JsonValue) -> Result<()> {
        for info in self.cache.info().list_all()? {
            let shard = self.cache.shard(info.start_time)?;
            if shard.tpl_get_metadata(id)?.is_some() {
                return shard.tpl_update_metadata(id, metadata);
            }
        }
        Err(err_msg(format!("template {id} not found in any shard")))
    }

    /// Replace the body of template `id` and re-embed it.
    ///
    /// Scans all registered shards to locate `id`.
    pub fn tpl_update_body(&self, id: Uuid, body: &[u8]) -> Result<()> {
        for info in self.cache.info().list_all()? {
            let shard = self.cache.shard(info.start_time)?;
            if shard.tpl_get_metadata(id)?.is_some() {
                return shard.tpl_update_body(id, body);
            }
        }
        Err(err_msg(format!("template {id} not found in any shard")))
    }

    /// Remove template `id` from whichever shard contains it.
    ///
    /// Returns `Ok(())` if no shard contains the record.
    pub fn tpl_delete(&self, id: Uuid) -> Result<()> {
        for info in self.cache.info().list_all()? {
            let shard = self.cache.shard(info.start_time)?;
            if shard.tpl_get_metadata(id)?.is_some() {
                return shard.tpl_delete(id);
            }
        }
        Ok(())
    }

    // ── reads ─────────────────────────────────────────────────────────────────

    /// Return the JSON metadata for template `id`, scanning all shards.
    ///
    /// Returns `None` if no shard contains a template with that UUID.
    pub fn tpl_get_metadata(&self, id: Uuid) -> Result<Option<JsonValue>> {
        for info in self.cache.info().list_all()? {
            let shard = self.cache.shard(info.start_time)?;
            if let Some(meta) = shard.tpl_get_metadata(id)? {
                return Ok(Some(meta));
            }
        }
        Ok(None)
    }

    /// Return the raw body bytes for template `id`, scanning all shards.
    ///
    /// Returns `None` if no shard contains a template with that UUID.
    pub fn tpl_get_body(&self, id: Uuid) -> Result<Option<Vec<u8>>> {
        for info in self.cache.info().list_all()? {
            let shard = self.cache.shard(info.start_time)?;
            if shard.tpl_get_metadata(id)?.is_some() {
                return shard.tpl_get_body(id);
            }
        }
        Ok(None)
    }

    // ── range queries ─────────────────────────────────────────────────────────

    /// Return all templates stored in shards that overlap
    /// `[now − duration, now]`, as `(id, metadata)` pairs.
    ///
    /// Results are merged from all matching shards.
    pub fn tpl_list(&self, duration: &str) -> Result<Vec<(Uuid, JsonValue)>> {
        let (start, end) = lookback_window(duration)?;
        let mut out = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            out.extend(shard.tpl_list()?);
        }
        Ok(out)
    }

    /// Semantic search over templates in shards overlapping
    /// `[now − duration, now]`, using a plain-text query.
    ///
    /// Results from all matching shards are merged and sorted by score
    /// descending, then truncated to `limit`.
    pub fn tpl_search_text(
        &self,
        duration: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<JsonValue>> {
        let (start, end) = lookback_window(duration)?;
        let mut results: Vec<JsonValue> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            results.extend(shard.tpl_search_text(query, limit)?);
        }
        results.sort_by(|a, b| {
            let sa = a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let sb = b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(results)
    }

    /// Semantic search over templates in shards overlapping
    /// `[now − duration, now]`, using a JSON query object.
    ///
    /// Results are merged, sorted by score descending, and truncated to `limit`.
    pub fn tpl_search_json(
        &self,
        duration: &str,
        query: &JsonValue,
        limit: usize,
    ) -> Result<Vec<JsonValue>> {
        let (start, end) = lookback_window(duration)?;
        let mut results: Vec<JsonValue> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            results.extend(shard.tpl_search_json(query, limit)?);
        }
        results.sort_by(|a, b| {
            let sa = a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let sb = b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(results)
    }

    /// Rebuild the tplstorage vector index for every shard overlapping
    /// `[now − duration, now]`.
    ///
    /// Returns the total number of templates re-indexed across all shards.
    pub fn tpl_reindex(&self, duration: &str) -> Result<usize> {
        let (start, end) = lookback_window(duration)?;
        let mut total = 0usize;
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            total += shard.tpl_reindex()?;
        }
        Ok(total)
    }
}
