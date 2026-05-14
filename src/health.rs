//! Process-wide health registry.
//!
//! The reliability work made bdsnode *fault-tolerant* — it survives
//! internal failures (panics caught, flushers respawned, poison
//! records isolated, pools time-bounded).  This module is the spine
//! of the next level — *self-healing*: a place where every subsystem
//! continuously reports whether it is `Healthy`, `Degraded`, or
//! `Failed`, and a heartbeat timestamp proving its background loop is
//! still ticking.
//!
//! It mirrors the [`crate::perf`] registry pattern: an `OnceLock`-backed
//! `DashMap` of named sources, lock-free to read, cheap to update.
//!
//! ## What consumes it
//!
//! - `v2/health` — a dedicated readiness/liveness probe for load
//!   balancers and orchestrators.
//! - `v2/status.health` — the aggregate verdict embedded in the
//!   general status payload.
//! - The shard rebuild healer (Phase 2) reads `Failed` shard sources
//!   to decide what to rebuild.
//!
//! ## Heartbeats vs. status
//!
//! A source carries **two** independent signals:
//!
//! - `status` — the subsystem's own self-assessment (`Healthy` /
//!   `Degraded` / `Failed`).  Set explicitly by the subsystem.
//! - `last_heartbeat` — wall-clock of the last [`heartbeat`] call.
//!   A background loop bumps this every tick; a *stale* heartbeat
//!   means the loop is **hung** — a failure mode `status` alone can't
//!   express, because a hung loop never gets the chance to set
//!   `status = Failed`.
//!
//! [`registry`]'s aggregate verdict folds both: a source is treated
//! as effectively `Failed` when its heartbeat is older than its
//! declared `stale_after`, regardless of its last self-reported
//! `status`.

use dashmap::DashMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A subsystem's self-assessed health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthStatus {
    /// Operating normally.
    Healthy,
    /// Working, but in a reduced or at-risk state — the reason string
    /// is operator-facing (e.g. `"pool checkout timeouts: 4"`).
    Degraded(String),
    /// Not functioning — the reason string explains what broke.
    Failed(String),
}

impl HealthStatus {
    /// Stable lowercase label for JSON / log output.
    pub fn label(&self) -> &'static str {
        match self {
            HealthStatus::Healthy     => "healthy",
            HealthStatus::Degraded(_) => "degraded",
            HealthStatus::Failed(_)   => "failed",
        }
    }

    /// The operator-facing reason, or empty for `Healthy`.
    pub fn reason(&self) -> &str {
        match self {
            HealthStatus::Healthy           => "",
            HealthStatus::Degraded(r)       => r,
            HealthStatus::Failed(r)         => r,
        }
    }

    /// Severity rank — higher is worse.  Used to fold many sources
    /// into one aggregate verdict.
    fn severity(&self) -> u8 {
        match self {
            HealthStatus::Healthy     => 0,
            HealthStatus::Degraded(_) => 1,
            HealthStatus::Failed(_)   => 2,
        }
    }
}

/// One registered subsystem's health record.
#[derive(Debug, Clone)]
pub struct HealthReport {
    /// Source name, e.g. `"ingest.flushers"`, `"cluster.gossip"`,
    /// `"shard.<start>_<end>"`.
    pub name: String,
    /// The subsystem's self-assessment.
    pub status: HealthStatus,
    /// Unix-seconds of the last [`heartbeat`] (or status update).
    pub last_heartbeat: u64,
    /// How long `last_heartbeat` may age before the source is treated
    /// as hung.  `0` disables the staleness check (for sources that
    /// don't run on a loop — they report status explicitly instead).
    pub stale_after_secs: u64,
}

impl HealthReport {
    /// `true` when the heartbeat is older than `stale_after_secs`.
    /// Always `false` when `stale_after_secs == 0` (check disabled).
    pub fn is_stale(&self, now: u64) -> bool {
        self.stale_after_secs != 0
            && now.saturating_sub(self.last_heartbeat) > self.stale_after_secs
    }

