use crate::common::error::{err_msg, Result};
use crate::common::timerange::align_to_duration;
use crate::observability::{ObservabilityStorage, ObservabilityStorageConfig};
use crate::shard::Shard;
use crate::shardsinfo::{ShardInfo, ShardInfoEngine};
use crate::EmbeddingEngine;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Result of a [`ShardsCache::rebuild_shard`] attempt.
#[derive(Debug, Clone)]
pub enum RebuildOutcome {
    /// The quarantine was a transient false positive — the shard
    /// re-opened cleanly with no rebuild needed.  Quarantine cleared.
    Transient,
    /// The index directories were corrupt; they were deleted and
    /// rebuilt from DuckDB.  `reindexed` is the primary-record count
    /// replayed into the fresh FTS + vector indexes.  Quarantine cleared.
    Reindexed { reindexed: usize },
    /// DuckDB itself — the source of truth — would not open, so the
    /// shard cannot be rebuilt from local data.  It stays quarantined;
    /// `reason` is the operator-facing explanation.
    Unhealable { reason: String },
}

struct CacheInner {
    map: HashMap<(SystemTime, SystemTime), Shard>,
    /// Keys in most-recently-used-first order.
    lru: VecDeque<(SystemTime, SystemTime)>,
}

/// In-memory cache of open [`Shard`] instances, keyed by their `[start, end)` time interval.
///
/// `ShardsCache` owns a [`ShardInfoEngine`] catalog that records every shard's
/// filesystem path and time interval on disk. The in-memory cache is a fast
/// lookup layer on top of that catalog.
///
/// ## `shard()` lookup order
///
/// 1. In-memory cache — O(1) lookup by aligned `(start, end)` key; returns immediately on hit.
/// 2. [`ShardInfoEngine`] catalog — if the cache misses, the catalog is queried for a
///    shard covering the given timestamp. On a catalog hit the shard is opened from
///    its stored path and inserted into the cache.
/// 3. Auto-create — if neither the cache nor the catalog covers the timestamp, a new
///    shard directory is provisioned at `{root_path}/{start_ts}_{end_ts}`, registered
///    in the catalog, opened, and inserted into the cache.
///
/// Time intervals are aligned to `shard_duration` boundaries relative to the Unix
/// epoch, so all shards of the same duration are non-overlapping and contiguous.
///
/// `ShardsCache` is `Clone`; all clones share the same underlying cache, catalog,
/// and connection pool.
#[derive(Clone)]
pub struct ShardsCache {
    root_path: String,
    shard_duration: Duration,
    pool_size: u32,
    embedding: EmbeddingEngine,
    obs_config: ObservabilityStorageConfig,
    info: ShardInfoEngine,
    cache: Arc<Mutex<CacheInner>>,
    /// Maximum number of shards kept open simultaneously. When exceeded, the
    /// least-recently-used shard is synced and evicted to reclaim file descriptors.
    max_open_shards: usize,
}

impl ShardsCache {
    /// Open or create a shard cache rooted at `root_path` with default
    /// [`ObservabilityStorageConfig`] (similarity threshold `0.85`).
    ///
    /// `shard_duration` is a human-readable duration string parsed by
    /// [`humantime`](https://docs.rs/humantime), e.g. `"1h"`, `"30min"`, `"1day"`.
    ///
    /// The catalog database is stored at `{root_path}/shards_info.db`.
    /// The root directory is created automatically if it does not exist.
    pub fn new(
        root_path: &str,
        shard_duration: &str,
        pool_size: u32,
        embedding: EmbeddingEngine,
    ) -> Result<Self> {
        Self::with_config(
            root_path,
            shard_duration,
            pool_size,
            embedding,
            ObservabilityStorageConfig::default(),
            16,
        )
    }

