//! End-to-end tests for time-based shard retention (Phase 1).
//!
//! Brings up a real `ShardsManager` against a tempdir, seeds the
//! catalog + on-disk shard directories + JsonCache, then drives
//! `retention::evict_expired` with a synthetic `now` so the cutoff is
//! deterministic.
//!
//! Each test binary gets its own process → `init_db`'s OnceLock isn't
//! shared with other integration tests.  We seed five shards spanning
//! a 5h synthetic time range:
//!
//!     [1h_window) [2h) [3h) [4h) [5h)
//!      0..3600    3600..7200    ... ... ...
//!
//! and walk the cutoff forward across the boundaries.

use bdslib::retention::{evict_expired, RetentionConfig};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const HOUR: u64 = 3600;

// ─────────────────────────────────────────────────────────────────────
// Setup — runs ONCE per test binary
// ─────────────────────────────────────────────────────────────────────

fn ensure_init() -> &'static PathBuf {
    use std::sync::OnceLock;
    static DB_ROOT: OnceLock<PathBuf> = OnceLock::new();
    DB_ROOT.get_or_init(|| {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("db");
        std::fs::create_dir_all(&db_path).expect("mkdir db");
        let cfg_path = tmp.path().join("bds.hjson");
        std::fs::write(&cfg_path, format!(
            "{{\n  dbpath: \"{}\"\n  shard_duration: \"1h\"\n  pool_size: 2\n}}\n",
            db_path.display(),
        )).expect("write cfg");
        bdslib::init_db(Some(cfg_path.to_str().unwrap())).expect("init_db");
        std::mem::forget(tmp);  // keep the dir alive for the binary's lifetime
        db_path
    })
}

/// Serialise every test on a global lock — they all mutate the shared
/// catalog, so they have to run one at a time inside the binary.
fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::OnceLock;
    static M: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

/// Erase every shard (catalog + dirs) so each test starts from a
/// clean baseline.
fn wipe_shards(db_root: &PathBuf) {
    let db = bdslib::get_db().expect("db");
    let live = db.cache().info().list_all().unwrap();
    for s in live {
        // Try the helper path first, then a brute-force fs cleanup so
        // the next ensure_init sees an empty world.
        let _ = db.evict_shard(s.shard_id);
        let _ = std::fs::remove_dir_all(&s.path);
    }
    // Belt-and-suspenders: any stray `*.evicting` dirs from a prior
    // crashed test.
    if let Ok(entries) = std::fs::read_dir(db_root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.file_name().and_then(|n| n.to_str())
                 .map(|s| s.ends_with(".evicting")).unwrap_or(false)
            {
                let _ = std::fs::remove_dir_all(&p);
            }
        }
    }
}

/// Seed N synthetic shards into the catalog AND create their on-disk
/// directories so the eviction path has something real to delete.
/// Returns the list of (shard_id, path, start_ts, end_ts) tuples in
/// insertion order.
fn seed_shards(n: usize, base_ts: u64) -> Vec<(Uuid, String, u64, u64)> {
    let db = bdslib::get_db().expect("db");
    let info = db.cache().info();
    let root = bdslib::dbpath_from_config(None).ok()
        .unwrap_or_else(|| db_root().to_string_lossy().into_owned());

    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let start = base_ts + (i as u64) * HOUR;
        let end   = start + HOUR;
        let path  = format!("{root}/{start}_{end}");
        // Make a sentinel file inside so freed_bytes is non-zero.
        std::fs::create_dir_all(&path).expect("mkdir shard");
        std::fs::write(format!("{path}/sentinel"), vec![0u8; 1024])
            .expect("write sentinel");
        let id = info.add_shard(
            &path,
            UNIX_EPOCH + Duration::from_secs(start),
            UNIX_EPOCH + Duration::from_secs(end),
        ).expect("add_shard");
        out.push((id, path, start, end));
    }
    out
}

fn db_root() -> &'static PathBuf {
    ensure_init()
}

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[test]
fn evict_expired_with_disabled_config_is_a_noop() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());
    let _seeded = seed_shards(3, 1_000_000);

    let cfg = RetentionConfig { enabled: false, ..Default::default() };
    let r = evict_expired(&cfg, at(2_000_000)).unwrap();
    assert!(r.disabled);
    assert_eq!(r.evicted, 0);

    // Catalog must be untouched.
    assert_eq!(bdslib::get_db().unwrap().cache().info().list_all().unwrap().len(), 3);
}

