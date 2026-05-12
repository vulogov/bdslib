//! Realistic synthetic dataset generator for emulating RCA, anomaly
//! detection, denoising, k-NN clustering, template mining, trends,
//! and aggregated search.
//!
//! Unlike [`crate::common::generator::Generator`] which produces a
//! flat stream of independently-random records, this module emits
//! a **structured** corpus built from three layers:
//!
//! 1. **Background noise** — high-cadence routine traffic
//!    (heartbeat metrics, health-check HTTP probes, cron ticks,
//!    audit log lines) that should be filterable as boilerplate by
//!    the denoiser and identifiable as routine clusters by k-NN.
//!
//! 2. **Incident scenarios** — small numbers of structured cascades
//!    where a precursor (e.g. `db.connections` saturation) fires
//!    first, downstream metrics deteriorate, log templates emerge,
//!    and finally a failure event lands.  Each scenario uses a
//!    consistent set of host / region / env tags so RCA can group
//!    them as a cluster, and the precursor → failure lead times
//!    are deterministic so `v?/rca` reports clear positive leads.
//!
//! 3. **Anomalies** — rare records with unusual vocabulary
//!    (kernel-level errors, ECC corrections, novel exception
//!    types) sprinkled at 1–2 % density so the n-gram anomaly
//!    detector has surprises to surface.
//!
//! All three layers share the same on-disk shape as the existing
//! `Generator` output: `{timestamp, key, data: {…}}`.
//!
//! ## Determinism
//!
//! Pass `seed: Some(n)` in [`RealisticConfig`] to lock the RNG.
//! Same seed + same config → byte-identical output (modulo
//! timestamp drift across `now`, which the caller can pin via the
//! generator's `with_time_range`).
//!
//! ## Tuning
//!
//! - `scenarios: 3` for a 6 h window gives ~150 cascade records on
//!   top of background.  Raise for richer RCA signal; lower if the
//!   noise floor should dominate.
//! - `noise_ratio: 0.7` means ~70 % of the corpus is background.
//!   The denoiser's default `noise_threshold` of `0.85` should
//!   keep ~all background as REMOVED and most scenario records as
//!   KEPT.
//! - `anomaly_ratio: 0.02` — 2 % rare records.  Adjust to test
//!   detector sensitivity.

use crate::common::time::now_secs;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as JsonValue};

/// Configuration for [`generate`].  All fields are required but the
/// `Default` impl gives sensible values for a typical 6 h sample.
#[derive(Debug, Clone)]
pub struct RealisticConfig {
    /// Humantime duration string for the data window (e.g. `"6h"`,
    /// `"24h"`).  The window ends at `now`.
    pub duration: String,
    /// Total target number of records (background + scenarios +
    /// anomalies).  The split is governed by `noise_ratio` and
    /// `anomaly_ratio`.  Default 2000.
    pub total: usize,
    /// Number of incident cascades to embed.  Each emits 30–60
    /// records spanning a few minutes of the timeline.  Default 3.
    pub scenarios: usize,
    /// Fraction of `total` reserved for background noise.  The
    /// remainder is split between scenarios and anomalies.  Default
    /// 0.7.  Clamped to `[0.0, 0.95]`.
    pub noise_ratio: f64,
    /// Fraction of `total` reserved for anomaly records.  Default
    /// 0.02.  Clamped to `[0.0, 0.10]`.
    pub anomaly_ratio: f64,
    /// Optional RNG seed for reproducibility.  When `None`, uses
    /// `thread_rng`-style entropy.
    pub seed: Option<u64>,
}

impl Default for RealisticConfig {
    fn default() -> Self {
        Self {
            duration:      "6h".to_owned(),
            total:         2000,
            scenarios:     3,
            noise_ratio:   0.7,
            anomaly_ratio: 0.02,
            seed:          None,
        }
    }
}

