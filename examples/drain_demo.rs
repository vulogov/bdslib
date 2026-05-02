/// drain_demo — Automatic log-template discovery via drain3 inside ShardsManager.
///
/// Sections:
///   1. Setup — write a temp hjson config with drain_enabled=true
///   2. Ingest — add 108 synthetic log entries (single-doc and batch)
///   3. Discovery — list and print every discovered drain template
///   4. Search   — semantic search over the template store
///   5. Reload   — demonstrate drain_load() pre-seeding a fresh parser
use bdslib::common::error::Result;
use bdslib::common::time::now_secs;
use bdslib::embedding::Model;
use bdslib::shardsmanager::ShardsManager;
use bdslib::EmbeddingEngine;
use serde_json::{json, Value};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn ts_ago(secs: u64) -> u64 {
    now_secs().saturating_sub(secs)
}

fn doc(key: &str, data: &str, timestamp: u64) -> Value {
    json!({ "key": key, "data": data, "timestamp": timestamp })
}

// ── log-line templates with variable slots ────────────────────────────────────

/// Generate a batch of realistic SRE-style log lines with injected variable values.
fn generate_logs(base_ts: u64) -> Vec<Value> {
    let mut logs: Vec<Value> = Vec::new();
    let step = 30u64; // 30 seconds between records

    // Template A — "user <name> logged in from <ip>"
    let users = ["alice", "bob", "carol", "dave", "eve"];
    let ips = ["10.0.0.1", "10.0.0.2", "192.168.1.10", "172.16.0.5"];
    for (i, u) in users.iter().enumerate() {
        let ip = ips[i % ips.len()];
        logs.push(doc("auth", &format!("user {u} logged in from {ip}"),
            base_ts + logs.len() as u64 * step));
    }

    // Template B — "connection to <host>:<port> established"
    for (host, port) in [("db1", 5432u16), ("db2", 5432), ("cache1", 6379), ("cache2", 6379)] {
        logs.push(doc("network", &format!("connection to {host}:{port} established"),
            base_ts + logs.len() as u64 * step));
    }

    // Template C — "request <method> <path> completed in <n> ms"
    let requests = [
        ("GET", "/api/v1/status", 12), ("POST", "/api/v1/ingest", 87),
        ("GET", "/api/v1/search", 45), ("DELETE", "/api/v1/record/123", 9),
        ("GET", "/api/v1/status", 15), ("POST", "/api/v1/ingest", 102),
    ];
    for (method, path, ms) in requests {
        logs.push(doc("http", &format!("request {method} {path} completed in {ms} ms"),
            base_ts + logs.len() as u64 * step));
    }

    // Template D — "disk usage on <volume> is <n>%"
    for (vol, pct) in [("/dev/sda1", 72), ("/dev/sdb1", 45), ("/dev/sda1", 75), ("/dev/sdb1", 46)] {
        logs.push(doc("disk", &format!("disk usage on {vol} is {pct}%"),
            base_ts + logs.len() as u64 * step));
    }

    // Template E — "worker <id> started processing job <job_id>"
    for (wid, jid) in [(1u32, 1001u32), (2, 1002), (3, 1003), (1, 1004), (2, 1005)] {
        logs.push(doc("worker", &format!("worker {wid} started processing job {jid}"),
            base_ts + logs.len() as u64 * step));
    }

    // Template F — "shard <name> opened at path <path>"
    for (name, path) in [("alpha", "/data/alpha"), ("beta", "/data/beta"), ("gamma", "/data/gamma")] {
        logs.push(doc("storage", &format!("shard {name} opened at path {path}"),
            base_ts + logs.len() as u64 * step));
    }

    // Template G — "error code <n> on service <svc>: <msg>"
    for (code, svc, msg) in [
        (500u32, "auth", "internal server error"),
        (503, "ingest", "upstream unavailable"),
        (429, "api", "rate limit exceeded"),
        (500, "auth", "database timeout"),
    ] {
        logs.push(doc("error", &format!("error code {code} on service {svc}: {msg}"),
            base_ts + logs.len() as u64 * step));
    }

    // Template H — "cache hit ratio for key prefix <prefix> is <ratio>"
    for (prefix, ratio) in [("session:", "0.92"), ("user:", "0.87"), ("session:", "0.91")] {
        logs.push(doc("cache", &format!("cache hit ratio for key prefix {prefix} is {ratio}"),
            base_ts + logs.len() as u64 * step));
    }

    // Template I — "backup of <db> completed in <n> seconds"
    for (db, secs) in [("postgres", 42u32), ("redis", 7), ("postgres", 44)] {
        logs.push(doc("backup", &format!("backup of {db} completed in {secs} seconds"),
            base_ts + logs.len() as u64 * step));
    }

    // Template J — "alert: <metric> crossed threshold <val> on host <host>"
    for (metric, val, host) in [
        ("cpu_usage", "90%", "host-01"), ("mem_usage", "85%", "host-02"),
        ("cpu_usage", "93%", "host-01"), ("latency_p99", "500ms", "host-03"),
    ] {
        logs.push(doc("alert", &format!("alert: {metric} crossed threshold {val} on host {host}"),
            base_ts + logs.len() as u64 * step));
    }

    // Repeats to grow cluster sizes and confirm merging
    for u in ["frank", "grace", "hank", "ivan", "judy"] {
        logs.push(doc("auth", &format!("user {u} logged in from 10.0.0.3"),
            base_ts + logs.len() as u64 * step));
    }
    for (host, port) in [("db3", 5432u16), ("cache3", 6379), ("db4", 5432), ("cache4", 6379)] {
        logs.push(doc("network", &format!("connection to {host}:{port} established"),
            base_ts + logs.len() as u64 * step));
    }
    for (wid, jid) in [(4u32, 1006u32), (5, 1007), (1, 1008), (2, 1009), (3, 1010)] {
        logs.push(doc("worker", &format!("worker {wid} started processing job {jid}"),
            base_ts + logs.len() as u64 * step));
    }
    for (code, svc, msg) in [
        (500u32, "search", "index unavailable"),
        (503, "api", "backend timeout"),
        (429, "ingest", "rate limit exceeded"),
    ] {
        logs.push(doc("error", &format!("error code {code} on service {svc}: {msg}"),
            base_ts + logs.len() as u64 * step));
    }

    logs
}