#[test]
fn evict_expired_removes_old_shards_from_catalog_and_disk() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());

    // Five shards spanning [1_000_000, 1_018_000) — each 1h wide.
    let seeded = seed_shards(5, 1_000_000);

    // Cutoff = now - 1h.  now = 1_000_000 + 4 * 3600 + 1800 (mid-way
    // through the 5th shard).  Eligible: shards with end_ts < cutoff.
    //   now = 1_015_000
    //   cutoff = 1_015_000 - 3600 = 1_011_400
    //   shard 0: end = 1_003_600  → 1_003_600 < 1_011_400  ✓
    //   shard 1: end = 1_007_200  → 1_007_200 < 1_011_400  ✓
    //   shard 2: end = 1_010_800  → 1_010_800 < 1_011_400  ✓
    //   shard 3: end = 1_014_400  → 1_014_400 ≥ 1_011_400  ✗
    //   shard 4: end = 1_018_000  → 1_018_000 ≥ 1_011_400  ✗
    let now = at(1_015_000);
    let cfg = RetentionConfig {
        enabled:  true,
        duration: Duration::from_secs(HOUR),
        max_evictions_per_run: 0,
        dry_run:  false,
        ..Default::default()
    };
    let r = evict_expired(&cfg, now).unwrap();
    assert!(!r.disabled);
    assert_eq!(r.evicted, 3, "expected shards 0,1,2 to be evicted");
    assert_eq!(r.errors, 0);
    assert!(r.freed_bytes > 0, "freed_bytes should be > 0 with on-disk sentinels");
    assert_eq!(r.min_start_ts, seeded[0].2);
    assert_eq!(r.max_end_ts,   seeded[2].3);

    // Catalog now holds shards 3 + 4 only.
    let surviving = bdslib::get_db().unwrap().cache().info().list_all().unwrap();
    assert_eq!(surviving.len(), 2);
    let surviving_ids: std::collections::HashSet<Uuid> = surviving.iter().map(|s| s.shard_id).collect();
    assert!(!surviving_ids.contains(&seeded[0].0));
    assert!(!surviving_ids.contains(&seeded[1].0));
    assert!(!surviving_ids.contains(&seeded[2].0));
    assert!( surviving_ids.contains(&seeded[3].0));
    assert!( surviving_ids.contains(&seeded[4].0));

    // On-disk dirs for shards 0..3 must be gone.
    for s in seeded.iter().take(3) {
        assert!(!std::path::Path::new(&s.1).exists(),
            "evicted shard dir still on disk: {}", s.1);
    }
    // Shards 3 + 4 still have their dirs.
    assert!(std::path::Path::new(&seeded[3].1).exists());
    assert!(std::path::Path::new(&seeded[4].1).exists());
}

#[test]
fn evict_expired_dry_run_does_not_touch_catalog_or_disk() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());
    let seeded = seed_shards(3, 2_000_000);

    let cfg = RetentionConfig {
        enabled:  true,
        duration: Duration::from_secs(HOUR),
        max_evictions_per_run: 0,
        dry_run:  true,
        ..Default::default()
    };
    // now = far enough in the future that every shard would be evictable.
    let r = evict_expired(&cfg, at(2_000_000 + 10 * HOUR)).unwrap();
    assert!(r.dry_run);
    assert_eq!(r.evicted, 3);    // count reflects WOULD-evict
    assert_eq!(r.freed_bytes, 0); // dry-run never accounts bytes

    // Catalog untouched.
    assert_eq!(bdslib::get_db().unwrap().cache().info().list_all().unwrap().len(), 3);

    // Dirs untouched.
    for s in &seeded {
        assert!(std::path::Path::new(&s.1).exists(),
            "dry-run deleted shard dir: {}", s.1);
    }
}

#[test]
fn evict_expired_respects_max_evictions_per_run() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());
    let seeded = seed_shards(5, 3_000_000);

    let cfg = RetentionConfig {
        enabled:  true,
        duration: Duration::from_secs(HOUR),
        max_evictions_per_run: 2,
        dry_run:  false,
        ..Default::default()
    };
    // now far enough that all 5 would normally be eligible.
    let r = evict_expired(&cfg, at(3_000_000 + 10 * HOUR)).unwrap();
    assert_eq!(r.evicted, 2, "max_evictions_per_run must cap the sweep");

    // Catalog now holds 3 (5 - 2).  The 2 oldest are gone.
    let surviving = bdslib::get_db().unwrap().cache().info().list_all().unwrap();
    assert_eq!(surviving.len(), 3);
    let surviving_ids: std::collections::HashSet<Uuid> = surviving.iter().map(|s| s.shard_id).collect();
    assert!(!surviving_ids.contains(&seeded[0].0));
    assert!(!surviving_ids.contains(&seeded[1].0));
    assert!( surviving_ids.contains(&seeded[2].0));
    assert!( surviving_ids.contains(&seeded[3].0));
    assert!( surviving_ids.contains(&seeded[4].0));
}

