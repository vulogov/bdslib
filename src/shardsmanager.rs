use crate::common::cache_json::JsonCache;
use crate::common::drain::DrainParser;
use crate::common::error::{err_msg, Result};
use crate::common::time::{extract_timestamp, lookback_window};
use crate::documentstorage::DocumentStorage;
use crate::observability::ObservabilityStorageConfig;
use crate::shardscache::ShardsCache;
use crate::vectorengine::json_fingerprint;
use crate::EmbeddingEngine;
use fastembed::EmbeddingModel;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

struct ManagerConfig {
    dbpath: String,
    shard_duration: String,
    pool_size: u32,
    similarity_threshold: Option<f32>,
    drain_enabled: bool,
    drain_load_duration: String,
    jsoncache_capacity: usize,
    jsoncache_ttl_secs: u64,
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

    let drain_enabled = obj
        .get("drain_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let drain_load_duration = obj
        .get("drain_load_duration")
        .and_then(|v| v.as_str())
        .unwrap_or("24h")
        .to_string();

    let jsoncache_capacity = obj
        .get("jsoncache_capacity")
        .and_then(|v| v.as_f64())
        .map(|n| n as usize)
        .unwrap_or(10_000);

    let jsoncache_ttl_secs = obj
        .get("jsoncache_ttl_secs")
        .and_then(|v| v.as_f64())
        .map(|n| n as u64)
        .unwrap_or(300);

    Ok(ManagerConfig {
        dbpath,
        shard_duration,
        pool_size,
        similarity_threshold,
        drain_enabled,
        drain_load_duration,
        jsoncache_capacity,
        jsoncache_ttl_secs,
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
/// | `drain_enabled` | bool | no (default false) | Mine log templates on every `add()` / `add_batch()` |
/// | `drain_load_duration` | string | no (default `"24h"`) | Lookback window for seeding drain from stored templates at startup |
/// | `jsoncache_capacity` | integer | no (default 10000) | Maximum number of records held in the in-memory JSON cache |
/// | `jsoncache_ttl_secs` | integer | no (default 300) | Per-entry TTL in seconds for the JSON cache |
///
/// `ShardsManager` is `Clone`; all clones share the same underlying shard cache,
/// document store, drain parser, and JSON cache.
#[derive(Clone)]
pub struct ShardsManager {
    pub(crate) cache: ShardsCache,
    pub(crate) docstore: DocumentStorage,
    /// Shared drain parser; `Some` only when `drain_enabled = true` in the config.
    pub(crate) drain: Option<Arc<Mutex<DrainParser>>>,
    /// Maps in-memory drain cluster ID → stored tplstorage UUID.
    pub(crate) drain_cluster_map: Arc<Mutex<HashMap<usize, Uuid>>>,
    /// In-memory LRU cache keyed by `(id, timestamp)`.  Populated on every
    /// write and on every search result fetch; consulted before any DB round-trip.
    pub(crate) jsoncache: JsonCache,
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

        // Clone the engine before handing ownership to the cache; both the
        // shard cache and the document store share the same underlying Arc.
        // Template storage lives inside each Shard at {shard_path}/tplstorage.
        let docstore_path = format!("{}/docstore", cfg.dbpath);
        let docstore = DocumentStorage::with_embedding(&docstore_path, embedding.clone())?;

        let cache = ShardsCache::with_config(
            &cfg.dbpath,
            &cfg.shard_duration,
            cfg.pool_size,
            embedding,
            obs_config,
        )?;

        let jsoncache = JsonCache::new(
            cfg.jsoncache_capacity,
            Duration::from_secs(cfg.jsoncache_ttl_secs),
        );

        let mut manager = Self {
            cache,
            docstore,
            drain: None,
            drain_cluster_map: Arc::new(Mutex::new(HashMap::new())),
            jsoncache,
        };

        if cfg.drain_enabled {
            let (parser, cluster_map) = manager.drain_load(&cfg.drain_load_duration)?;
            if let Ok(mut m) = manager.drain_cluster_map.lock() {
                *m = cluster_map;
            }
            manager.drain = Some(Arc::new(Mutex::new(parser)));
        }

        Ok(manager)
    }

    // ── writes ────────────────────────────────────────────────────────────────

    /// Add a JSON document to the shard covering its `"timestamp"` field.
    ///
    /// The document must contain a numeric `"timestamp"` field (Unix seconds).
    /// Returns the UUIDv7 assigned to the stored record.
    ///
    /// When `drain_enabled` is set in the configuration, the document's log
    /// string is also parsed by the drain3 algorithm and any newly discovered or
    /// updated templates are stored in the shard's tplstorage.  Drain errors
    /// (e.g. the document has no `"data"` field) are non-fatal and do not
    /// prevent the document from being stored.
    pub fn add(&self, doc: JsonValue) -> Result<Uuid> {
        let maybe_cluster_id: Option<usize> = if let Some(drain) = &self.drain {
            if let Ok(mut parser) = drain.lock() {
                let result = parser.parse_json_with_callback(&doc, |meta, body| {
                    self.tpl_add(meta, &body)
                });
                drop(parser);
                match result {
                    Ok(r) => {
                        let cluster_id = r.cluster_id;
                        if let Ok(mut map) = self.drain_cluster_map.lock() {
                            if let Some(uuid) = r.stored_id {
                                map.insert(cluster_id, uuid);
                            }
                        }
                        Some(cluster_id)
                    }
                    Err(_) => None,
                }
            } else {
                None
            }
        } else {
            None
        };

        let ts = extract_timestamp(&doc)?;
        let shard = self.cache.shard(ts)?;

        if let Some(cluster_id) = maybe_cluster_id {
            if let Ok(map) = self.drain_cluster_map.lock() {
                if let Some(uuid) = map.get(&cluster_id) {
                    let _ = shard.tplstorage.frequencytracking_observe(&uuid.to_string());
                }
            }
        }

        let ts_u64 = doc["timestamp"].as_u64().unwrap_or(0);
        let mut cached_doc = doc.clone();
        let id = shard.add(doc)?;
        cached_doc["id"] = serde_json::json!(id.to_string());
        self.jsoncache.insert(id.to_string(), ts_u64, cached_doc);
        Ok(id)
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

        // Run drain on all docs before shard routing. Lock is held for the
        // whole batch (parsing is CPU-only, no I/O under the lock).
        let mut matched_cluster_ids: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        if let Some(drain) = &self.drain {
            let mut new_mappings: Vec<(usize, Uuid)> = Vec::new();
            if let Ok(mut parser) = drain.lock() {
                for doc in &docs {
                    if let Ok(r) = parser.parse_json_with_callback(doc, |meta, body| {
                        self.tpl_add(meta, &body)
                    }) {
                        if let Some(uuid) = r.stored_id {
                            new_mappings.push((r.cluster_id, uuid));
                        }
                        matched_cluster_ids.insert(r.cluster_id);
                    }
                }
            }
            if !new_mappings.is_empty() {
                if let Ok(mut map) = self.drain_cluster_map.lock() {
                    for (cid, uuid) in new_mappings {
                        map.insert(cid, uuid);
                    }
                }
            }
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
        let mut first_shard: Option<crate::shard::Shard> = None;

        while group_start < tagged.len() {
            let current_start = tagged[group_start].shard_start;

            // Find the end of this shard's group.
            let group_end = tagged[group_start..]
                .partition_point(|t| t.shard_start == current_start)
                + group_start;

            // Open the shard once; lock is released before any document work.
            let shard = self.cache.shard(current_start)?;
            if first_shard.is_none() {
                first_shard = Some(shard.clone());
            }

            let group = &tagged[group_start..group_end];
            let shard_docs: Vec<JsonValue> =
                group.iter().map(|t| t.doc.clone()).collect();
            let shard_ids = shard.add_batch(shard_docs)?;

            for (t, id) in group.iter().zip(shard_ids.iter().copied()) {
                result_ids[t.orig_idx] = id;
                let ts_u64 = t.doc["timestamp"].as_u64().unwrap_or(0);
                let mut cached_doc = t.doc.clone();
                cached_doc["id"] = serde_json::json!(id.to_string());
                self.jsoncache.insert(id.to_string(), ts_u64, cached_doc);
            }

            group_start = group_end;
        }

        // Record "seen now" observations for every unique template matched in this batch.
        if let Some(shard) = first_shard {
            if !matched_cluster_ids.is_empty() {
                if let Ok(map) = self.drain_cluster_map.lock() {
                    for cluster_id in &matched_cluster_ids {
                        if let Some(uuid) = map.get(cluster_id) {
                            let _ = shard.tplstorage.frequencytracking_observe(&uuid.to_string());
                        }
                    }
                }
            }
        }

        Ok(result_ids)
    }

    /// Delete the record with `id` from whichever catalog-registered shard contains it.
    ///
    /// Returns `Ok(())` if no shard contains the record.
    pub fn delete_by_id(&self, id: Uuid) -> Result<()> {
        self.jsoncache.remove_by_id(&id.to_string());
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
            let scored = shard.search_fts_scored(query, 100)?;
            let ids: Vec<Uuid> = scored.iter().map(|(id, _)| *id).collect();
            let doc_map = self.resolve_records_with_cache(&shard, &ids)?;
            for (id, _score) in &scored {
                if let Some(doc) = doc_map.get(id) {
                    results.push(doc.clone());
                }
            }
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
        let fingerprint = json_fingerprint(query);
        let query_vec = self.cache.embedding().embed(&fingerprint)?;
        let (start, end) = lookback_window(duration)?;
        let mut results: Vec<(Uuid, i64, f32)> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            results.extend(shard.search_vector_scored_precomputed(&query_vec, &fingerprint, limit)?);
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
        let fingerprint = json_fingerprint(query);
        let query_vec = self.cache.embedding().embed(&fingerprint)?;
        let (start, end) = lookback_window(duration)?;
        let mut results: Vec<JsonValue> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            let shard_results =
                shard.search_vector_precomputed(&query_vec, &fingerprint, limit)?;
            for doc in &shard_results {
                if let (Some(id_str), Some(ts)) =
                    (doc["id"].as_str(), doc["timestamp"].as_u64())
                {
                    self.jsoncache.insert(id_str.to_owned(), ts, doc.clone());
                }
            }
            results.extend(shard_results);
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
        let fingerprint = json_fingerprint(query);
        let query_vec = self.cache.embedding().embed(&fingerprint)?;
        let (start, end) = lookback_window(duration)?;
        let mut results = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            let shard_results =
                shard.search_vector_precomputed(&query_vec, &fingerprint, 100)?;
            for doc in &shard_results {
                if let (Some(id_str), Some(ts)) =
                    (doc["id"].as_str(), doc["timestamp"].as_u64())
                {
                    self.jsoncache.insert(id_str.to_owned(), ts, doc.clone());
                }
            }
            results.extend(shard_results);
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

    /// Return the unique, sorted list of primary record keys within
    /// `[now − duration, now + 1s)` whose key matches `pattern` (DuckDB shell-glob).
    ///
    /// Pass `"*"` as the pattern to return all keys (equivalent to `v2/keys`).
    pub fn keys_all(&self, duration: &str, pattern: &str) -> Result<Vec<String>> {
        let (start, end) = lookback_window(duration)?;
        let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            let shard_keys = shard
                .observability()
                .list_primary_keys_in_range_by_pattern(pattern, start, end)?;
            keys.extend(shard_keys);
        }
        Ok(keys.into_iter().collect())
    }

    /// Return keys that have more than one primary record within
    /// `[now − duration, now + 1s)`, together with their record count and IDs.
    ///
    /// Results are sorted alphabetically by key.  Keys with exactly one primary
    /// are excluded.
    pub fn primaries_explore(
        &self,
        duration: &str,
    ) -> Result<Vec<(String, usize, Vec<Uuid>)>> {
        let (start, end) = lookback_window(duration)?;
        let mut key_map: std::collections::HashMap<String, Vec<Uuid>> =
            std::collections::HashMap::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            for (id, key) in shard
                .observability()
                .list_primaries_with_keys_in_range(start, end)?
            {
                key_map.entry(key).or_default().push(id);
            }
        }
        let mut result: Vec<(String, usize, Vec<Uuid>)> = key_map
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(key, ids)| {
                let count = ids.len();
                (key, count, ids)
            })
            .collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }

    /// Return keys with more than one primary record that carries numeric data
    /// within `[now − duration, now + 1s)`, together with their record count
    /// and IDs.
    ///
    /// A record is considered numeric when `data` is a JSON number **or**
    /// `data.value` is a JSON number (the same extraction rule used by
    /// [`TelemetryTrend`](crate::TelemetryTrend)).  Results are sorted
    /// alphabetically by key.
    pub fn primaries_explore_telemetry(
        &self,
        duration: &str,
    ) -> Result<Vec<(String, usize, Vec<Uuid>)>> {
        let (start, end) = lookback_window(duration)?;
        let mut key_map: std::collections::HashMap<String, Vec<Uuid>> =
            std::collections::HashMap::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            for (id, key, data) in shard
                .observability()
                .list_primaries_with_data_in_range(start, end)?
            {
                if data.as_f64().is_some() || data["value"].as_f64().is_some() {
                    key_map.entry(key).or_default().push(id);
                }
            }
        }
        let mut result: Vec<(String, usize, Vec<Uuid>)> = key_map
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(key, ids)| {
                let count = ids.len();
                (key, count, ids)
            })
            .collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }

    /// Return `(id, timestamp, data)` for every primary record whose `key`
    /// matches exactly within `[now − duration, now + 1s)`, sorted by
    /// timestamp ascending.
    pub fn primaries_get(
        &self,
        duration: &str,
        key: &str,
    ) -> Result<Vec<(Uuid, u64, JsonValue)>> {
        let (start, end) = lookback_window(duration)?;
        let start_secs = start
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let end_secs = end
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut results: Vec<(Uuid, u64, JsonValue)> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            for doc in shard.get_primaries_by_key(key)? {
                let ts = doc["timestamp"].as_u64().unwrap_or(0);
                if ts >= start_secs && ts < end_secs {
                    let id = doc["id"]
                        .as_str()
                        .and_then(|s| Uuid::parse_str(s).ok())
                        .unwrap_or_default();
                    let data = doc["data"].clone();
                    results.push((id, ts, data));
                }
            }
        }
        results.sort_by_key(|(_, ts, _)| *ts);
        Ok(results)
    }

    /// Return `(id, timestamp, value)` for every primary record whose `key`
    /// matches exactly within `[now − duration, now + 1s)` and whose `data`
    /// contains a numeric measurement.  Records where no number can be extracted
    /// are silently skipped.  Results are sorted by timestamp ascending.
    ///
    /// Extraction order: bare `data` number first, then `data["value"]`.
    pub fn primaries_get_telemetry(
        &self,
        duration: &str,
        key: &str,
    ) -> Result<Vec<(Uuid, u64, f64)>> {
        let (start, end) = lookback_window(duration)?;
        let start_secs = start
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let end_secs = end
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut results: Vec<(Uuid, u64, f64)> = Vec::new();
        for info in self.cache.info().shards_in_range(start, end)? {
            let shard = self.cache.shard(info.start_time)?;
            for doc in shard.get_primaries_by_key(key)? {
                let ts = doc["timestamp"].as_u64().unwrap_or(0);
                if ts >= start_secs && ts < end_secs {
                    let d = &doc["data"];
                    let value = d.as_f64().or_else(|| d["value"].as_f64());
                    if let Some(v) = value {
                        let id = doc["id"]
                            .as_str()
                            .and_then(|s| Uuid::parse_str(s).ok())
                            .unwrap_or_default();
                        results.push((id, ts, v));
                    }
                }
            }
        }
        results.sort_by_key(|(_, ts, _)| *ts);
        Ok(results)
    }

    // ── cache-first record resolution ─────────────────────────────────────────

    /// Resolve a list of UUIDs to full JSON documents, preferring the in-memory
    /// cache over the shard database.
    ///
    /// For each UUID: if a live cache entry exists it is returned immediately.
    /// UUIDs not found in the cache are batch-fetched from `shard`'s
    /// observability storage (including secondaries), and the result is stored
    /// in the cache before being returned.
    ///
    /// Returns a `HashMap` keyed by UUID so callers can reassemble results in
    /// their original order (e.g. FTS relevance order).
    fn resolve_records_with_cache(
        &self,
        shard: &crate::shard::Shard,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, JsonValue>> {
        let mut result: HashMap<Uuid, JsonValue> = HashMap::new();
        let mut missed: Vec<Uuid> = Vec::new();

        for &id in ids {
            match self.jsoncache.get_by_id(&id.to_string()) {
                Some(doc) => { result.insert(id, doc); }
                None      => missed.push(id),
            }
        }

        if !missed.is_empty() {
            let obs = shard.observability();
            let docs = obs.get_by_ids(&missed)?;
            let mut sec_map = obs.get_secondaries_batch(&missed)?;

            for mut doc in docs {
                let uuid = doc["id"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok());
                if let Some(uuid) = uuid {
                    let secondaries = sec_map.remove(&uuid).unwrap_or_default();
                    doc["secondaries"] = serde_json::json!(secondaries);
                    let ts = doc["timestamp"].as_u64().unwrap_or(0);
                    self.jsoncache.insert(uuid.to_string(), ts, doc.clone());
                    result.insert(uuid, doc);
                }
            }
        }

        Ok(result)
    }

    // ── accessors ─────────────────────────────────────────────────────────────

    /// Borrow the underlying [`ShardsCache`].
    pub fn cache(&self) -> &ShardsCache {
        &self.cache
    }

    /// Borrow the embedded [`DocumentStorage`].
    pub fn docstore(&self) -> &DocumentStorage {
        &self.docstore
    }

    /// Number of entries currently held in the JSON cache (including stale
    /// entries not yet swept by the background thread).
    pub fn jsoncache_len(&self) -> usize {
        self.jsoncache.len()
    }

    /// Maximum number of entries the JSON cache will hold before evicting.
    pub fn jsoncache_capacity(&self) -> usize {
        self.jsoncache.capacity()
    }

    /// Cache utilization as an integer percentage `[0, 100]`.
    ///
    /// Returns 0 when capacity is zero (cache disabled).
    pub fn jsoncache_utilization_pct(&self) -> u64 {
        let cap = self.jsoncache.capacity();
        if cap == 0 {
            return 0;
        }
        (self.jsoncache.len() * 100 / cap) as u64
    }
}