// ── main ──────────────────────────────────────────────────────────────────────

fn run() -> Result<()> {
    let _ = env_logger::try_init();

    // ── Section 1: Setup ─────────────────────────────────────────────────────

    println!("=== Section 1: Setup ===");

    let dir = TempDir::new().unwrap();
    let dbpath = dir.path().join("db");
    let cfg_path = dir.path().join("bds.hjson");

    let hjson = format!(
        r#"{{
  dbpath: "{}"
  shard_duration: "1h"
  pool_size: 4
  similarity_threshold: 0.85
  drain_enabled: true
  drain_load_duration: "24h"
}}"#,
        dbpath.display()
    );
    std::fs::write(&cfg_path, &hjson).unwrap();

    let embedding = EmbeddingEngine::new(Model::AllMiniLML6V2, None)
        .map_err(|e| bdslib::common::error::err_msg(format!("{e}")))?;
    let manager = ShardsManager::with_embedding(cfg_path.to_str().unwrap(), embedding)?;

    println!("ShardsManager created with drain_enabled=true");
    println!("  dbpath: {}", dbpath.display());

    // ── Section 2: Ingest ────────────────────────────────────────────────────

    println!("\n=== Section 2: Ingest ===");

    let base_ts = ts_ago(1800); // 30 minutes ago so all docs fit in the lookback
    let logs = generate_logs(base_ts);
    let total = logs.len();
    println!("Generated {total} log documents");

    // Add first 20 one-by-one (exercises add())
    let single_count = 20.min(total);
    for doc in logs[..single_count].iter().cloned() {
        manager.add(doc)?;
    }
    println!("  add()       → {single_count} docs");

    // Add the rest as a batch (exercises add_batch())
    let batch = logs[single_count..].to_vec();
    let batch_count = batch.len();
    manager.add_batch(batch)?;
    println!("  add_batch() → {batch_count} docs");

    println!("Total ingested: {}", single_count + batch_count);

    // ── Section 3: Discovery ─────────────────────────────────────────────────

    println!("\n=== Section 3: Discovered templates ===");

    let templates = manager.tpl_list("2h")?;
    println!("Templates found: {}", templates.len());
    println!();

    let mut sorted = templates.clone();
    sorted.sort_by_key(|(_, m)| {
        m.get("cluster_id").and_then(|v| v.as_u64()).unwrap_or(0)
    });

    for (id, meta) in &sorted {
        let name = meta.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let cid  = meta.get("cluster_id").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("  [{cid:>2}] {name}");
        println!("       uuid={id}");
    }

    // ── Section 4: Semantic search ────────────────────────────────────────────

    println!("\n=== Section 4: Semantic search ===");

    let queries = [
        ("database connection", 3),
        ("user authentication login", 3),
        ("cpu memory alert threshold", 3),
        ("disk storage capacity", 3),
    ];

    for (query, limit) in queries {
        let results = manager.tpl_search_text("2h", query, limit)?;
        println!("\nQuery: {:?}  (top {limit})", query);
        for r in &results {
            let name  = r.get("metadata").and_then(|m| m.get("name")).and_then(|v| v.as_str()).unwrap_or("?");
            let score = r.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            println!("  {score:.4}  {name}");
        }
    }

    // ── Section 5: Reload ─────────────────────────────────────────────────────

    println!("\n=== Section 5: Reload — drain_load() ===");

    let mut reloaded = manager.drain_load("2h")?;
    println!("Seeded a fresh DrainParser from stored templates");
    println!("  clusters in reloaded parser: {}", reloaded.clusters().len());

    // Parse a new line with the reloaded parser (no global DB — uses instance API)
    let new_doc = doc("auth", "user ivan logged in from 10.0.1.50", now_secs());
    let result = manager.drain_parse_json(&mut reloaded, &new_doc)?;
    println!("\nParsed new log line with reloaded parser:");
    println!("  template : {}", result.template.join(" "));
    println!("  change   : {:?}", result.change_type);
    println!("  cluster  : id={} size={}", result.cluster_id, result.cluster_size);

    println!("\nDone.");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