    /// Effective status: a stale heartbeat overrides a stale `status`
    /// — a hung loop can't update its own `status`, so the heartbeat
    /// is the source of truth for liveness.
    pub fn effective(&self, now: u64) -> HealthStatus {
        if self.is_stale(now) {
            HealthStatus::Failed(format!(
                "no heartbeat for {}s (stale_after={}s) — task hung?",
                now.saturating_sub(self.last_heartbeat),
                self.stale_after_secs,
            ))
        } else {
            self.status.clone()
        }
    }
}

/// Internal mutable record — `HealthReport` is the read-side snapshot.
struct Source {
    status:           HealthStatus,
    last_heartbeat:   u64,
    stale_after_secs: u64,
}

/// Process-wide registry of named health sources.
pub struct HealthRegistry {
    sources: DashMap<String, Source>,
}

impl HealthRegistry {
    fn new() -> Self {
        Self { sources: DashMap::new() }
    }

    /// Register (or re-register) a source.  Idempotent — a source that
    /// re-registers keeps reporting under the same name.  `stale_after_secs`
    /// of `0` means "no liveness check" (for non-loop sources).
    pub fn register(&self, name: &str, stale_after_secs: u64) {
        self.sources.entry(name.to_owned())
            .and_modify(|s| s.stale_after_secs = stale_after_secs)
            .or_insert_with(|| Source {
                status:           HealthStatus::Healthy,
                last_heartbeat:   now_secs(),
                stale_after_secs,
            });
    }

    /// Record a liveness heartbeat for `name`.  Called from a
    /// background loop every tick.  Creates the source on first call
    /// (with `stale_after_secs = 0` — callers that want staleness
    /// detection should [`register`] explicitly first).
    pub fn heartbeat(&self, name: &str) {
        let now = now_secs();
        self.sources.entry(name.to_owned())
            .and_modify(|s| s.last_heartbeat = now)
            .or_insert_with(|| Source {
                status:           HealthStatus::Healthy,
                last_heartbeat:   now,
                stale_after_secs: 0,
            });
    }

    /// Set a source's self-assessed status (also counts as a
    /// heartbeat — a subsystem that can report status is alive).
    pub fn report(&self, name: &str, status: HealthStatus) {
        let now = now_secs();
        self.sources.entry(name.to_owned())
            .and_modify(|s| { s.status = status.clone(); s.last_heartbeat = now; })
            .or_insert_with(|| Source {
                status,
                last_heartbeat:   now,
                stale_after_secs: 0,
            });
    }

    /// Drop a source — used when a subsystem is permanently retired
    /// (e.g. a shard that was evicted by retention).
    pub fn deregister(&self, name: &str) {
        self.sources.remove(name);
    }

