use crate::common::error::{err_msg, Result};
use crate::observability::ObservabilityStorageConfig;
use crate::shardscache::ShardsCache;
use crate::EmbeddingEngine;
use fastembed::EmbeddingModel;
use serde_json::Value as JsonValue;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

struct ManagerConfig {
    dbpath: String,
    shard_duration: String,
    pool_size: u32,
    similarity_threshold: Option<f32>,
}

fn parse_config(raw: &str) -> Result<ManagerConfig> {
    let val: serde_hjson::Value = serde_hjson::from_str(raw)
        .map_err(|e| err_msg(format!("hjson parse error: {e}")))?;
    let obj = val
        .as_object()
        .ok_or_else(|| err_msg("config must be a JSON object"))?;

    let dbpath = obj
        .get("dbpath")
        .and_then(|v| v.as_str())
        .ok_or_else(|| err_msg("missing required field 'dbpath'"))?
        .to_string();

    let shard_duration = obj
        .get("shard_duration")
        .and_then(|v| v.as_str())
        .ok_or_else(|| err_msg("missing required field 'shard_duration'"))?
        .to_string();

    let pool_size = obj
        .get("pool_size")
        .and_then(|v| v.as_f64())
        .map(|n| n as u32)
        .unwrap_or(4);

    let similarity_threshold = obj
        .get("similarity_threshold")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);

    Ok(ManagerConfig {
        dbpath,
        shard_duration,
        pool_size,
        similarity_threshold,
    })
}

/// High-level shard-aware document store driven by an hjson configuration file.
///
/// `ShardsManager` wraps a [`ShardsCache`] and routes records to the correct
/// time-partitioned shard based on each document's embedded `"timestamp"` field.
///
/// The configuration file is an [hjson](https://hjson.github.io/) document with
/// the following keys:
///
/// | Key | Type | Required | Description |
/// |---|---|---|---|
/// | `dbpath` | string | yes | Filesystem root for all shards |
/// | `shard_duration` | string | yes | Shard width (`"1h"`, `"1day"`, …) |
/// | `pool_size` | integer | no (default 4) | DuckDB connection-pool size |
/// | `similarity_threshold` | float | no (default 0.85) | Deduplication threshold |
///
/// `ShardsManager` is `Clone`; all clones share the same underlying shard cache.
#[derive(Clone)]
pub struct ShardsManager {
    cache: ShardsCache,
}

impl ShardsManager {
    /// Open or create a shard manager described by the hjson config at `config_path`.
    ///
    /// Loads [`EmbeddingModel::AllMiniLML6V2`]. Use [`with_embedding`](Self::with_embedding)
    /// to supply a pre-loaded model and avoid repeated download/initialisation costs.
    pub fn new(config_path: &str) -> Result<Self> {
        let embedding = EmbeddingEngine::new(EmbeddingModel::AllMiniLML6V2, None)
            .map_err(|e| err_msg(format!("failed to load embedding model: {e}")))?;
        Self::with_embedding(config_path, embedding)
    }

    /// Open or create a shard manager with a pre-loaded embedding model.
    ///
    /// Preferred in tests to share a single model instance across test runs.
    pub fn with_embedding(config_path: &str, embedding: EmbeddingEngine) -> Result<Self> {
        let raw = std::fs::read_to_string(config_path)
            .map_err(|e| err_msg(format!("cannot read config '{config_path}': {e}")))?;
        let cfg = parse_config(&raw)
            .map_err(|e| err_msg(format!("invalid config '{config_path}': {e}")))?;

        let obs_config = match cfg.similarity_threshold {
            Some(t) => ObservabilityStorageConfig {
                similarity_threshold: t,
            },
            None => ObservabilityStorageConfig::default(),
        };
        let cache = ShardsCache::with_config(
            &cfg.dbpath,
            &cfg.shard_duration,
            cfg.pool_size,
            embedding,
            obs_config,
        )?;
        Ok(Self { cache })
    }

    // ── writes ────────────────────────────────────────────────────────────────

    /// Add a JSON document to the shard covering its `"timestamp"` field.
    ///
    /// The document must contain a numeric `"timestamp"` field (Unix seconds).
    /// Returns the UUIDv7 assigned to the stored record.
    pub fn add(&self, doc: JsonValue) -> Result<Uuid> {
        let ts = extract_timestamp(&doc)?;
        self.cache.shard(ts)?.add(doc)
    }