/// Top-level entry point.  Returns a corpus of `cfg.total` (approx.)
/// records: background noise + incident scenarios + anomalies,
/// sorted by timestamp.
pub fn generate(cfg: &RealisticConfig) -> Vec<JsonValue> {
    let duration_secs = humantime::parse_duration(&cfg.duration)
        .map(|d| d.as_secs())
        .unwrap_or(6 * 3600);
    let now = now_secs();
    let window_start = now.saturating_sub(duration_secs);
    let window_end   = now;

    let mut rng: StdRng = match cfg.seed {
        Some(s) => StdRng::seed_from_u64(s),
        None    => StdRng::from_entropy(),
    };

    let noise_ratio   = cfg.noise_ratio.clamp(0.0, 0.95);
    let anomaly_ratio = cfg.anomaly_ratio.clamp(0.0, 0.10);

    let target_noise    = ((cfg.total as f64) * noise_ratio).round() as usize;
    let target_anomaly  = ((cfg.total as f64) * anomaly_ratio).round() as usize;

    let mut out: Vec<JsonValue> = Vec::with_capacity(cfg.total + 256);

    // 1. Background noise — emit first because it forms the corpus
    //    baseline the analyses operate against.
    out.extend(background_noise(&mut rng, window_start, window_end, target_noise));

    // 2. Incident scenarios — place each at a quasi-random offset
    //    inside the central 80 % of the window so its precursors
    //    fit and its consequences land before `now`.
    for i in 0..cfg.scenarios {
        let offset_frac = 0.10 + ((i as f64 + 0.5) / cfg.scenarios as f64) * 0.80
            + rng.gen_range(-0.05f64..0.05);
        let offset = (offset_frac.clamp(0.05, 0.95) * duration_secs as f64) as u64;
        let failure_ts = window_start + offset;
        out.extend(emit_scenario(&mut rng, failure_ts));
    }

    // 3. Anomalies — uniform spread, but only as many as needed to
    //    hit the target ratio (scenarios already contribute some
    //    rare-vocabulary records on their own).
    out.extend(anomalies(&mut rng, window_start, window_end, target_anomaly));

    // Sort by timestamp so downstream analyses see a coherent
    // time-ordered stream rather than three concatenated batches.
    out.sort_by_key(|r| r.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0));
    out
}

// ── Background noise ─────────────────────────────────────────────────────────

fn background_noise<R: Rng>(rng: &mut R, t0: u64, t1: u64, n: usize) -> Vec<JsonValue> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let ts = rand_ts(rng, t0, t1);
        let r = rng.gen_range(0u8..10);
        out.push(match r {
            0..=3 => routine_heartbeat_metric(rng, ts),
            4..=6 => routine_health_probe(rng, ts),
            7..=8 => routine_cron_tick(rng, ts),
            _     => routine_audit_log(rng, ts),
        });
    }
    out
}

/// A flat-ish CPU/mem/net heartbeat metric.  Values stay in a
/// "boring" band so the denoiser bucks them out as noise and the
/// trend analyzer sees flat lines.
fn routine_heartbeat_metric<R: Rng>(rng: &mut R, ts: u64) -> JsonValue {
    const KEYS: &[(&str, &str, f64, f64)] = &[
        ("cpu.usage",            "percent", 25.0, 55.0),
        ("mem.used_pct",         "percent", 35.0, 65.0),
        ("disk.iowait",          "percent",  0.5,  6.0),
        ("net.rx_bytes_sec",     "bytes/s",  5e5,  3e7),
        ("net.tx_bytes_sec",     "bytes/s",  5e5,  3e7),
        ("cache.hit_rate",       "percent", 88.0, 99.0),
        ("http.request_rate",    "req/s",   80.0, 250.0),
        ("queue.depth",          "count",    0.0,  10.0),
    ];
    let (key, unit, lo, hi) = KEYS[rng.gen_range(0..KEYS.len())];
    let value = round2(rng.gen_range(lo..=hi));
    json!({
        "timestamp": ts,
        "key": key,
        "data": {
            "value": value, "unit": unit,
            "host":   pick_host(rng),
            "region": "us-east-1",
            "env":    "prod",
        }
    })
}

