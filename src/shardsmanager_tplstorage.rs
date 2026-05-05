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
use crate::common::time::{extract_timestamp, lookback_window};
use crate::shardsmanager::ShardsManager;
use serde_json::Value as JsonValue;
use std::collections::HashSet;
use uuid::Uuid;

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

    // ── frequency-tracking queries ────────────────────────────────────────────

    /// Return the full template record for `id`, scanning all registered shards.
    ///
    /// The returned JSON object has three keys:
    /// - `"id"`: the UUID string
    /// - `"metadata"`: the stored template metadata
    /// - `"body"`: the template content decoded as UTF-8
    ///
    /// Returns `None` if no shard contains a template with that UUID.
    /// Returns `Err` if `id` is not a valid UUID string.
    pub fn template_by_id(&self, id: &str) -> Result<Option<JsonValue>> {
        let uuid = Uuid::parse_str(id)
            .map_err(|e| err_msg(format!("invalid template id '{id}': {e}")))?;
        for info in self.cache.info().list_all()? {
            let shard = self.cache.shard(info.start_time)?;
            if let Some(metadata) = shard.tpl_get_metadata(uuid)? {
                let body = shard.tpl_get_body(uuid)?.unwrap_or_default();
                let body_str = String::from_utf8_lossy(&body).into_owned();
                return Ok(Some(serde_json::json!({
                    "id":       id,
                    "metadata": metadata,
                    "body":     body_str,
                })));
            }
        }
        Ok(None)
    }

    /// Return all templates whose FrequencyTracking observation falls in the
    /// inclusive interval `[start, end]` (Unix seconds).
    ///
    /// Each shard's `tplstorage` FrequencyTracking is queried for template
    /// UUIDs in the time range; those UUIDs are then resolved to full template
    /// records with `"id"`, `"metadata"`, and `"body"` fields.  Results are
    /// deduplicated by UUID across shards.
    pub fn templates_by_timestamp(&self, start: u64, end: u64) -> Result<Vec<JsonValue>> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for info in self.cache.info().list_all()? {
            let shard = self.cache.shard(info.start_time)?;
            for id_str in shard.tplstorage.frequencytracking_time_range(start, end)? {
                if !seen.insert(id_str.clone()) {
                    continue;
                }
                if let Ok(uuid) = Uuid::parse_str(&id_str) {
                    if let Some(metadata) = shard.tpl_get_metadata(uuid)? {
                        let body = shard.tpl_get_body(uuid)?.unwrap_or_default();
                        let body_str = String::from_utf8_lossy(&body).into_owned();
                        out.push(serde_json::json!({
                            "id":       id_str,
                            "metadata": metadata,
                            "body":     body_str,
                        }));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Return all templates added within the humantime `duration` window.
    ///
    /// Queries each shard's `tplstorage` FrequencyTracking with
    /// [`recent`](crate::FrequencyTracking::recent) to collect template UUIDs
    /// seen in `[now − duration, now]`, then resolves each UUID to a full
    /// template record with `"id"`, `"metadata"`, and `"body"` fields.
    /// Results are deduplicated by UUID across shards.
    ///
    /// `duration` is any string accepted by
    /// [`humantime::parse_duration`]: `"30s"`, `"5min"`, `"1h"`, `"7days"`.
    pub fn templates_recent(&self, duration: &str) -> Result<Vec<JsonValue>> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for info in self.cache.info().list_all()? {
            let shard = self.cache.shard(info.start_time)?;
            for id_str in shard.tplstorage.frequencytracking_recent(duration)? {
                if !seen.insert(id_str.clone()) {
                    continue;
                }
                if let Ok(uuid) = Uuid::parse_str(&id_str) {
                    if let Some(metadata) = shard.tpl_get_metadata(uuid)? {
                        let body = shard.tpl_get_body(uuid)?.unwrap_or_default();
                        let body_str = String::from_utf8_lossy(&body).into_owned();
                        out.push(serde_json::json!({
                            "id":       id_str,
                            "metadata": metadata,
                            "body":     body_str,
                        }));
                    }
                }
            }
        }
        Ok(out)
    }
}