    /// Open or create a shard cache with a custom [`ObservabilityStorageConfig`].
    ///
    /// `shard_duration` uses the same human-readable format as [`new`](Self::new).
    /// `max_open_shards` caps the number of shards held open at once; the LRU shard
    /// is synced and evicted when the limit is reached.
    pub fn with_config(
        root_path: &str,
        shard_duration: &str,
        pool_size: u32,
        embedding: EmbeddingEngine,
        obs_config: ObservabilityStorageConfig,
        max_open_shards: usize,
    ) -> Result<Self> {
        let duration = humantime::parse_duration(shard_duration).map_err(|e| {
            err_msg(format!(
                "invalid shard_duration '{shard_duration}': {e}"
            ))
        })?;
        if duration.is_zero() {
            return Err(err_msg("shard_duration must be non-zero"));
        }
        std::fs::create_dir_all(root_path)
            .map_err(|e| err_msg(format!("cannot create shard cache root '{root_path}': {e}")))?;
        let info_path = format!("{root_path}/shards_info.db");
        let info = ShardInfoEngine::new(&info_path, pool_size)?;
        Ok(Self {
            root_path: root_path.to_string(),
            shard_duration: duration,
            pool_size,
            embedding,
            obs_config,
            info,
            cache: Arc::new(Mutex::new(CacheInner {
                map: HashMap::new(),
                lru: VecDeque::new(),
            })),
            max_open_shards: max_open_shards.max(1),
        })
    }

    // ── primary API ───────────────────────────────────────────────────────────