#[test]
fn evict_expired_invalidates_jsoncache_entries_in_evicted_window() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());

    let base = 4_000_000;
    let _seeded = seed_shards(3, base);

    // Prime the JsonCache with one entry per shard's timestamp range
    // plus one entry that's clearly outside any evicted window.
    let cache = bdslib::get_db().unwrap().jsoncache();
    cache.insert("rec-in-evict-1", base + 100,        json!({"k": "v1"}));  // shard 0
    cache.insert("rec-in-evict-2", base + HOUR + 100, json!({"k": "v2"}));  // shard 1
    cache.insert("rec-survivor",   base + 3 * HOUR,   json!({"k": "v3"}));  // beyond
    assert_eq!(cache.len(), 3);

    // Evict the first 2 shards (cutoff just past shard 1's end_ts).
    let cfg = RetentionConfig {
        enabled:  true,
        duration: Duration::from_secs(HOUR),
        max_evictions_per_run: 0,
        dry_run:  false,
        ..Default::default()
    };
    let r = evict_expired(&cfg, at(base + 3 * HOUR + 1)).unwrap();
    assert_eq!(r.evicted, 2);

    // JsonCache: the two in-range entries are gone, the survivor remains.
    assert!(cache.get("rec-in-evict-1", base + 100).is_none());
    assert!(cache.get("rec-in-evict-2", base + HOUR + 100).is_none());
    assert!(cache.get("rec-survivor",   base + 3 * HOUR).is_some());
}

#[test]
fn cleanup_orphan_evicting_finishes_crashed_sweeps() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());

    // Seed a shard, manually mark it evicting, rename its directory to
    // .evicting (i.e. simulate a crash after step 3 of evict_shard).
    let seeded = seed_shards(2, 5_000_000);
    let (id_orphan, path_orphan, _, _) = seeded[0].clone();
    let evict_path = format!("{path_orphan}.evicting");
    bdslib::get_db().unwrap().cache().info().mark_evicting(id_orphan).unwrap();
    std::fs::rename(&path_orphan, &evict_path).unwrap();

    // Pre-condition: catalog has 2 rows (1 evicting + 1 normal),
    // on-disk has 1 shard + 1 .evicting dir.
    assert_eq!(bdslib::get_db().unwrap().cache().info().list_all().unwrap().len(), 2);
    assert!(std::path::Path::new(&evict_path).exists());

    // Recovery.
    let n = bdslib::get_db().unwrap().cleanup_orphan_evicting().unwrap();
    assert_eq!(n, 1, "expected exactly 1 orphan cleaned up");

    // Post-condition: catalog has 1 row (the non-orphan), .evicting dir is gone.
    assert_eq!(bdslib::get_db().unwrap().cache().info().list_all().unwrap().len(), 1);
    assert!(!std::path::Path::new(&evict_path).exists());
    assert!(!std::path::Path::new(&path_orphan).exists());
}

#[test]
fn evict_expired_records_run_stats() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());
    let _seeded = seed_shards(2, 6_000_000);

    // Snapshot the lifetime counter so we can prove it grew.
    use std::sync::atomic::Ordering;
    let before = bdslib::retention::stats().evicted_lifetime.load(Ordering::Relaxed);

    let cfg = RetentionConfig {
        enabled:  true,
        duration: Duration::from_secs(HOUR),
        max_evictions_per_run: 0,
        dry_run:  false,
        ..Default::default()
    };
    let r = evict_expired(&cfg, at(6_000_000 + 10 * HOUR)).unwrap();
    bdslib::retention::record_run(&r);

    let s = bdslib::retention::stats();
    assert_eq!(s.evicted_last_run.load(Ordering::Relaxed), 2);
    assert!(s.evicted_lifetime.load(Ordering::Relaxed) >= before + 2);
    assert!(s.last_run_ts.load(Ordering::Relaxed) > 0);
}

// ─────────────────────────────────────────────────────────────────────
// Phase 3 — cluster-aware quorum gating
// ─────────────────────────────────────────────────────────────────────

use bdslib::retention::evict_expired_with_quorum;

#[test]
fn quorum_check_skips_evictions_when_closure_returns_false() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());
    let seeded = seed_shards(3, 7_000_000);

    let cfg = RetentionConfig {
        enabled:              true,
        duration:             Duration::from_secs(HOUR),
        max_evictions_per_run: 0,
        dry_run:               false,
        quorum_check_enabled: true,
        quorum_min_peers:     1,
    };
    // Closure always says "no quorum" — every candidate must be skipped.
    let report = evict_expired_with_quorum(
        &cfg,
        at(7_000_000 + 10 * HOUR),
        |_, _| false,
    ).unwrap();

    assert_eq!(report.evicted, 0, "no shards must be evicted when quorum closure returns false");
    assert_eq!(report.quorum_skipped, 3, "all 3 candidates must register as quorum_skipped");

    // Catalog + dirs untouched.
    assert_eq!(bdslib::get_db().unwrap().cache().info().list_all().unwrap().len(), 3);
    for s in &seeded {
        assert!(std::path::Path::new(&s.1).exists(),
            "quorum-skipped shard dir was deleted: {}", s.1);
    }
}