fn routine_health_probe<R: Rng>(rng: &mut R, ts: u64) -> JsonValue {
    let host = pick_host(rng);
    let path = ["/health", "/healthz", "/ready", "/metrics"][rng.gen_range(0..4)];
    let raw = format!(
        r#"10.0.0.{} - - [-] "GET {} HTTP/1.1" 200 64 "-" "kube-probe/1.27""#,
        rng.gen_range(2..200), path
    );
    json!({
        "timestamp": ts,
        "key": format!("GET {path}"),
        "data": {
            "method": "GET", "path": path, "status": "200",
            "bytes": "64", "client": format!("10.0.0.{}", rng.gen_range(2..200)),
            "user_agent": "kube-probe/1.27",
            "host": host, "raw": raw,
        }
    })
}

fn routine_cron_tick<R: Rng>(rng: &mut R, ts: u64) -> JsonValue {
    let job = ["log_rotate", "metrics_flush", "vacuum_analyze",
               "session_gc", "snapshot_backup"][rng.gen_range(0..5)];
    let host = pick_host(rng);
    let raw = format!("cron[{}]: ({}) CMD (/usr/local/bin/{} --quiet)", rng.gen_range(1000..9999), host, job);
    json!({
        "timestamp": ts,
        "key": "cron",
        "data": {
            "message": format!("({host}) CMD (/usr/local/bin/{job} --quiet)"),
            "host": host, "pid": rng.gen_range(1000..9999).to_string(),
            "raw": raw,
        }
    })
}

fn routine_audit_log<R: Rng>(rng: &mut R, ts: u64) -> JsonValue {
    let user = ["alice", "bob", "ops-deploy", "ci-pipeline"][rng.gen_range(0..4)];
    let action = ["session opened", "session closed", "authentication ok",
                  "token refreshed"][rng.gen_range(0..4)];
    let host = pick_host(rng);
    let raw = format!("sshd[{}]: {} for user {} from 10.0.0.{}", rng.gen_range(500..9000), action, user, rng.gen_range(2..200));
    json!({
        "timestamp": ts,
        "key": "sshd",
        "data": {
            "message": format!("{action} for user {user}"),
            "host": host, "pid": rng.gen_range(500..9000).to_string(),
            "raw": raw,
        }
    })
}

// ── Anomalies (rare records) ─────────────────────────────────────────────────

fn anomalies<R: Rng>(rng: &mut R, t0: u64, t1: u64, n: usize) -> Vec<JsonValue> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let ts = rand_ts(rng, t0, t1);
        out.push(match rng.gen_range(0u8..5) {
            0 => json!({
                "timestamp": ts, "key": "kernel",
                "data": {
                    "message": "NMI watchdog: BUG: soft lockup - CPU#3 stuck for 23s! [kworker/3:0:42]",
                    "host": pick_host(rng), "pid": "0",
                    "raw": "kernel: NMI watchdog: BUG: soft lockup - CPU#3 stuck for 23s! [kworker/3:0:42]",
                }
            }),
            1 => json!({
                "timestamp": ts, "key": "kernel",
                "data": {
                    "message": format!("ECC memory: corrected single-bit error at address 0x{:x}", rng.gen_range(0x100000u64..0xFFFFFFFFu64)),
                    "host": pick_host(rng), "pid": "0",
                    "raw": format!("kernel: EDAC MC0: 1 CE memory read error on CPU_SrcID#0_Ha#0_Chan#1_DIMM#0 (channel:1 slot:0 page:0x{:x})", rng.gen_range(0x10000u64..0xFFFFFFu64)),
                }
            }),
            2 => json!({
                "timestamp": ts, "key": "process.mem_rss",
                "data": {
                    "value": round2(rng.gen_range(4.8e10..6.5e10)),
                    "unit": "bytes",
                    "host": pick_host(rng), "region": "us-east-1", "env": "prod",
                    "process": "java-pid-31337",
                }
            }),
            3 => json!({
                "timestamp": ts, "key": "ZeroDivisionError",
                "data": {
                    "exception_type": "ZeroDivisionError",
                    "exception_message": "float division by zero in rare_codepath()",
                    "frames": [{
                        "file": "core/processor.py",
                        "function": "dispatch",
                        "line": rng.gen_range(200..400),
                        "source": "ratio = x / window_size",
                    }],
                }
            }),
            _ => json!({
                "timestamp": ts, "key": "systemd",
                "data": {
                    "message": "watchdog timeout: service failed to ping in 60s, forcibly restarting",
                    "host": pick_host(rng), "pid": "1",
                    "raw": "systemd[1]: watchdog: forced restart of payment-api.service after 60s ping timeout",
                }
            }),
        });
    }
    out
}