    /// Add a batch of JSON documents, routing each to its timestamp-appropriate shard.
    ///
    /// Documents are grouped by shard interval before processing so that each
    /// unique shard is opened exactly once and receives a single batched FTS
    /// commit for all its primaries, rather than one commit per document.
    /// This reduces mutex contention on `ShardsCache` and dramatically cuts
    /// Tantivy write amplification for large batches.
    ///
    /// The `ShardsCache` lock is never held during document processing — it is
    /// acquired briefly to look up or create each shard, then released before
    /// any I/O or embedding work begins.
    ///
    /// Returns UUIDs in the same order as the input documents.
    pub fn add_batch(&self, docs: Vec<JsonValue>) -> Result<Vec<Uuid>> {
        if docs.is_empty() {
            return Ok(vec![]);
        }

        let shard_dur = self.cache.shard_duration();

        // Tag each document with its original index and aligned shard-start time.
        struct Tagged {
            orig_idx: usize,
            shard_start: SystemTime,
            doc: JsonValue,
        }
        let mut tagged: Vec<Tagged> = Vec::with_capacity(docs.len());
        for (orig_idx, doc) in docs.into_iter().enumerate() {
            let ts = extract_timestamp(&doc)?;
            let (shard_start, _) =
                crate::common::timerange::align_to_duration(ts, shard_dur)?;
            tagged.push(Tagged { orig_idx, shard_start, doc });
        }

        // Sort so all docs for the same shard are contiguous.
        tagged.sort_by_key(|t| t.shard_start);

        let mut result_ids = vec![Uuid::nil(); tagged.len()];
        let mut group_start = 0;

        while group_start < tagged.len() {
            let current_start = tagged[group_start].shard_start;

            // Find the end of this shard's group.
            let group_end = tagged[group_start..]
                .partition_point(|t| t.shard_start == current_start)
                + group_start;

            // Open the shard once; lock is released before any document work.
            let shard = self.cache.shard(current_start)?;

            let group = &tagged[group_start..group_end];
            let shard_docs: Vec<JsonValue> =
                group.iter().map(|t| t.doc.clone()).collect();
            let shard_ids = shard.add_batch(shard_docs)?;

            for (t, id) in group.iter().zip(shard_ids) {
                result_ids[t.orig_idx] = id;
            }

            group_start = group_end;
        }

        Ok(result_ids)
    }

    /// Delete the record with `id` from whichever catalog-registered shard contains it.
    ///
    /// Returns `Ok(())` if no shard contains the record.
    pub fn delete_by_id(&self, id: Uuid) -> Result<()> {
        for info in self.cache.info().list_all()? {
            let shard = self.cache.shard(info.start_time)?;
            if shard.get(id)?.is_some() {
                return shard.delete(id);
            }
        }
        Ok(())
    }

    /// Update the record `id` with new content.
    ///
    /// Deletes the existing record and inserts the new document. If the new
    /// document's `"timestamp"` maps to a different shard interval, the record
    /// is moved to that shard. Returns the UUID of the newly inserted record.
    pub fn update(&self, id: Uuid, doc: JsonValue) -> Result<Uuid> {
        self.delete_by_id(id)?;
        self.add(doc)
    }

    // ── search ────────────────────────────────────────────────────────────────