#[test]
fn quorum_check_evicts_when_closure_returns_true() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());
    let _seeded = seed_shards(3, 8_000_000);

    let cfg = RetentionConfig {
        enabled:              true,
        duration:             Duration::from_secs(HOUR),
        max_evictions_per_run: 0,
        dry_run:               false,
        quorum_check_enabled: true,
        quorum_min_peers:     1,
    };
    // Closure always allows.  Quorum is enabled but never blocks.
    let report = evict_expired_with_quorum(
        &cfg,
        at(8_000_000 + 10 * HOUR),
        |_, _| true,
    ).unwrap();

    assert_eq!(report.evicted, 3);
    assert_eq!(report.quorum_skipped, 0);
    assert_eq!(bdslib::get_db().unwrap().cache().info().list_all().unwrap().len(), 0);
}

#[test]
fn quorum_check_inspects_per_shard_intervals() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());
    let seeded = seed_shards(3, 9_000_000);

    // Allow only the middle shard's interval.
    let middle_start = seeded[1].2 as i64;
    let middle_end   = seeded[1].3 as i64;

    let cfg = RetentionConfig {
        enabled:              true,
        duration:             Duration::from_secs(HOUR),
        max_evictions_per_run: 0,
        dry_run:               false,
        quorum_check_enabled: true,
        quorum_min_peers:     1,
    };
    let report = evict_expired_with_quorum(
        &cfg,
        at(9_000_000 + 10 * HOUR),
        move |start, end| start == middle_start && end == middle_end,
    ).unwrap();

    assert_eq!(report.evicted, 1, "only the middle shard's quorum check passes");
    assert_eq!(report.quorum_skipped, 2);

    let surviving = bdslib::get_db().unwrap().cache().info().list_all().unwrap();
    assert_eq!(surviving.len(), 2);
    let surviving_ids: std::collections::HashSet<Uuid> = surviving.iter().map(|s| s.shard_id).collect();
    assert!( surviving_ids.contains(&seeded[0].0), "shard 0 must survive (quorum denied)");
    assert!(!surviving_ids.contains(&seeded[1].0), "shard 1 must be evicted (quorum allowed)");
    assert!( surviving_ids.contains(&seeded[2].0), "shard 2 must survive (quorum denied)");
}

#[test]
fn quorum_disabled_ignores_closure_completely() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());
    let _seeded = seed_shards(2, 10_000_000);

    // quorum_check_enabled=false: the closure is never called even
    // if it would refuse every interval.
    let cfg = RetentionConfig {
        enabled:              true,
        duration:             Duration::from_secs(HOUR),
        max_evictions_per_run: 0,
        dry_run:               false,
        quorum_check_enabled: false,   // ← OFF
        quorum_min_peers:     1,
    };
    let report = evict_expired_with_quorum(
        &cfg,
        at(10_000_000 + 10 * HOUR),
        |_, _| panic!("quorum closure must not be invoked when quorum_check_enabled=false"),
    ).unwrap();

    assert_eq!(report.evicted, 2);
    assert_eq!(report.quorum_skipped, 0);
}

#[test]
fn quorum_skipped_rolls_into_lifetime_stats() {
    let _ = ensure_init();
    let _g = test_lock();
    wipe_shards(db_root());
    let _seeded = seed_shards(2, 11_000_000);

    use std::sync::atomic::Ordering;
    let before = bdslib::retention::stats().quorum_skipped_lifetime.load(Ordering::Relaxed);

    let cfg = RetentionConfig {
        enabled:              true,
        duration:             Duration::from_secs(HOUR),
        max_evictions_per_run: 0,
        dry_run:               false,
        quorum_check_enabled: true,
        quorum_min_peers:     1,
    };
    let report = evict_expired_with_quorum(
        &cfg,
        at(11_000_000 + 10 * HOUR),
        |_, _| false,    // deny all
    ).unwrap();
    bdslib::retention::record_run(&report);

    let s = bdslib::retention::stats();
    assert!(s.quorum_skipped_lifetime.load(Ordering::Relaxed) >= before + 2);
    assert_eq!(s.quorum_skipped_last_run.load(Ordering::Relaxed), 2);
}