// ── Incident scenarios ──────────────────────────────────────────────────────

fn emit_scenario<R: Rng>(rng: &mut R, failure_ts: u64) -> Vec<JsonValue> {
    match rng.gen_range(0u8..5) {
        0 => scenario_db_overload(rng, failure_ts),
        1 => scenario_oom_kill(rng, failure_ts),
        2 => scenario_cert_expiry(rng, failure_ts),
        3 => scenario_deployment_regression(rng, failure_ts),
        _ => scenario_network_partition(rng, failure_ts),
    }
}

/// Scenario 1 — Database overload → API latency → 5xx.
///
/// Precursor chain: connections saturate → query latency climbs →
/// cache hit-rate drops → HTTP p99 climbs → 5xx rate spikes →
/// api-gateway service crashes.  All events tagged with the same
/// `region=us-east-1, env=prod` so RCA clusters them together.
fn scenario_db_overload<R: Rng>(rng: &mut R, t0: u64) -> Vec<JsonValue> {
    let host_db  = "db-primary";
    let host_api = ["api-01", "api-02"][rng.gen_range(0..2)];
    let host_cache = "cache-01";
    let mut out = Vec::new();

    // T-300s … T-240s: db.connections breach
    for _ in 0..rng.gen_range(6..11) {
        let ts = t0.saturating_sub(rng.gen_range(240..301));
        out.push(metric_record(ts, "db.connections", rng.gen_range(485.0..500.0), "count",
                               host_db, "us-east-1", "prod"));
    }
    // T-240s … T-180s: db.query_latency_ms spike
    for _ in 0..rng.gen_range(8..14) {
        let ts = t0.saturating_sub(rng.gen_range(180..241));
        out.push(metric_record(ts, "db.query_latency_ms", rng.gen_range(900.0..2400.0), "ms",
                               host_db, "us-east-1", "prod"));
    }
    // T-150s … T-90s: cache.hit_rate drops
    for _ in 0..rng.gen_range(5..9) {
        let ts = t0.saturating_sub(rng.gen_range(90..151));
        out.push(metric_record(ts, "cache.hit_rate", rng.gen_range(15.0..32.0), "percent",
                               host_cache, "us-east-1", "prod"));
    }
    // T-120s … T-30s: http.p99_latency_ms climbs
    for _ in 0..rng.gen_range(6..12) {
        let ts = t0.saturating_sub(rng.gen_range(30..121));
        out.push(metric_record(ts, "http.p99_latency_ms", rng.gen_range(2200.0..6500.0), "ms",
                               host_api, "us-east-1", "prod"));
    }
    // T-60s … T-15s: log lines about upstream timeouts
    for _ in 0..rng.gen_range(8..14) {
        let ts = t0.saturating_sub(rng.gen_range(15..61));
        out.push(log_record(ts, "nginx", "us-east-1", host_api,
            "upstream timed out (110: Connection timed out) while connecting to upstream, client: 10.0.5.42, server: api.prod.example.com, request: \"POST /api/v1/checkout HTTP/1.1\", upstream: \"http://10.0.0.51:8080/api/v1/checkout\"",
        ));
    }
    // T-30s … T-5s: http.error_rate
    for _ in 0..rng.gen_range(4..8) {
        let ts = t0.saturating_sub(rng.gen_range(5..31));
        out.push(metric_record(ts, "http.error_rate", rng.gen_range(15.0..28.0), "percent",
                               host_api, "us-east-1", "prod"));
    }
    // T0: the failure event
    out.push(log_record(t0, "systemd", "us-east-1", host_api,
        "Failed with result 'core-dump': api-gateway.service main process (12345) crashed with SIGSEGV (signal 11), code=killed",
    ));
    out.push(metric_record(t0, "service.crashed", 1.0, "count",
                           host_api, "us-east-1", "prod"));
    // T+30s … T+90s: blast-radius consequences (alerts firing, restart attempts)
    for _ in 0..rng.gen_range(3..6) {
        let ts = t0 + rng.gen_range(30..91);
        out.push(log_record(ts, "systemd", "us-east-1", host_api,
            "api-gateway.service: Scheduled restart job, restart counter is at 1",
        ));
    }
    out
}

