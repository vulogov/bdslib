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
    /// Returns UUIDs in the same order as the input documents.
    pub fn add_batch(&self, docs: Vec<JsonValue>) -> Result<Vec<Uuid>> {
        docs.into_iter().map(|doc| self.add(doc)).collect()
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
