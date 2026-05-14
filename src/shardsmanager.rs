use crate::cluster::{Cluster, ClusterConfig};
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
use std::path::PathBuf;
use rayon::prelude::*;
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
    r2d2_thread_pool_size: usize,
    max_open_shards: usize,
    /// Optional fastembed `EmbeddingModel` variant name (matches the Rust
    /// Debug form, case-insensitive — e.g. `"AllMiniLML6V2"`,
    /// `"BGESmallENV15"`).  When `None`, [`DEFAULT_EMBEDDING_MODEL`] is used.
    embedding_model: Option<String>,
    /// Optional override for the fastembed model cache directory.  When
    /// `None`, fastembed's default is used (`~/.cache/huggingface/hub` or
    /// `$HF_HOME`).
    embedding_cache_dir: Option<String>,
    /// Capacity of the in-process query-embedding cache.  `0` disables
    /// the cache.  Default: 256.
    embedding_cache_size: usize,
}

/// Fallback when the config does not pin an `embedding_model`.  Matches
/// the historical hardcoded choice; existing dbpaths keep working with no
/// config change.
pub(crate) const DEFAULT_EMBEDDING_MODEL: EmbeddingModel = EmbeddingModel::AllMiniLML6V2;

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

    let r2d2_thread_pool_size = obj
        .get("r2d2_thread_pool_size")
        .and_then(|v| v.as_f64())
        .map(|n| n as usize)
        .unwrap_or(3);

    let max_open_shards = obj
        .get("max_open_shards")
        .and_then(|v| v.as_f64())
        .map(|n| n as usize)
        .unwrap_or(16);

    let embedding_model = obj
        .get("embedding_model")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let embedding_cache_dir = obj
        .get("embedding_cache_dir")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let embedding_cache_size = obj
        .get("embedding_cache_size")
        .and_then(|v| v.as_f64())
        .map(|n| n as usize)
        .unwrap_or(crate::embedding::DEFAULT_CACHE_CAPACITY);

    Ok(ManagerConfig {
        dbpath,
        shard_duration,
        pool_size,
        similarity_threshold,
        drain_enabled,
        drain_load_duration,
        jsoncache_capacity,
        jsoncache_ttl_secs,
        r2d2_thread_pool_size,
        max_open_shards,
        embedding_model,
        embedding_cache_dir,
        embedding_cache_size,
    })
}

