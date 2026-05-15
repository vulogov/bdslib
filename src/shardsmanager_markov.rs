//! Semi-Markov log/event projection over recent primary records.
//!
//! [`ShardsManager::project_logs_recent`] is a thin shard-aware wrapper
//! around [`crate::analysis::markov::markov_project_timed_with`].  It
//! walks every shard that overlaps `[now − lookback, now)`, pulls
//! `(ts, fingerprint)` for every primary record found there, and feeds
//! the result into the projection.
//!
//! Why fingerprint instead of an arbitrary text body? Same rationale as
//! the n-gram pipeline: [`crate::common::jsonfingerprint::json_fingerprint`]
//! exposes both schema (field names) and payload (values) as a stable
//! string, which is the right unit for drain3 templating — the default
//! bucketing for [`crate::analysis::markov`].  Records that share a
//! schema with different values cluster correctly; records with
//! genuinely different schemas land in distinct clusters.
//!
//! For the cluster-wide path see `v3/project_logs` and the helper
//! [`collect_fingerprints_with_ts_in_recent`], which mirrors the
//! `fingerprints_with_ids_in_recent` primitive that `v3/knn`,
//! `v3/anomaly.recent`, `v3/denoise.recent` already use.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use uuid::Uuid;

use crate::analysis::markov::{
    markov_project_timed_with, MarkovProjectionConfig, ProjectedEvent,
};
use crate::common::error::Result;
use crate::common::jsonfingerprint::json_fingerprint;
use crate::shard::Shard;
use crate::shardsmanager::ShardsManager;

impl ShardsManager {
    /// Project the next likely events over `project_forward`, using a
    /// semi-Markov chain trained on the primary records in
    /// `[now − lookback, now)`.  See [`MarkovProjectionConfig`] for
    /// tuning knobs (order, n_samples, min_consensus, …).
    ///
    /// Inter-arrival times are learned from the empirical gaps observed
    /// in the lookback window — much more realistic than the untimed
    /// fallback that assumes a Poisson process at `events_per_second`.
    pub fn project_logs_recent(
        &self,
        lookback:         Duration,
        project_forward:  &str,
        cfg:              &MarkovProjectionConfig,
    ) -> Result<Vec<ProjectedEvent>> {
        let triples = self.collect_fingerprints_with_ts_in_recent(lookback)?;
        let events: Vec<(i64, String)> = triples
            .into_iter()
            .map(|(_, ts, fp)| (ts, fp))
            .collect();
        Ok(markov_project_timed_with(&events, project_forward, cfg))
    }

    /// Walk every shard that overlaps `[now − lookback, now)` and return
    /// `(uuid, ts, fingerprint)` triples for every primary record found.
    ///
    /// Used by `v2/fingerprints.recent_timed` so the cluster-wide
    /// `v3/project_logs` can fan out, dedupe by UUID, and run the
    /// Markov projection once on the union of peer corpora.  Same
    /// per-shard parallelism as `fingerprints_with_ids_in_recent`.
    pub fn collect_fingerprints_with_ts_in_recent(
        &self,
        lookback: Duration,
    ) -> Result<Vec<(Uuid, i64, String)>> {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let start_secs = now_secs.saturating_sub(lookback.as_secs());

        let start_st = UNIX_EPOCH + Duration::from_secs(start_secs);
        let end_st   = UNIX_EPOCH + Duration::from_secs(now_secs);

        let infos = self.cache.info().shards_in_range(start_st, end_st)?;
        let mut shards: Vec<Shard> = Vec::with_capacity(infos.len());
        for info in infos {
            shards.push(self.cache.shard(info.start_time)?);
        }

        let collected: Vec<Vec<(Uuid, i64, String)>> = if shards.len() <= 1 {
            shards.iter()
                .map(|s| collect_with_ts(s, start_st, end_st))
                .collect::<Result<Vec<_>>>()?
        } else {
            shards.par_iter()
                .map(|s| collect_with_ts(s, start_st, end_st))
                .collect::<Result<Vec<_>>>()?
        };
        let total: usize = collected.iter().map(|v| v.len()).sum();
        let mut out = Vec::with_capacity(total);
        for v in collected { out.extend(v); }
        Ok(out)
    }
}

fn collect_with_ts(
    shard: &Shard,
    start: SystemTime,
    end:   SystemTime,
) -> Result<Vec<(Uuid, i64, String)>> {
    let rows = shard.observability().list_primaries_with_ts_data_in_range(start, end)?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, ts, key, data) in rows {
        let fp = record_to_fingerprint(&key, &data);
        if !fp.trim().is_empty() {
            out.push((id, ts, fp));
        }
    }
    Ok(out)
}

/// Same recipe used by `shardsmanager_ngram.rs::record_to_fingerprint`:
/// `key` followed by the JSON fingerprint of `data`.  Keeping the two
/// places identical means the n-gram and Markov pipelines see the same
/// strings, so a drain3 cluster trained on one matches the templates
/// the other would mine.
fn record_to_fingerprint(key: &str, data: &serde_json::Value) -> String {
    let data_fp = json_fingerprint(data);
    if data_fp.is_empty() {
        key.to_owned()
    } else {
        format!("{key} {data_fp}")
    }
}