    /// Full-text search across all catalog-registered shards that overlap the
    /// lookback window `[now − duration, now + 1s)`.
    ///
    /// `duration` uses the same human-readable format as the shard constructor
    /// (`"1h"`, `"30min"`, `"7days"`). No empty shards are auto-created.
    ///
    /// Results are returned in Tantivy relevance order within each shard, shards
    /// ordered oldest-first.
    pub fn search_fts(&self, duration: &str, query: &str) -> Result<Vec<JsonValue>> {
        let (start, end) = lookback_window(duration)?;
        let mut results = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            results.extend(shard.search_fts(query, 100)?);
        }
        Ok(results)
    }

    /// Full-text search returning `(primary_id, BM25_score)` pairs across all
    /// catalog-registered shards that overlap the lookback window
    /// `[now − duration, now + 1s)`.
    ///
    /// Results from all shards are merged and sorted by score descending.
    /// No document bodies are fetched — use [`search_fts`] when you need the
    /// full records.
    ///
    /// [`search_fts`]: ShardsManager::search_fts
    pub fn fulltextsearch(&self, duration: &str, query: &str, limit: usize) -> Result<Vec<(Uuid, f32)>> {
        let (start, end) = lookback_window(duration)?;
        let mut results: Vec<(Uuid, f32)> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            // Fetch up to `limit` candidates per shard; after merging across all
            // shards the final list is truncated to `limit` by score.
            results.extend(shard.search_fts_scored(query, limit)?);
        }
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        Ok(results)
    }

    /// Full-text search returning `(primary_id, unix_ts, BM25_score)` triples
    /// across all catalog-registered shards that overlap the lookback window
    /// `[now − duration, now + 1s)`.
    ///
    /// Results from all shards are merged and sorted by timestamp descending
    /// (most recent first). After sorting the list is truncated to `limit`.
    pub fn fulltextsearch_recent(
        &self,
        duration: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(Uuid, i64, f32)>> {
        let (start, end) = lookback_window(duration)?;
        let mut results: Vec<(Uuid, i64, f32)> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            results.extend(shard.search_fts_with_ts(query, limit)?);
        }
        results.sort_by(|a, b| b.1.cmp(&a.1));
        results.truncate(limit);
        Ok(results)
    }

    /// Semantic vector search returning `(primary_id, unix_ts, score)` triples
    /// across all catalog-registered shards that overlap
    /// `[now − duration, now + 1s)`.
    ///
    /// Results are merged from all shards, sorted by score descending, then
    /// truncated to `limit`. No document bodies are returned.
    pub fn vectorsearch(
        &self,
        duration: &str,
        query: &JsonValue,
        limit: usize,
    ) -> Result<Vec<(Uuid, i64, f32)>> {
        let (start, end) = lookback_window(duration)?;
        let mut results: Vec<(Uuid, i64, f32)> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            for doc in shard.search_vector(query, limit)? {
                let id_str = doc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let id = Uuid::parse_str(id_str)
                    .map_err(|e| err_msg(format!("invalid UUID in vector result: {e}")))?;
                let ts = doc.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
                let score = doc.get("_score").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                results.push((id, ts, score));
            }
        }
        results.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        Ok(results)
    }

    /// Semantic vector search returning full primary documents sorted by
    /// timestamp descending across all catalog-registered shards that overlap
    /// `[now − duration, now + 1s)`.
    ///
    /// Results from all shards are merged, sorted by `timestamp` descending,
    /// then truncated to `limit`. Each document includes a `"_score"` field
    /// and an embedded `"secondaries"` array.
    pub fn vectorsearch_recent(
        &self,
        duration: &str,
        query: &JsonValue,
        limit: usize,
    ) -> Result<Vec<JsonValue>> {
        let (start, end) = lookback_window(duration)?;
        let mut results: Vec<JsonValue> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            results.extend(shard.search_vector(query, limit)?);
        }
        results.sort_by(|a, b| {
            let ta = a.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
            let tb = b.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
            tb.cmp(&ta)
        });
        results.truncate(limit);
        Ok(results)
    }

    /// Semantic vector search across all catalog-registered shards that overlap
    /// the lookback window `[now − duration, now + 1s)`.
    ///
    /// Results from all shards are merged and sorted by `_score` descending, then
    /// `timestamp` descending. No empty shards are auto-created.
    pub fn search_vector(&self, duration: &str, query: &JsonValue) -> Result<Vec<JsonValue>> {
        let (start, end) = lookback_window(duration)?;
        let mut results = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            results.extend(shard.search_vector(query, 100)?);
        }
        results.sort_by(|a, b| {
            let sa = a.get("_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let sb = b.get("_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            sb.partial_cmp(&sa)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let ta = a.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
                    let tb = b.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
                    tb.cmp(&ta)
                })
        });
        Ok(results)
    }

    /// Return `(primary_id, timestamp, secondary_ids)` for every primary whose
    /// key matches `pattern` (DuckDB shell-glob: `*`, `?`, `[abc]`) across all
    /// shards that overlap `[now − duration, now + 1s)`.
    ///
    /// Results from all shards are merged and sorted by `timestamp` ascending.
    pub fn keys_by_pattern(
        &self,
        duration: &str,
        pattern: &str,
    ) -> Result<Vec<(Uuid, i64, Vec<Uuid>)>> {
        let (start, end) = lookback_window(duration)?;
        let mut results: Vec<(Uuid, i64, Vec<Uuid>)> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            let obs = shard.observability();
            for (id, ts) in obs.list_primaries_by_key_pattern_in_range(pattern, start, end)? {
                let secondaries = obs.list_secondaries(id)?;
                results.push((id, ts, secondaries));
            }
        }
        results.sort_by_key(|r| r.1);
        Ok(results)
    }

    // ── accessors ─────────────────────────────────────────────────────────────

    /// Borrow the underlying [`ShardsCache`].
    pub fn cache(&self) -> &ShardsCache {
        &self.cache
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn extract_timestamp(doc: &JsonValue) -> Result<SystemTime> {
    let secs = doc
        .get("timestamp")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| err_msg("document must contain a numeric 'timestamp' field"))?;
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