    /// Return the [`Shard`] whose interval `[start, end)` covers `timestamp`.
    ///
    /// Lookup order: in-memory cache → catalog → auto-create. See struct-level
    /// documentation for details.
    ///
    /// The returned `Shard` is a cheap clone that shares all underlying resources
    /// with the cached instance.
    pub fn shard(&self, timestamp: SystemTime) -> Result<Shard> {
        let (start, end) = align_to_duration(timestamp, self.shard_duration)?;
        let key = (start, end);

        let mut state = self.cache.lock();

        // 1. In-memory cache hit — promote to MRU position.
        if let Some(shard) = state.map.get(&key) {
            let shard = shard.clone();
            state.lru.retain(|k| k != &key);
            state.lru.push_front(key);
            return Ok(shard);
        }

        // 2. Catalog lookup.
        let infos = self.info.shards_at(timestamp)?;
        let (insert_key, shard) = if let Some(info) = infos.into_iter().next() {
            // Race-guard for the retention subsystem: if the catalog
            // says this shard is being evicted, refuse to open it.  The
            // caller (typically the ingest path) sees this as a
            // transient failure and can fall back to retry / pick a
            // future shard / drop the record.
            if self.info.is_evicting(info.shard_id)? {
                return Err(err_msg(format!(
                    "shard {} ({}) is being evicted",
                    info.shard_id, info.path
                )));
            }
            // Self-healing short-circuit: a quarantined shard has
            // failing storage and is awaiting repair by the rebuild
            // healer.  Refuse to open it so the rest of the node keeps
            // serving — same transient-failure contract as `evicting`.
            if self.info.is_quarantined(info.shard_id)? {
                return Err(err_msg(format!(
                    "shard {} ({}) is quarantined (failing storage — awaiting rebuild)",
                    info.shard_id, info.path
                )));
            }
            // Circuit-breaker fast-fail: when a shard's opens have
            // been failing, the breaker trips Open and every call
            // here returns immediately instead of paying the (up to
            // 10 s) pool-checkout timeout on a doomed open.  After a
            // cooldown the breaker goes HalfOpen and lets the next
            // attempt through as a probe.  Distinct from quarantine:
            // the breaker is a transient latency guard, quarantine is
            // the persistent corruption verdict.
            let health_key = crate::shardhealth::key_of(info.start_time, info.end_time);
            if crate::shardhealth::tracker().breaker_check(health_key)
                == crate::shardhealth::BreakerState::Open
            {
                return Err(err_msg(format!(
                    "shard {} ({}) circuit breaker OPEN — fast-failing \
                     (struggling storage; retry shortly)",
                    info.shard_id, info.path
                )));
            }

            // Try to open the shard's three engines.  An open failure
            // is the unambiguous corruption signal — feed it to the
            // shard-health tracker, which drives both the circuit
            // breaker and the quarantine decision.
            let shard = match Shard::with_config(
                &info.path,
                self.pool_size,
                self.embedding.clone(),
                self.obs_config.clone(),
            ) {
                Ok(s) => {
                    crate::shardhealth::tracker().record_open_success(health_key);
                    s
                }
                Err(e) => {
                    if crate::shardhealth::tracker().record_open_failure(health_key) {
                        // Consecutive open failures crossed the
                        // threshold — quarantine this shard.  The next
                        // `shard()` call short-circuits above; the
                        // rebuild healer will pick it up from
                        // `list_quarantined()`.
                        log::error!(
                            "[shard-health] shard {} ({}) failed to open \
                             {} times — quarantining for rebuild: {e}",
                            info.shard_id, info.path,
                            crate::shardhealth::QUARANTINE_THRESHOLD,
                        );
                        if let Err(qe) = self.info.mark_quarantined(info.shard_id) {
                            log::error!(
                                "[shard-health] failed to mark shard {} quarantined: {qe}",
                                info.shard_id
                            );
                        }
                        crate::health::report(
                            &format!("shard.{}_{}", health_key.0, health_key.1),
                            crate::health::HealthStatus::Failed(format!(
                                "quarantined: {e}"
                            )),
                        );
                    }
                    return Err(e);
                }
            };
            ((info.start_time, info.end_time), shard)
        } else {
            // 3. Auto-create.
            let start_secs = start
                .duration_since(UNIX_EPOCH)
                .map_err(|e| err_msg(format!("shard start predates epoch: {e}")))?
                .as_secs();
            let end_secs = end
                .duration_since(UNIX_EPOCH)
                .map_err(|e| err_msg(format!("shard end predates epoch: {e}")))?
                .as_secs();
            let path = format!("{}/{start_secs}_{end_secs}", self.root_path);
            let shard = Shard::with_config(
                &path,
                self.pool_size,
                self.embedding.clone(),
                self.obs_config.clone(),
            )?;
            self.info.add_shard(&path, start, end)?;
            (key, shard)
        };

        state.map.insert(insert_key, shard.clone());
        state.lru.push_front(insert_key);

        // Evict LRU shards until we're within the open-shard limit —
        // but NEVER evict a shard whose `Shard` is still referenced by
        // a caller's clone.  Dropping the cache's copy would not
        // release the shard's underlying engines (the clones share the
        // same `Arc`s), so a later re-open of the same directory would
        // fight the still-held Tantivy `IndexWriter` lock and fail with
        // `LockBusy`.  An in-use shard is therefore kept (the cap is
        // "soft") and re-checked on the next eviction pass once it goes
        // idle.  `scanned` bounds the loop when every cached shard is
        // in use.
        let mut scanned = 0;
        while state.map.len() > self.max_open_shards && scanned < state.lru.len() {
            let Some(candidate) = state.lru.pop_back() else { break; };
            scanned += 1;
            match state.map.get(&candidate) {
                Some(shard) if shard.is_in_use() => {
                    // Still held outside the cache — can't reclaim it now.
                    state.lru.push_front(candidate);
                }
                Some(_) => {
                    if let Some(evicted) = state.map.remove(&candidate) {
                        let _ = evicted.sync();
                    }
                }
                None => {} // already gone (raced with close_if_open)
            }
        }

        Ok(shard)
    }

    /// Return one [`Shard`] per aligned interval that overlaps `[start_ts, end_ts)`.
    ///
    /// Intervals are enumerated by stepping in `shard_duration` increments starting
    /// from the aligned floor of `start_ts`. Each step calls [`shard`](Self::shard),
    /// so shards are auto-created when not already present.
    ///
    /// Returns an empty `Vec` when `start_ts >= end_ts`.
    pub fn shards_span(
        &self,
        start_ts: SystemTime,
        end_ts: SystemTime,
    ) -> Result<Vec<Shard>> {
        if start_ts >= end_ts {
            return Ok(vec![]);
        }
        let (mut cursor, _) = align_to_duration(start_ts, self.shard_duration)?;
        let mut shards = Vec::new();
        while cursor < end_ts {
            shards.push(self.shard(cursor)?);
            cursor += self.shard_duration;
        }
        Ok(shards)
    }