/// Scenario 2 — Memory pressure → OOM kill.
fn scenario_oom_kill<R: Rng>(rng: &mut R, t0: u64) -> Vec<JsonValue> {
    let host = ["worker-01", "worker-02", "k8s-node-01"][rng.gen_range(0..3)];
    let pid = rng.gen_range(20000..40000);
    let mut out = Vec::new();

    // mem.used_pct rising over 10 minutes
    let stages = [(600, 72.0, 80.0), (450, 81.0, 88.0), (300, 89.0, 93.0), (180, 93.0, 97.0), (90, 97.0, 99.5)];
    for (offset, lo, hi) in stages.iter() {
        for _ in 0..rng.gen_range(3..7) {
            let ts = t0.saturating_sub(offset + rng.gen_range(0..60));
            out.push(metric_record(ts, "mem.used_pct", rng.gen_range(*lo..=*hi), "percent",
                                   host, "us-west-2", "prod"));
        }
    }
    // swap usage starts climbing
    for _ in 0..rng.gen_range(5..9) {
        let ts = t0.saturating_sub(rng.gen_range(60..301));
        out.push(metric_record(ts, "mem.swap_used_bytes", rng.gen_range(8e8..2.1e9), "bytes",
                               host, "us-west-2", "prod"));
    }
    // log line: OOM killer invoked
    out.push(log_record(t0.saturating_sub(15), "kernel", "us-west-2", host,
        &format!("Out of memory: Kill process {pid} (java) score 891 or sacrifice child"),
    ));
    out.push(log_record(t0.saturating_sub(8), "kernel", "us-west-2", host,
        &format!("oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),cpuset=/,mems_allowed=0,global_oom,task_memcg=/system.slice/payment-worker.service,task=java,pid={pid},uid=997"),
    ));
    // T0: systemd reports the kill
    out.push(log_record(t0, "systemd", "us-west-2", host,
        &format!("payment-worker.service: Main process exited, code=killed, status=9/KILL"),
    ));
    out.push(metric_record(t0, "service.crashed", 1.0, "count", host, "us-west-2", "prod"));
    // Consequence: queue.lag_ms surges as the dead worker stops draining
    for _ in 0..rng.gen_range(4..8) {
        let ts = t0 + rng.gen_range(15..120);
        out.push(metric_record(ts, "queue.lag_ms", rng.gen_range(15000.0..29000.0), "ms",
                               host, "us-west-2", "prod"));
    }
    out
}