/// Resolve a `&str` model name to a fastembed `EmbeddingModel`.  Matches
/// the variant name case-insensitively (`"BGESmallENV15"`, `"bgesmallenv15"`,
/// and `"BgeSmallEnV15"` all work).
fn parse_embedding_model(name: &str) -> Result<EmbeddingModel> {
    name.parse::<EmbeddingModel>().map_err(|e| {
        err_msg(format!(
            "invalid embedding_model {name:?}: {e}. \
             See fastembed::EmbeddingModel for accepted variant names."
        ))
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
/// Outcome of a single [`ShardsManager::evict_shard`] call.
///
/// `start_time` / `end_time` are surfaced so the retention sweeper can
/// invalidate any cache layer keyed by record timestamp (the JsonCache)
/// without having to re-fetch the catalog row.
#[derive(Debug, Clone)]
pub struct EvictionOutcome {
    pub shard_id:   Uuid,
    pub path:       String,
    pub start_time: SystemTime,
    pub end_time:   SystemTime,
    pub freed_bytes: u64,
    /// `false` when `shard_id` didn't match any catalog row (idempotent
    /// no-op).  `true` for any actual removal — including the case where
    /// the on-disk directory had already vanished but the catalog row
    /// existed.
    pub existed:    bool,
}

impl EvictionOutcome {
    fn not_found(shard_id: Uuid) -> Self {
        Self {
            shard_id,
            path:       String::new(),
            start_time: UNIX_EPOCH,
            end_time:   UNIX_EPOCH,
            freed_bytes: 0,
            existed:    false,
        }
    }
}

#[derive(Clone)]
pub struct ShardsManager {
    pub(crate) cache: ShardsCache,
    pub(crate) docstore: DocumentStorage,
    /// Signal store: emitted signals with name, severity, and timestamp metadata.
    pub(crate) signals: DocumentStorage,
    /// Script store: BUND scripts addressable by UUID, with `name` + `schedule`
    /// metadata. Backed by `DocumentStorage` like `docstore` and `signals`.
    pub(crate) scripts: DocumentStorage,
    /// Shared drain parser; `Some` only when `drain_enabled = true` in the config.
    pub(crate) drain: Option<Arc<Mutex<DrainParser>>>,
    /// Maps in-memory drain cluster ID → stored tplstorage UUID.
    pub(crate) drain_cluster_map: Arc<Mutex<HashMap<usize, Uuid>>>,
    /// In-memory LRU cache keyed by `(id, timestamp)`.  Populated on every
    /// write and on every search result fetch; consulted before any DB round-trip.
    pub(crate) jsoncache: JsonCache,
    /// Resolved name of the loaded embedding model (Rust Debug form, e.g.
    /// `"AllMiniLML6V2"`).  Populated by [`Self::new`]; left `None` when the
    /// caller used [`Self::with_embedding`] directly (typically tests, where
    /// no model name is meaningful).
    pub(crate) embedding_model_name: Arc<std::sync::Mutex<Option<String>>>,
    /// Cluster layer.  `Some` when `cluster.enabled = true` in `bds.hjson`;
    /// `None` for stand-alone deployments (zero overhead — no background tasks).
    pub(crate) cluster: Option<Arc<Cluster>>,
    /// Serialises concurrent callers of `add_batch`.
    ///
    /// `add_batch` internally uses Rayon to parallelise across shards, but
    /// the per-shard storage engines (DuckDB connection pool, Tantivy
    /// index writer, HNSW upserter) are not safe under concurrent writers
    /// from different threads.  Without this lock, two flushers (or a
    /// flusher + a direct `v2/add` call) that target the same shard
    /// produce `Batch transaction failed` / `Execution of select_all
    /// failed` errors from DuckDB.
    ///
    /// Held only for the duration of one `add_batch` call.  Since the
    /// Rayon par_iter inside `add_batch` already saturates the engines
    /// across shards, this lock costs nothing on the common path.
    pub(crate) add_batch_lock: Arc<std::sync::Mutex<()>>,
}

impl ShardsManager {
    /// Open or create a shard manager described by the hjson config at `config_path`.
    ///
    /// Reads two optional config keys to pick the embedding model:
    ///
    /// - `embedding_model` — fastembed [`EmbeddingModel`] variant name
    ///   (matches Rust Debug form, case-insensitive).  Defaults to
    ///   [`DEFAULT_EMBEDDING_MODEL`] (`AllMiniLML6V2`) when absent.
    /// - `embedding_cache_dir` — override for the fastembed model cache
    ///   directory.  Defaults to fastembed's internal default
    ///   (`~/.cache/huggingface/hub` or `$HF_HOME`) when absent.
    ///
    /// **Dimension lock-in.** The embedding dimension is fixed at first
    /// vector insert.  Switching `embedding_model` on an existing
    /// `dbpath` will break vector search; rebuild the dbpath with
    /// `bdsnode --new` to switch models.
    ///
    /// Use [`with_embedding`](Self::with_embedding) to supply a
    /// pre-loaded model directly (used by tests to share one model
    /// across runs).
    pub fn new(config_path: &str) -> Result<Self> {
        // Pre-parse the config so we can resolve the embedding model
        // before we load it.  `with_embedding` re-parses the same file
        // — that's cheap and keeps both entry points self-contained.
        let raw = std::fs::read_to_string(config_path)
            .map_err(|e| err_msg(format!("cannot read config '{config_path}': {e}")))?;
        let cfg = parse_config(&raw)
            .map_err(|e| err_msg(format!("invalid config '{config_path}': {e}")))?;

        let model = match &cfg.embedding_model {
            Some(name) => parse_embedding_model(name)?,
            None       => DEFAULT_EMBEDDING_MODEL,
        };
        let cache_dir = cfg.embedding_cache_dir.as_deref().map(PathBuf::from);

        log::info!(
            "loading embedding model {model:?} (cache_dir={:?})",
            cache_dir.as_deref().map(std::path::Path::display)
        );

        let embedding = EmbeddingEngine::new_with_cache(
            model.clone(),
            cache_dir,
            cfg.embedding_cache_size,
        )
            .map_err(|e| err_msg(format!("failed to load embedding model {model:?}: {e}")))?;

        let mgr = Self::with_embedding(config_path, embedding)?;
        // Stash the resolved name so callers (v2/status) can echo it back
        // without having to re-parse the config or interrogate fastembed.
        let model_name = format!("{model:?}");
        if let Ok(mut slot) = mgr.embedding_model_name.lock() {
            *slot = Some(model_name.clone());
        }
        // Mirror into the cluster layer so `v3/cluster.hello` can advertise
        // it (peers refuse to form when their dimensions don't match).
        if let Some(cluster) = &mgr.cluster {
            cluster.set_embedding_model(Some(model_name));
        }
        Ok(mgr)
    }

    /// Open or create a shard manager with a pre-loaded embedding model.
    ///
    /// Preferred in tests to share a single model instance across test runs.
    pub fn with_embedding(config_path: &str, embedding: EmbeddingEngine) -> Result<Self> {
        let raw = std::fs::read_to_string(config_path)
            .map_err(|e| err_msg(format!("cannot read config '{config_path}': {e}")))?;
        let cfg = parse_config(&raw)
            .map_err(|e| err_msg(format!("invalid config '{config_path}': {e}")))?;

        // Must be called before any StorageEngine is constructed so the shared
        // r2d2 maintenance thread pool is sized correctly.
        crate::storageengine::init_r2d2_thread_pool(cfg.r2d2_thread_pool_size);

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

        let signals_path = format!("{}/signals", cfg.dbpath);
        let signals = DocumentStorage::with_embedding(&signals_path, embedding.clone())?;

        let scripts_path = format!("{}/scripts", cfg.dbpath);
        let scripts = DocumentStorage::with_embedding(&scripts_path, embedding.clone())?;

        let cache = ShardsCache::with_config(
            &cfg.dbpath,
            &cfg.shard_duration,
            cfg.pool_size,
            embedding,
            obs_config,
            cfg.max_open_shards,
        )?;

        let jsoncache = JsonCache::new(
            cfg.jsoncache_capacity,
            Duration::from_secs(cfg.jsoncache_ttl_secs),
        );

        // Cluster layer is opt-in.  Parse the cluster.* sub-block; an
        // absent or `enabled: false` block returns a disabled config and
        // leaves `cluster` as None (zero overhead).
        let cluster_cfg = ClusterConfig::from_hjson_str(&raw)?;
        let cluster = if cluster_cfg.enabled {
            Some(Cluster::init(&cfg.dbpath, cluster_cfg)?)
        } else {
            None
        };

        let mut manager = Self {
            cache,
            docstore,
            signals,
            scripts,
            drain: None,
            drain_cluster_map: Arc::new(Mutex::new(HashMap::new())),
            jsoncache,
            // `with_embedding` doesn't know which model variant produced the
            // engine — `Self::new` populates this slot after construction.
            embedding_model_name: Arc::new(std::sync::Mutex::new(None)),
            cluster,
            add_batch_lock: Arc::new(std::sync::Mutex::new(())),
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

        // Serialise concurrent callers — DuckDB + Tantivy + HNSW are not
        // safe under multiple simultaneous writers to the same shard, and
        // bdsnode's parallel flusher pool (`pipe_flushers > 1`) plus
        // direct `v2/add` calls can race here.  The internal Rayon
        // par_iter still parallelises across shards within ONE call, so
        // holding this lock doesn't sacrifice the meaningful concurrency.
        // Poisoning is fatal (a panic happened mid-write); surfacing it
        // as an error rather than silently re-poisoning is the safer
        // path because subsequent writes may have nothing to write to.
        let _guard = self.add_batch_lock.lock()
            .map_err(|e| err_msg(format!("add_batch lock poisoned: {e}")))?;

        // ── drain: fast DuckDB-only writes, no ONNX inside the lock ─────────────
        struct PendingTpl {
            id: Uuid,
            shard_ts: SystemTime,
            meta: JsonValue,
            body: Vec<u8>,
        }
        let mut pending_tpls: Vec<PendingTpl> = Vec::new();
        let mut matched_cluster_ids: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        if let Some(drain) = &self.drain {
            let mut new_mappings: Vec<(usize, Uuid)> = Vec::new();
            if let Ok(mut parser) = drain.lock() {
                for doc in &docs {
                    if let Ok(r) = parser.parse_json_with_callback(doc, |meta, body| {
                        // Store to DuckDB only — defer ONNX embedding to after the lock.
                        let ts = extract_timestamp(&meta)
                            .unwrap_or_else(|_| SystemTime::now());
                        let shard = self.cache.shard(ts)?;
                        let id = shard.tpl_add_no_embed(meta.clone(), &body)?;
                        pending_tpls.push(PendingTpl { id, shard_ts: ts, meta, body });
                        Ok(id)
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

        // ── batch-embed all new templates in one ONNX pass (outside drain lock) ─
        if !pending_tpls.is_empty() {
            let shard_dur = self.cache.shard_duration();
            let mut by_shard: HashMap<SystemTime, Vec<(Uuid, JsonValue, Vec<u8>)>> =
                HashMap::new();
            for tpl in pending_tpls {
                let (aligned, _) =
                    crate::common::timerange::align_to_duration(tpl.shard_ts, shard_dur)?;
                by_shard
                    .entry(aligned)
                    .or_default()
                    .push((tpl.id, tpl.meta, tpl.body));
            }
            for (shard_start, entries) in &by_shard {
                let shard = self.cache.shard(*shard_start)?;
                shard.tplstorage.embed_documents_batch(entries)?;
                // One sync per shard (not per template).
                shard.tplstorage.sync()?;
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

        // ── partition tagged docs into per-shard groups ───────────────────────
        // Each group owns its docs so the rayon parallel pass below can take
        // them without borrowing across threads.
        struct ShardGroup {
            shard_start:  SystemTime,
            orig_indices: Vec<usize>,
            docs:         Vec<JsonValue>,
        }
        let mut groups: Vec<ShardGroup> = Vec::new();
        {
            let mut group_start = 0;
            while group_start < tagged.len() {
                let current_start = tagged[group_start].shard_start;
                let group_end = tagged[group_start..]
                    .partition_point(|t| t.shard_start == current_start)
                    + group_start;
                let span = &tagged[group_start..group_end];
                let mut g = ShardGroup {
                    shard_start:  current_start,
                    orig_indices: Vec::with_capacity(span.len()),
                    docs:         Vec::with_capacity(span.len()),
                };
                for t in span {
                    g.orig_indices.push(t.orig_idx);
                    g.docs.push(t.doc.clone());
                }
                groups.push(g);
                group_start = group_end;
            }
        }

        // ── open every shard upfront (cache lookup is cheap and serialises
        //    on a single mutex; doing it in parallel buys nothing and risks
        //    contention on the cache) ──────────────────────────────────────────
        let mut opened: Vec<(crate::shard::Shard, ShardGroup)> =
            Vec::with_capacity(groups.len());
        let mut first_shard: Option<crate::shard::Shard> = None;
        for g in groups {
            let shard = self.cache.shard(g.shard_start)?;
            if first_shard.is_none() {
                first_shard = Some(shard.clone());
            }
            opened.push((shard, g));
        }

        // ── per-shard add_batch in parallel ───────────────────────────────────
        // Each Shard's storage engines (DuckDB pool, Tantivy index, VecStore)
        // are independent, so concurrent writes don't contend on shared state.
        // The only cross-shard work is the per-doc jsoncache insert, which is
        // done sequentially below to avoid making the parallel closure return
        // a tuple of (id, full doc) for every document.
        struct ShardOutcome {
            orig_indices: Vec<usize>,
            docs:         Vec<JsonValue>,
            ids:          Vec<Uuid>,
        }
        let per_shard: Vec<Result<ShardOutcome>> = opened
            .into_par_iter()
            .map(|(shard, g)| {
                // `add_batch` consumes the docs Vec; re-clone for cache replay.
                let cache_docs = g.docs.clone();
                let ids = shard.add_batch(g.docs)?;
                Ok(ShardOutcome {
                    orig_indices: g.orig_indices,
                    docs:         cache_docs,
                    ids,
                })
            })
            .collect();

        // ── hoist results back into input order, populate jsoncache ───────────
        let mut result_ids = vec![Uuid::nil(); tagged.len()];
        for outcome in per_shard {
            let outcome = outcome?;
            for ((orig_idx, id), doc) in outcome
                .orig_indices
                .into_iter()
                .zip(outcome.ids.into_iter())
                .zip(outcome.docs.into_iter())
            {
                result_ids[orig_idx] = id;
                let ts_u64 = doc["timestamp"].as_u64().unwrap_or(0);
                let mut cached_doc = doc;
                cached_doc["id"] = serde_json::json!(id.to_string());
                self.jsoncache.insert(id.to_string(), ts_u64, cached_doc);
            }
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

        let infos = self.cache.info().shards_in_range(start, end)?;
        let mut shards: Vec<crate::shard::Shard> = Vec::with_capacity(infos.len());
        for info in infos {
            shards.push(self.cache.shard(info.start_time)?);
        }

        // Per-shard FTS in parallel — matches the vector-search pattern.
        // Tantivy reads are reentrant; each shard owns an independent
        // index, so Rayon parallelism is safe.
        let per_shard: Vec<Vec<(Uuid, f32)>> = shards
            .par_iter()
            .map(|shard| shard.search_fts_scored(query, limit))
            .collect::<Result<Vec<_>>>()?;

        let total: usize = per_shard.iter().map(|v| v.len()).sum();
        let mut results: Vec<(Uuid, f32)> = Vec::with_capacity(total);
        for v in per_shard {
            results.extend(v);
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

        let infos = self.cache.info().shards_in_range(start, end)?;
        let mut shards: Vec<crate::shard::Shard> = Vec::with_capacity(infos.len());
        for info in infos {
            shards.push(self.cache.shard(info.start_time)?);
        }

        let per_shard: Vec<Vec<(Uuid, i64, f32)>> = shards
            .par_iter()
            .map(|shard| shard.search_fts_with_ts(query, limit))
            .collect::<Result<Vec<_>>>()?;

        let total: usize = per_shard.iter().map(|v| v.len()).sum();
        let mut results: Vec<(Uuid, i64, f32)> = Vec::with_capacity(total);
        for v in per_shard {
            results.extend(v);
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
        self.vectorsearch_with_vec(duration, &fingerprint, &query_vec, limit)
    }

    /// Like [`vectorsearch`] but accepts a pre-computed embedding vector
    /// + the matching fingerprint string.  Used by the cluster query
    /// coordinator (`v3/search`) so the heavy ONNX inference happens
    /// **once** on the entry node — peers run the same merge/rank
    /// pipeline against the already-embedded vector and skip the
    /// ~5-15 ms per-call embed cost.
    ///
    /// Errors only if the catalog lookup or any per-shard search fails
    /// — embedding is not retried here.
    pub fn vectorsearch_with_vec(
        &self,
        duration:    &str,
        fingerprint: &str,
        query_vec:   &[f32],
        limit:       usize,
    ) -> Result<Vec<(Uuid, i64, f32)>> {
        let (start, end) = lookback_window(duration)?;

        let infos = self.cache.info().shards_in_range(start, end)?;
        let mut shards: Vec<crate::shard::Shard> = Vec::with_capacity(infos.len());
        for info in infos {
            shards.push(self.cache.shard(info.start_time)?);
        }

        let per_shard: Vec<Vec<(Uuid, i64, f32)>> = shards
            .par_iter()
            .map(|shard| {
                shard.search_vector_scored_precomputed(query_vec, fingerprint, limit)
            })
            .collect::<Result<Vec<_>>>()?;

        let total: usize = per_shard.iter().map(|v| v.len()).sum();
        let mut results: Vec<(Uuid, i64, f32)> = Vec::with_capacity(total);
        for v in per_shard {
            results.extend(v);
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

        let infos = self.cache.info().shards_in_range(start, end)?;
        let mut shards: Vec<crate::shard::Shard> = Vec::with_capacity(infos.len());
        for info in infos {
            shards.push(self.cache.shard(info.start_time)?);
        }

        let per_shard: Vec<Vec<JsonValue>> = shards
            .par_iter()
            .map(|shard| shard.search_vector_precomputed(&query_vec, &fingerprint, limit))
            .collect::<Result<Vec<_>>>()?;

        let total: usize = per_shard.iter().map(|v| v.len()).sum();
        let mut results: Vec<JsonValue> = Vec::with_capacity(total);
        for shard_results in per_shard {
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

        let infos = self.cache.info().shards_in_range(start, end)?;
        let mut shards: Vec<crate::shard::Shard> = Vec::with_capacity(infos.len());
        for info in infos {
            shards.push(self.cache.shard(info.start_time)?);
        }

        let per_shard: Vec<Vec<JsonValue>> = shards
            .par_iter()
            .map(|shard| shard.search_vector_precomputed(&query_vec, &fingerprint, 100))
            .collect::<Result<Vec<_>>>()?;

        let total: usize = per_shard.iter().map(|v| v.len()).sum();
        let mut results: Vec<JsonValue> = Vec::with_capacity(total);
        for shard_results in per_shard {
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

    // ── retention / shard eviction ────────────────────────────────────────────
    //
    // Online eviction is the 5-step procedure documented in
    // `Documentation/RETENTION.md`.  The catalog `evicting` flag is
    // flipped first so any concurrent ingest racing us either hits the
    // shard before the cache entry is dropped (the write succeeds, but
    // is lost on delete — operator chose retention < shard_duration is
    // their problem) or sees the catalog say "evicting" and falls back
    // upstream.
    //
    // POSIX `remove_dir_all` is safe even with FDs still open against
    // the directory's children: inodes survive until the last close, so
    // any in-flight query against the shard completes against the
    // about-to-be-unlinked files.  Windows would need a refcount wait;
    // bdslib is POSIX-only.

    /// Evict a single shard online — closes its cached instance,
    /// removes the row from the catalog, and deletes the shard's
    /// directory.  Idempotent on a missing shard.  Returns the
    /// reclaimed bytes (best-effort `du`) plus the original time
    /// window so the caller can do post-eviction cache invalidation.
    pub fn evict_shard(&self, shard_id: Uuid) -> Result<EvictionOutcome> {
        let info = match self.cache.info().get_by_id(shard_id)? {
            Some(i) => i,
            None    => return Ok(EvictionOutcome::not_found(shard_id)),
        };
        let path = info.path.clone();

        // 1) Flag the row.  Racing opens hit the is_evicting guard in
        //    ShardsCache::shard() and refuse.
        self.cache.info().mark_evicting(shard_id)?;

        // 2) Close the open Shard (if any).  Sync first so on-disk
        //    state is consistent with the in-memory state we're about
        //    to throw away.  No-op when the shard isn't cached.
        let key = (info.start_time, info.end_time);
        if let Err(e) = self.cache.close_if_open(key) {
            log::warn!("[retention] close_if_open failed for shard {shard_id} \
                        ({path}): {e}; continuing with eviction");
        }

        // 3) Move the shard dir aside.  If the process crashes between
        //    here and step 5, startup discovery
        //    (`cleanup_orphan_evicting`) walks the catalog for any
        //    `evicting=true` rows, removes their .evicting directories,
        //    and deletes the rows.
        let evict_path = format!("{path}.evicting");
        if std::path::Path::new(&path).exists() {
            if let Err(e) = std::fs::rename(&path, &evict_path) {
                // Roll back the flag — the shard is unharmed.
                let _ = self.cache.info().mark_evicting(shard_id);  // re-mark is harmless
                return Err(err_msg(format!(
                    "[retention] cannot rename {path} → {evict_path}: {e}"
                )));
            }
        }

        let freed_bytes = du_secs(&evict_path).unwrap_or(0);

        // 4) Delete the catalog row.
        self.cache.info().delete_by_id(shard_id)?;

        // 5) Recursively unlink the on-disk shard data.  Failure here
        //    leaves orphan files but the catalog is already clean —
        //    operators see the warning and can rm -rf manually.
        if let Err(e) = std::fs::remove_dir_all(&evict_path) {
            log::warn!("[retention] catalog row for shard {shard_id} deleted but \
                        on-disk dir {evict_path} could not be removed: {e}");
        }

        Ok(EvictionOutcome {
            shard_id,
            path,
            start_time: info.start_time,
            end_time:   info.end_time,
            freed_bytes,
            existed:    true,
        })
    }

    /// Discover and finish any half-completed evictions from a prior
    /// run.  Walks the catalog for `evicting=true` rows, removes their
    /// `.evicting` directories from disk, and deletes the catalog rows.
    /// Returns the number of orphans cleaned up.
    ///
    /// Safe to call on every startup; no-op when there are no orphans.
    /// MUST run BEFORE the first regular shard open so a racing ingest
    /// doesn't get pointed at an `evicting=true` row.
    pub fn cleanup_orphan_evicting(&self) -> Result<usize> {
        let orphans = self.cache.info().list_evicting()?;
        let n = orphans.len();
        for info in orphans {
            let evict_path = format!("{}.evicting", info.path);
            if std::path::Path::new(&evict_path).exists() {
                if let Err(e) = std::fs::remove_dir_all(&evict_path) {
                    log::warn!(
                        "[retention] startup orphan cleanup: cannot remove \
                         {evict_path}: {e}; leaving catalog row {} as-is",
                        info.shard_id,
                    );
                    continue;
                }
            }
            // Also unlink the original path in case the rename never
            // happened (crashed between step 1 and step 3).
            if std::path::Path::new(&info.path).exists() {
                if let Err(e) = std::fs::remove_dir_all(&info.path) {
                    log::warn!(
                        "[retention] startup orphan cleanup: cannot remove \
                         {}: {e}; leaving catalog row {} as-is",
                        info.path, info.shard_id,
                    );
                    continue;
                }
            }
            self.cache.info().delete_by_id(info.shard_id)?;
            log::info!(
                "[retention] startup orphan cleanup: dropped shard {} ({})",
                info.shard_id, info.path,
            );
        }
        if n > 0 {
            log::info!("[retention] startup cleanup removed {n} orphan shard(s)");
        }
        Ok(n)
    }

    /// Borrow the embedded [`JsonCache`].  Used by the retention sweeper
    /// to invalidate entries whose `(_, timestamp)` falls in an evicted
    /// shard's window.
    pub fn jsoncache(&self) -> &JsonCache {
        &self.jsoncache
    }

    /// Re-seed the in-memory drain parser from the templates currently
    /// stored across surviving shards.  Called after a retention sweep
    /// that touched any shard inside the configured `drain_load_duration`
    /// window — without this, the parser holds in-memory cluster IDs
    /// whose backing template UUIDs have been deleted from disk.
    ///
    /// No-op when drain is disabled (`drain_enabled = false`) or when
    /// the lock has been poisoned by a panic elsewhere.  Returns the
    /// number of clusters the fresh parser was seeded with.
    pub fn drain_reload(&self, duration: &str) -> Result<usize> {
        let Some(drain_arc) = self.drain.as_ref() else {
            return Ok(0);  // drain disabled — nothing to reseed
        };
        let (fresh_parser, fresh_cluster_map) = self.drain_load(duration)?;
        let n = fresh_parser.clusters().len();
        if let Ok(mut g) = drain_arc.lock() {
            *g = fresh_parser;
        }
        if let Ok(mut g) = self.drain_cluster_map.lock() {
            *g = fresh_cluster_map;
        }
        Ok(n)
    }

    // ── rebalancer support ────────────────────────────────────────────────────

    /// Return the subset of `ids` that exist as a primary record on
    /// this node (across any catalog-registered shard).
    ///
    /// Backs `v2/cluster.has_records` for the rebalancer.  Performs
    /// one DuckDB query per shard the candidate IDs fall into — the
    /// implementation is deliberately naive (no bloom filters, no
    /// per-shard partitioning) because the rebalancer's batch size
    /// is small (default 50) and the read is throttled.
    pub fn has_records_present(&self, ids: &[Uuid]) -> Result<Vec<Uuid>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Probe every shard.  An ID's natural shard is determined by
        // its v7 timestamp, but we don't pre-compute that here — a
        // missed shard would silently report "not present" and trigger
        // unnecessary replication, which is wasteful but not incorrect.
        let mut present: Vec<Uuid> = Vec::new();
        for info in self.cache.info().list_all()? {
            let shard = self.cache.shard(info.start_time)?;
            let docs  = shard.observability().get_by_ids(ids)?;
            for d in docs {
                if let Some(id) = d.get("id").and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    present.push(id);
                }
            }
        }
        // De-dup in case the same ID ever lands in two shards (should
        // not happen, but defensive).
        present.sort();
        present.dedup();
        Ok(present)
    }

    /// Idempotently store a replicated record received from a peer.
    ///
    /// Accepts a doc that carries an `id` field; checks
    /// [`has_records_present`] first and returns `(id, false)` when
    /// the record is already on this node.  Otherwise inserts via
    /// `add_batch` (which itself preserves the doc's `id` field) and
    /// returns `(stored_id, true)`.
    ///
    /// **Idempotent**: re-running with the same record is safe and
    /// reports `was_new=false`.
    pub fn replicate_record(&self, doc: JsonValue) -> Result<(Uuid, bool)> {
        let id = doc.get("id").and_then(|v| v.as_str())
            .ok_or_else(|| err_msg("replicate_record: doc missing 'id' field"))?;
        let id = Uuid::parse_str(id)
            .map_err(|e| err_msg(format!("replicate_record: invalid id: {e}")))?;

        let present = self.has_records_present(&[id])?;
        if !present.is_empty() {
            return Ok((id, false));
        }
        let ids = self.add_batch(vec![doc])?;
        let stored = ids.into_iter().next().unwrap_or(id);
        Ok((stored, true))
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

    /// Resolved name of the embedding model loaded into this manager,
    /// e.g. `"AllMiniLML6V2"` or `"BGESmallENV15"`.
    ///
    /// Returns `None` when the manager was constructed via
    /// [`Self::with_embedding`] (the model identity is opaque to that
    /// constructor — it accepts a pre-loaded `EmbeddingEngine` and has no
    /// way to introspect which variant it came from).  In production
    /// (anything that goes through [`Self::new`]) this always returns
    /// `Some`; tests typically use `with_embedding` and see `None`.
    ///
    /// Used by `v2/status` so operators can confirm which model is
    /// currently loaded without re-parsing the config file.
    pub fn embedding_model_name(&self) -> Option<String> {
        self.embedding_model_name
            .lock()
            .ok()
            .and_then(|g| g.clone())
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

    /// Borrow the cluster layer.  `Some` only when `cluster.enabled = true`
    /// in the originating `bds.hjson`; `None` for stand-alone deployments.
    pub fn cluster(&self) -> Option<&Arc<Cluster>> {
        self.cluster.as_ref()
    }
}

/// Best-effort `du -sb`-style recursive size for a path.  Returns 0 on
/// any I/O error — this is reporting-only telemetry, never load-bearing.
/// Used by `evict_shard` to populate `EvictionOutcome::freed_bytes` so
/// `v2/status` can show how much disk the last sweep reclaimed.
fn du_secs(path: &str) -> Option<u64> {
    fn walk(p: &std::path::Path, total: &mut u64) -> std::io::Result<()> {
        let md = std::fs::symlink_metadata(p)?;
        if md.file_type().is_dir() {
            for entry in std::fs::read_dir(p)? {
                let entry = entry?;
                walk(&entry.path(), total)?;
            }
        } else {
            *total += md.len();
        }
        Ok(())
    }
    let mut total = 0u64;
    walk(std::path::Path::new(path), &mut total).ok()?;
    Some(total)
}