    /// Return one [`Shard`] per aligned interval that overlaps the window
    /// `[now, now + duration)`.
    ///
    /// `duration` uses the same human-readable format as the constructor
    /// (e.g. `"1h"`, `"30min"`, `"2days"`).
    ///
    /// This is a convenience wrapper around [`shards_span`](Self::shards_span).
    pub fn current(&self, duration: &str) -> Result<Vec<Shard>> {
        let dur = humantime::parse_duration(duration)
            .map_err(|e| err_msg(format!("invalid duration '{duration}': {e}")))?;
        let now = SystemTime::now();
        self.shards_span(now, now + dur)
    }

    /// Flush all cached shards to disk.
    ///
    /// All shards are attempted; the first error encountered is returned after
    /// the remaining shards have been synced.
    ///
    /// The cache lock is held only for the initial snapshot — not across the
    /// DuckDB CHECKPOINT calls — so concurrent shard lookups are not blocked
    /// during the flush.
    pub fn sync(&self) -> Result<()> {
        let shards: Vec<Shard> = self.cache.lock().map.values().cloned().collect();
        let mut first_err: Option<String> = None;
        for shard in &shards {
            if let Err(e) = shard.sync() {
                first_err.get_or_insert_with(|| e.to_string());
            }
        }
        match first_err {
            None => Ok(()),
            Some(msg) => Err(err_msg(msg)),
        }
    }

    /// Flush a single shard to disk and remove it from the in-memory cache.
    ///
    /// Idempotent: if the key is not currently cached, returns `Ok(())`
    /// without touching anything.  Used by the retention subsystem to
    /// drop the cached [`Shard`] instance before the on-disk shard
    /// directory is renamed + deleted.
    ///
    /// Note: as with [`close`](Self::close), the underlying engine
    /// resources (DuckDB pool, Tantivy IndexWriter lock, VecStore index)
    /// are only released when every cloned [`Shard`] returned by prior
    /// calls is also dropped.  On POSIX this is fine — `remove_dir_all`
    /// succeeds even with open FDs (the inodes survive until the last
    /// close).  On Windows the retention subsystem would have to poll
    /// for the refcount to settle, but bdslib is POSIX-only.
    pub fn close_if_open(&self, key: (std::time::SystemTime, std::time::SystemTime)) -> Result<()> {
        let mut state = self.cache.lock();
        let shard = match state.map.remove(&key) {
            Some(s) => s,
            None    => return Ok(()),  // not cached → nothing to do
        };
        state.lru.retain(|k| k != &key);
        drop(state);  // release the cache lock before the (potentially slow) sync
        shard.sync()
    }