/// Scenario 3 — TLS certificate expiry.
fn scenario_cert_expiry<R: Rng>(rng: &mut R, t0: u64) -> Vec<JsonValue> {
    let host_lb = "web-01";
    let domain = ["api.prod.example.com", "auth.prod.example.com", "payments.prod.example.com"][rng.gen_range(0..3)];
    let mut out = Vec::new();

    // T-3600s … T-1800s: warning logs about impending expiry
    for offset in [3600u64, 2400, 1800, 1200, 600] {
        let ts = t0.saturating_sub(offset + rng.gen_range(0..120));
        out.push(log_record(ts, "nginx", "us-east-1", host_lb,
            &format!("[warn] TLS certificate /etc/nginx/ssl/{domain}.crt expires in {:.1} hours", (offset as f64) / 3600.0),
        ));
    }
    // T-120s: tls_error_count starts to rise
    for _ in 0..rng.gen_range(6..10) {
        let ts = t0.saturating_sub(rng.gen_range(60..151));
        out.push(metric_record(ts, "http.tls_error_count", rng.gen_range(3.0..18.0), "count",
                               host_lb, "us-east-1", "prod"));
    }
    // T-60s … T-15s: client handshake failures
    for _ in 0..rng.gen_range(10..18) {
        let ts = t0.saturating_sub(rng.gen_range(15..61));
        out.push(log_record(ts, "nginx", "us-east-1", host_lb,
            &format!("client TLS handshake failed (SSL: error:14094412:SSL routines:ssl3_read_bytes:sslv3 alert bad certificate) while SSL handshaking, client: 10.{}.{}.{}, server: {}", rng.gen_range(0..255), rng.gen_range(0..255), rng.gen_range(2..200), domain),
        ));
    }
    // T-30s: http.error_rate spikes
    for _ in 0..rng.gen_range(4..7) {
        let ts = t0.saturating_sub(rng.gen_range(5..31));
        out.push(metric_record(ts, "http.error_rate", rng.gen_range(8.0..18.0), "percent",
                               host_lb, "us-east-1", "prod"));
    }
    // T0: the failure event
    out.push(log_record(t0, "nginx", "us-east-1", host_lb,
        &format!("TLS certificate /etc/nginx/ssl/{domain}.crt HAS EXPIRED — refusing new TLS connections until renewal"),
    ));
    out.push(metric_record(t0, "cert.expired", 1.0, "count", host_lb, "us-east-1", "prod"));
    out
}

/// Scenario 4 — Deployment regression.
fn scenario_deployment_regression<R: Rng>(rng: &mut R, t0: u64) -> Vec<JsonValue> {
    let service = ["payment-api", "auth-api", "order-api"][rng.gen_range(0..3)];
    let host = ["api-01", "api-02"][rng.gen_range(0..2)];
    let old_v  = format!("v1.{}.{}", rng.gen_range(2..5), rng.gen_range(0..9));
    let new_v  = format!("v1.{}.{}", rng.gen_range(5..8), rng.gen_range(0..9));
    let mut out = Vec::new();

    // T-1200s: deployment log line
    out.push(log_record(t0.saturating_sub(1200), "kubelet", "us-east-1", host,
        &format!("rolling update: {} {} -> {} on {}", service, old_v, new_v, host),
    ));
    out.push(log_record(t0.saturating_sub(1180), "kubelet", "us-east-1", host,
        &format!("pod {}-7d4c95b8f-{} created with image registry.internal/{}:{}", service, random_id(rng, 5), service, new_v),
    ));
    // T-1100s … T-300s: latency doubles
    for _ in 0..rng.gen_range(8..14) {
        let ts = t0.saturating_sub(rng.gen_range(300..1101));
        out.push(metric_record(ts, format!("{}.latency_ms", service.replace('-', "_")).as_str(),
                               rng.gen_range(800.0..2200.0), "ms",
                               host, "us-east-1", "prod"));
    }
    // T-600s: NullPointerException starts firing
    for _ in 0..rng.gen_range(6..12) {
        let ts = t0.saturating_sub(rng.gen_range(60..601));
        out.push(log_record(ts, &format!("{service}"), "us-east-1", host,
            &format!("ERROR: NullPointerException in OrderService.process(OrderService.java:{}) — cannot invoke \"com.example.Currency.code()\" because \"this.currency\" is null", rng.gen_range(120..240)),
        ));
    }
    // T-300s: error_rate spike on this service
    for _ in 0..rng.gen_range(4..7) {
        let ts = t0.saturating_sub(rng.gen_range(60..301));
        out.push(metric_record(ts, format!("{}.error_rate", service.replace('-', "_")).as_str(),
                               rng.gen_range(12.0..30.0), "percent",
                               host, "us-east-1", "prod"));
    }
    // T0: rollback decision
    out.push(log_record(t0, "kubelet", "us-east-1", host,
        &format!("rollout {}: aborted due to elevated error rate, rolling back {} -> {}", service, new_v, old_v),
    ));
    out.push(metric_record(t0, "deployment.rollback", 1.0, "count", host, "us-east-1", "prod"));
    out
}