    /// Snapshot every source, sorted by name for stable output.
    pub fn snapshot(&self) -> Vec<HealthReport> {
        let mut out: Vec<HealthReport> = self.sources.iter()
            .map(|e| HealthReport {
                name:             e.key().clone(),
                status:           e.status.clone(),
                last_heartbeat:   e.last_heartbeat,
                stale_after_secs: e.stale_after_secs,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Fold every source's *effective* status into one verdict —
    /// the worst wins.  An empty registry is `Healthy`.
    pub fn verdict(&self) -> HealthStatus {
        let now = now_secs();
        let mut worst = HealthStatus::Healthy;
        let mut worst_sev = 0u8;
        for e in self.sources.iter() {
            let eff = HealthReport {
                name:             e.key().clone(),
                status:           e.status.clone(),
                last_heartbeat:   e.last_heartbeat,
                stale_after_secs: e.stale_after_secs,
            }.effective(now);
            let sev = eff.severity();
            if sev > worst_sev {
                worst_sev = sev;
                worst = match &eff {
                    HealthStatus::Healthy => HealthStatus::Healthy,
                    HealthStatus::Degraded(_) =>
                        HealthStatus::Degraded(format!("{} degraded", e.key())),
                    HealthStatus::Failed(_) =>
                        HealthStatus::Failed(format!("{} failed", e.key())),
                };
            }
        }
        worst
    }
}

static REGISTRY: OnceLock<HealthRegistry> = OnceLock::new();

/// Borrow the process-wide health registry.  Lazily initialised, so
/// library callers that never touch health pay nothing.
pub fn registry() -> &'static HealthRegistry {
    REGISTRY.get_or_init(HealthRegistry::new)
}

/// Convenience: register a loop source with a staleness window.
pub fn register(name: &str, stale_after_secs: u64) {
    registry().register(name, stale_after_secs);
}

/// Convenience: record a heartbeat for `name`.
pub fn heartbeat(name: &str) {
    registry().heartbeat(name);
}

/// Convenience: set a source's status.
pub fn report(name: &str, status: HealthStatus) {
    registry().report(name, status);
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Convenience alias for callers that want a typed staleness window.
pub const fn secs(d: Duration) -> u64 {
    d.as_secs()
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> HealthRegistry {
        HealthRegistry::new()
    }

    #[test]
    fn empty_registry_is_healthy() {
        assert_eq!(fresh().verdict(), HealthStatus::Healthy);
    }

    #[test]
    fn worst_status_wins_the_verdict() {
        let r = fresh();
        r.report("a", HealthStatus::Healthy);
        r.report("b", HealthStatus::Degraded("slow".into()));
        match r.verdict() {
            HealthStatus::Degraded(_) => {}
            other => panic!("expected Degraded, got {other:?}"),
        }
        r.report("c", HealthStatus::Failed("broken".into()));
        match r.verdict() {
            HealthStatus::Failed(_) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn stale_heartbeat_overrides_healthy_status() {
        let report = HealthReport {
            name:             "loop".into(),
            status:           HealthStatus::Healthy,
            last_heartbeat:   1_000,
            stale_after_secs: 30,
        };
        // 20s after last heartbeat — within window, still healthy.
        assert_eq!(report.effective(1_020), HealthStatus::Healthy);
        // 40s after — stale, effectively Failed despite Healthy status.
        match report.effective(1_040) {
            HealthStatus::Failed(_) => {}
            other => panic!("expected Failed from staleness, got {other:?}"),
        }
    }

    #[test]
    fn zero_stale_after_disables_liveness_check() {
        let report = HealthReport {
            name:             "oneshot".into(),
            status:           HealthStatus::Healthy,
            last_heartbeat:   1_000,
            stale_after_secs: 0,
        };
        // Even a year later, a 0-window source is never "stale".
        assert!(!report.is_stale(1_000 + 31_536_000));
        assert_eq!(report.effective(1_000 + 31_536_000), HealthStatus::Healthy);
    }

    #[test]
    fn heartbeat_refreshes_liveness() {
        let r = fresh();
        r.register("loop", 30);
        r.heartbeat("loop");
        let snap = r.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "loop");
        // Fresh heartbeat → not stale right now.
        assert!(!snap[0].is_stale(now_secs()));
    }

    #[test]
    fn deregister_removes_source() {
        let r = fresh();
        r.report("temp", HealthStatus::Degraded("x".into()));
        assert_eq!(r.snapshot().len(), 1);
        r.deregister("temp");
        assert_eq!(r.snapshot().len(), 0);
        assert_eq!(r.verdict(), HealthStatus::Healthy);
    }

    #[test]
    fn snapshot_is_sorted_by_name() {
        let r = fresh();
        r.report("zebra", HealthStatus::Healthy);
        r.report("alpha", HealthStatus::Healthy);
        r.report("mike",  HealthStatus::Healthy);
        let names: Vec<String> = r.snapshot().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["alpha", "mike", "zebra"]);
    }
}