    /// Attempt to repair a quarantined shard — the self-healing rebuild
    /// path (Phase 2).  Called by the shard-rebuild healer task for
    /// every entry in [`ShardInfoEngine::list_quarantined`].
    ///
    /// Two tiers, cheapest first:
    ///
    /// 1. **Transient retry.**  Re-open the shard normally.  Many
    ///    quarantines are false positives — a momentary pool
    ///    saturation or fs hiccup that crossed the failure threshold.
    ///    If the shard opens cleanly now, the quarantine is cleared
    ///    with no further action.
    /// 2. **Index rebuild.**  If the re-open still fails, probe DuckDB
    ///    alone.  When DuckDB opens (so the source of truth is intact)
    ///    the corruption is in the Tantivy / HNSW index directories:
    ///    delete `{path}/fts` and `{path}/vec`, re-open the shard
    ///    (which recreates them empty), and replay every primary
    ///    record from DuckDB via [`Shard::rebuild_indexes`].  When
    ///    DuckDB itself won't open, the shard cannot self-heal from
    ///    local data — it stays quarantined and the outcome reports
    ///    `Unhealable` for the operator (or a future peer-rebuild
    ///    phase) to act on.
    ///
    /// On success the catalog `quarantined` flag is cleared and the
    /// shard-health tracker is reset, so the shard rejoins the normal
    /// read/write path on the next access.
    pub fn rebuild_shard(&self, info: &ShardInfo) -> Result<RebuildOutcome> {
        let key = (info.start_time, info.end_time);
        let health_key = crate::shardhealth::key_of(info.start_time, info.end_time);

        // Defensive: drop any stale cached instance for this key.
        let _ = self.close_if_open(key);

        // ── Tier 1: transient retry ───────────────────────────────────
        // A shard can be quarantined for two reasons: it failed to
        // *open* (corruption), or it opened fine but its engines have
        // *drifted* (the Phase-3 consistency sweep).  Tier-1 only
        // resolves the first kind — so it must verify consistency
        // before declaring victory, otherwise a drift quarantine
        // would be "healed" with the drift still in place.
        match Shard::with_config(
            &info.path, self.pool_size,
            self.embedding.clone(), self.obs_config.clone(),
        ) {
            Ok(shard) => {
                match shard.consistency_check() {
                    Ok(cc) if cc.consistent => {
                        // Opened cleanly AND consistent — a genuine
                        // transient false positive.  Done.
                        self.info.clear_quarantined(info.shard_id)?;
                        crate::shardhealth::tracker().clear(health_key);
                        return Ok(RebuildOutcome::Transient);
                    }
                    Ok(cc) => {
                        log::warn!(
                            "[shard-healer] shard {} opens but engines drifted \
                             (duckdb={} fts={} hnsw={}) — rebuilding indexes",
                            info.shard_id,
                            cc.primary_count, cc.fts_count, cc.vector_count,
                        );
                        // fall through to Tier-2 (index rebuild)
                    }
                    Err(e) => {
                        log::warn!(
                            "[shard-healer] shard {} opened but consistency \
                             check failed ({e}); attempting index rebuild",
                            info.shard_id
                        );
                        // fall through to Tier-2
                    }
                }
            }
            Err(open_err) => {
                log::warn!(
                    "[shard-healer] shard {} re-open still failing ({open_err}); \
                     attempting index rebuild",
                    info.shard_id
                );
            }
        }

        // ── Tier 2: is DuckDB (the source of truth) intact? ───────────
        let obs_path = format!("{}/obs.db", info.path);
        if let Err(db_err) = ObservabilityStorage::with_config(
            &obs_path, self.pool_size,
            self.embedding.clone(), self.obs_config.clone(),
        ) {
            // DuckDB itself is corrupt — nothing local can rebuild it.
            return Ok(RebuildOutcome::Unhealable {
                reason: format!("DuckDB at {obs_path} will not open: {db_err}"),
            });
        }

        // DuckDB is fine → the corruption is in fts/ and/or vec/.
        // Delete both index directories so the next open recreates
        // them empty, then replay from DuckDB.
        let fts_dir = format!("{}/fts", info.path);
        let vec_dir = format!("{}/vec", info.path);
        for dir in [&fts_dir, &vec_dir] {
            if std::path::Path::new(dir).exists() {
                std::fs::remove_dir_all(dir).map_err(|e| {
                    err_msg(format!("rebuild: cannot remove corrupt index dir {dir}: {e}"))
                })?;
            }
        }

        // Re-open with the index dirs gone — `FTSEngine::new` /
        // `VectorEngine::new` recreate them empty — then replay.
        let shard = Shard::with_config(
            &info.path, self.pool_size,
            self.embedding.clone(), self.obs_config.clone(),
        ).map_err(|e| {
            err_msg(format!(
                "rebuild: shard {} still fails to open after clearing index dirs: {e}",
                info.shard_id
            ))
        })?;
        let reindexed = shard.rebuild_indexes()?;
        shard.sync()?;

        self.info.clear_quarantined(info.shard_id)?;
        crate::shardhealth::tracker().clear(health_key);
        Ok(RebuildOutcome::Reindexed { reindexed })
    }