/// Scenario 5 — Network partition between regions.
fn scenario_network_partition<R: Rng>(rng: &mut R, t0: u64) -> Vec<JsonValue> {
    let host_a = "api-01";
    let host_b = "db-replica-01";
    let mut out = Vec::new();

    // Net packet drops on the link climb
    for _ in 0..rng.gen_range(8..14) {
        let ts = t0.saturating_sub(rng.gen_range(60..301));
        out.push(metric_record(ts, "net.dropped_packets", rng.gen_range(180.0..900.0), "count",
                               host_a, "us-east-1", "prod"));
    }
    // RX bytes drop on the receiving side (the link is gone)
    for _ in 0..rng.gen_range(6..10) {
        let ts = t0.saturating_sub(rng.gen_range(30..181));
        out.push(metric_record(ts, "net.rx_bytes_sec", rng.gen_range(0.0..1.5e6), "bytes/s",
                               host_b, "eu-west-1", "prod"));
    }
    // Connection refused logs
    for _ in 0..rng.gen_range(10..16) {
        let ts = t0.saturating_sub(rng.gen_range(15..121));
        out.push(log_record(ts, "postgres", "eu-west-1", host_b,
            "could not connect to primary server: connection refused\n\tIs the server running on host \"db-primary.us-east-1.internal\" (10.0.0.10) and accepting TCP/IP connections on port 5432?",
        ));
    }
    // Replication lag explodes
    for _ in 0..rng.gen_range(5..8) {
        let ts = t0.saturating_sub(rng.gen_range(15..121));
        out.push(metric_record(ts, "replication.lag_secs", rng.gen_range(600.0..2400.0), "seconds",
                               host_b, "eu-west-1", "prod"));
    }
    // T0: partition declared
    out.push(log_record(t0, "patroni", "eu-west-1", host_b,
        "network partition detected: lost contact with primary db-primary.us-east-1.internal for 120s, promoting local replica to primary",
    ));
    out.push(metric_record(t0, "cluster.split_brain_event", 1.0, "count",
                           host_b, "eu-west-1", "prod"));
    out
}

// ── Building blocks ──────────────────────────────────────────────────────────

fn metric_record(ts: u64, key: &str, value: f64, unit: &str, host: &str, region: &str, env: &str) -> JsonValue {
    json!({
        "timestamp": ts,
        "key": key,
        "data": {
            "value": round2(value), "unit": unit,
            "host": host, "region": region, "env": env,
        }
    })
}