    /// Tier-3 escalation: **destroy** a failed shard and recreate it
    /// empty for the same `[start, end)` interval.
    ///
    /// Used by the shard healer when a shard has been *unhealable*
    /// (DuckDB itself corrupt — no local rebuild possible) for longer
    /// than the configured window AND the operator opted into
    /// `recreate_failed_shards` AND the cluster rebalancer is enabled.
    /// The empty shard is a valid, healthy (if empty) member of the
    /// catalog again; peers' rebalancers then push the missing records
    /// back into it via `v2/cluster.replicate_record`.
    ///
    /// **This is destructive** — the failed shard's local data is gone
    /// after this call.  It is only safe because the precondition is
    /// "we are in a cluster whose other nodes hold replicas, and the
    /// rebalancer will restore them".  The healer enforces those
    /// preconditions before calling this; the method itself just does
    /// the mechanical delete + recreate.
    ///
    /// Procedure:
    /// 1. Drop any cached instance.
    /// 2. Delete the catalog row **first** — so the recreate's
    ///    `shard()` call doesn't short-circuit on the stale
    ///    `quarantined` flag.
    /// 3. `remove_dir_all` the on-disk shard directory.
    /// 4. Clear the shard-health tracking record.
    /// 5. `shard()` for a timestamp inside the interval — with no
    ///    catalog row present this hits the auto-create path,
    ///    provisioning a fresh empty shard + catalog row at the same
    ///    path and validating its three engines.
    pub fn recreate_shard(&self, info: &ShardInfo) -> Result<()> {
        let key = (info.start_time, info.end_time);
        let health_key = crate::shardhealth::key_of(info.start_time, info.end_time);

        // 1. drop the cached (broken) instance, if any.
        let _ = self.close_if_open(key);

        // 2. delete the catalog row before touching disk, so a
        //    concurrent reader either sees the old row (and fails to
        //    open the about-to-be-deleted dir — transient) or sees no
        //    row (and the auto-create in step 5 wins the race).
        self.info.delete_by_id(info.shard_id)?;

        // 3. remove the on-disk state.  POSIX `remove_dir_all` is safe
        //    even with FDs still open against the files.
        if std::path::Path::new(&info.path).exists() {
            std::fs::remove_dir_all(&info.path).map_err(|e| {
                err_msg(format!(
                    "recreate: cannot remove failed shard dir {}: {e}",
                    info.path
                ))
            })?;
        }

        // 4. forget all failure history for this interval.
        crate::shardhealth::tracker().clear(health_key);

        // 5. recreate: `shard()` finds no catalog row covering this
        //    timestamp and takes the auto-create branch, provisioning
        //    a fresh empty shard for the SAME interval.  `with_config`
        //    eagerly probes all three engines, so a recreate that
        //    can't even stand up a fresh shard surfaces as an error
        //    here rather than silently leaving a gap.
        self.shard(info.start_time)?;
        Ok(())
    }

    /// Flush all cached shards to disk and evict them from the in-memory cache.
    ///
    /// After `close` the cache is empty. The catalog and on-disk shard data are
    /// unaffected; a subsequent [`shard`](Self::shard) call will reopen from disk.
    ///
    /// Note: underlying engine resources (IndexWriter lock, connection pool) are
    /// released only when all clones of the evicted [`Shard`]s are dropped by
    /// callers.
    pub fn close(&self) -> Result<()> {
        let mut state = self.cache.lock();
        let mut first_err: Option<String> = None;
        for shard in state.map.values() {
            if let Err(e) = shard.sync() {
                first_err.get_or_insert_with(|| e.to_string());
            }
        }
        state.map.clear();
        state.lru.clear();
        match first_err {
            None => Ok(()),
            Some(msg) => Err(err_msg(msg)),
        }
    }

    // ── accessors ─────────────────────────────────────────────────────────────

    /// Borrow the underlying [`ShardInfoEngine`] catalog.
    pub fn info(&self) -> &ShardInfoEngine {
        &self.info
    }

    /// Borrow the shared [`EmbeddingEngine`].
    ///
    /// Used by callers that want to embed a query once and pass the resulting
    /// vector to multiple per-shard searches.
    pub fn embedding(&self) -> &EmbeddingEngine {
        &self.embedding
    }

    /// Return the configured shard width.
    pub fn shard_duration(&self) -> Duration {
        self.shard_duration
    }

    /// Return the number of shards currently in the in-memory cache.
    pub fn cached_count(&self) -> usize {
        self.cache.lock().map.len()
    }
}