fn log_record(ts: u64, key: &str, region: &str, host: &str, message: &str) -> JsonValue {
    let pid = (ts % 60000) as u32 + 1000;
    let raw = format!("{} {}[{}]: {}", ts, host, pid, message);
    json!({
        "timestamp": ts,
        "key": key,
        "data": {
            "message": message,
            "host":    host,
            "region":  region,
            "pid":     pid.to_string(),
            "raw":     raw,
        }
    })
}

fn pick_host<R: Rng>(rng: &mut R) -> &'static str {
    const HOSTS: &[&str] = &[
        "web-01", "web-02", "web-03",
        "api-01", "api-02",
        "worker-01", "worker-02",
        "k8s-node-01", "k8s-node-02",
        "db-primary", "db-replica-01",
        "cache-01",
    ];
    HOSTS[rng.gen_range(0..HOSTS.len())]
}

fn random_id<R: Rng>(rng: &mut R, len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..len).map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char).collect()
}

fn rand_ts<R: Rng>(rng: &mut R, t0: u64, t1: u64) -> u64 {
    if t0 >= t1 { return t0; }
    rng.gen_range(t0..=t1)
}

fn round2(x: f64) -> f64 { (x * 100.0).round() / 100.0 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_with_seed() {
        let cfg = RealisticConfig {
            duration:      "1h".to_owned(),
            total:         200,
            scenarios:     2,
            noise_ratio:   0.7,
            anomaly_ratio: 0.02,
            seed:          Some(42),
        };
        let a = generate(&cfg);
        let b = generate(&cfg);
        // The `now`-anchored timestamps differ between calls because
        // `now` advances, so we can't compare raw timestamps.  But
        // count + key sequence should match.
        assert_eq!(a.len(), b.len());
        let keys_a: Vec<_> = a.iter().filter_map(|r| r.get("key").and_then(|v| v.as_str())).collect();
        let keys_b: Vec<_> = b.iter().filter_map(|r| r.get("key").and_then(|v| v.as_str())).collect();
        assert_eq!(keys_a, keys_b);
    }

    #[test]
    fn produces_targeted_volume() {
        let cfg = RealisticConfig { total: 500, ..Default::default() };
        let out = generate(&cfg);
        // Scenarios contribute beyond the noise+anomaly target so
        // the corpus comfortably exceeds `total`.  Floor is enough
        // to ensure the generator isn't silently empty.
        assert!(out.len() >= 400, "got only {} records", out.len());
    }

    #[test]
    fn every_record_has_timestamp_and_key() {
        let out = generate(&RealisticConfig { total: 100, scenarios: 1, ..Default::default() });
        for r in &out {
            assert!(r.get("timestamp").and_then(|v| v.as_u64()).is_some(),
                    "record missing timestamp: {r}");
            assert!(r.get("key").and_then(|v| v.as_str()).is_some(),
                    "record missing key: {r}");
        }
    }

    #[test]
    fn output_is_time_ordered() {
        let out = generate(&RealisticConfig { total: 200, ..Default::default() });
        let mut prev = 0u64;
        for r in &out {
            let ts = r["timestamp"].as_u64().unwrap();
            assert!(ts >= prev, "timestamps out of order: {prev} > {ts}");
            prev = ts;
        }
    }

    #[test]
    fn scenarios_emit_known_failure_keys() {
        // With 5 scenarios we should hit at least one of each kind
        // most of the time.  Run with a fixed seed so the test is
        // deterministic.
        let out = generate(&RealisticConfig {
            scenarios: 5, total: 600, seed: Some(7), ..Default::default()
        });
        let keys: std::collections::HashSet<_> = out.iter()
            .filter_map(|r| r.get("key").and_then(|v| v.as_str()))
            .collect();
        // Spot-check a handful of scenario-only keys.
        let any_scenario_key = ["service.crashed", "cert.expired",
                                "deployment.rollback", "cluster.split_brain_event"]
            .iter().any(|k| keys.contains(k));
        assert!(any_scenario_key, "no scenario failure key in output; got {:?}", keys);
    }
}
