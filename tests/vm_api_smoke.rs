//! Standalone-mode end-to-end smoke test for the `vm::api::*` helpers.
//!
//! The DB is initialised against a temp directory with no cluster
//! configured, so every helper takes its standalone path
//! (`db.cluster() == None`).  Cluster-mode behaviour is exercised by
//! Phase 1's 3-node smoke against the `cluster::merge` helpers that
//! `vm::api::*` re-uses.
//!
//! Each integration test file runs as a separate binary so the
//! process-wide `globals::DB` `OnceLock` is fresh for this test.

use bdslib::vm::api;
use rust_dynamic::value::Value;
use serde_json::json;

fn write_config(dir: &tempfile::TempDir) -> String {
    let db_path = dir.path().join("db");
    std::fs::create_dir_all(&db_path).unwrap();
    let config_path = dir.path().join("bds.hjson");
    std::fs::write(
        &config_path,
        format!(
            "{{\n  dbpath: \"{}\"\n  shard_duration: \"1h\"\n  pool_size: 2\n}}\n",
            db_path.display()
        ),
    )
    .unwrap();
    config_path.to_str().unwrap().to_string()
}

#[test]
fn vm_api_standalone_roundtrip() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = write_config(&dir);
    bdslib::init_db(Some(&cfg)).expect("init_db");

    // ── add: a single record, returns its UUID as a string Value ─────────
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    // Use ts a few minutes in the past so it falls strictly inside any
    // "1h" lookback window (the keys() helper's [start, end) range
    // excludes its end, which is wall-clock now).
    let doc = bdslib::vm::helpers::eval::json_to_dynamic(json!({
        "key": "cpu.user",
        "timestamp": now - 60,
        "data": "smoke test row",
    }));
    let id = api::add::add(doc).expect("add");
    let id_str = id.cast_string().expect("id is a string");
    assert!(uuid::Uuid::parse_str(&id_str).is_ok(),
        "vm::api::add returned a non-UUID: {id_str:?}");

    // Standalone mode → meta cleared.
    assert!(api::meta::get().is_none(),
        "standalone mode should clear LAST_META, got {:?}", api::meta::get());

    // ── count: should see at least 1 record (we just added one) ──────────
    let count = api::add::count(Value::nodata()).expect("count");
    let map = match count.data {
        rust_dynamic::types::Val::Map(m) => m,
        other => panic!("count expected Map, got {other:?}"),
    };
    let total = map.get("count").and_then(|v| v.cast_int().ok()).unwrap_or(-1);
    assert!(total >= 1, "expected count >= 1 after add, got {total}");

    // ── primaries: at least one UUID in the listing ──────────────────────
    let primaries = api::primaries::primaries(Value::nodata()).expect("primaries");
    let m = match primaries.data {
        rust_dynamic::types::Val::Map(m) => m,
        other => panic!("primaries expected Map, got {other:?}"),
    };
    let ids = m.get("ids").expect("ids field present");
    let n_ids = match &ids.data {
        rust_dynamic::types::Val::List(l) => l.len(),
        other => panic!("ids expected List, got {other:?}"),
    };
    assert!(n_ids >= 1, "expected ≥1 primary id, got {n_ids}");

    // ── timeline: should have a non-null min_ts now ──────────────────────
    let timeline = api::analysis::timeline().expect("timeline");
    let m = match timeline.data {
        rust_dynamic::types::Val::Map(m) => m,
        other => panic!("timeline expected Map, got {other:?}"),
    };
    let min_ts = m.get("min_ts").expect("min_ts present");
    assert!(!matches!(min_ts.data, rust_dynamic::types::Val::Null),
        "min_ts should be non-null after add, got Null");

    // ── keys: 'cpu.user' should appear in the recent-key listing ─────────
    let keys = api::keys::keys("1h").expect("keys");
    let m = match keys.data {
        rust_dynamic::types::Val::Map(m) => m,
        other => panic!("keys expected Map, got {other:?}"),
    };
    let key_list = m.get("keys").expect("keys field");
    let names: Vec<String> = match &key_list.data {
        rust_dynamic::types::Val::List(l) => l.iter().filter_map(|v| v.clone().cast_string().ok()).collect(),
        other => panic!("keys field expected List, got {other:?}"),
    };
    assert!(names.iter().any(|k| k == "cpu.user"),
        "expected 'cpu.user' in {names:?}");

    // ── duplicates: an empty map (no exact-match dupes from a single add) ─
    let dups = api::add::duplicates(Value::nodata()).expect("duplicates");
    match &dups.data {
        rust_dynamic::types::Val::Map(_) => {},
        other => panic!("duplicates expected Map, got {other:?}"),
    }

    // ── fingerprints_recent: returns a Map with `fingerprints` list ──────
    let fps = api::add::fingerprints_recent("1h").expect("fingerprints_recent");
    let m = match fps.data {
        rust_dynamic::types::Val::Map(m) => m,
        other => panic!("fingerprints_recent expected Map, got {other:?}"),
    };
    assert!(m.get("fingerprints").is_some(), "fingerprints field missing");

    // ── doc_add + doc_get_metadata ───────────────────────────────────────
    let meta = bdslib::vm::helpers::eval::json_to_dynamic(json!({
        "name": "test.txt", "category": "smoke",
    }));
    let did = api::documents::doc_add(meta, b"hello world".to_vec()).expect("doc_add");
    let did_str = did.cast_string().expect("doc id is string");
    let got = api::documents::doc_get_metadata(Value::from_string(did_str.clone()))
        .expect("doc_get_metadata");
    let m = match got.data {
        rust_dynamic::types::Val::Map(m) => m,
        other => panic!("doc metadata expected Map, got {other:?}"),
    };
    let name = m.get("name").and_then(|v| v.clone().cast_string().ok()).unwrap_or_default();
    assert_eq!(name, "test.txt", "round-trip metadata mismatch");

    // ── signal_emit + signal_get ─────────────────────────────────────────
    let extras = bdslib::vm::helpers::eval::json_to_dynamic(json!({"source": "smoke"}));
    let sid = api::signals::signal_emit("test.signal", "info", 1700000123, extras)
        .expect("signal_emit");
    let sid_str = sid.cast_string().expect("signal id string");
    let sig_meta = api::signals::signal_get(Value::from_string(sid_str.clone()))
        .expect("signal_get");
    let m = match sig_meta.data {
        rust_dynamic::types::Val::Map(m) => m,
        other => panic!("signal_get expected Map, got {other:?}"),
    };
    let nm = m.get("name").and_then(|v| v.clone().cast_string().ok()).unwrap_or_default();
    assert_eq!(nm, "test.signal", "signal round-trip name mismatch");

    // Standalone path → LAST_META still cleared after the chain of calls.
    // (signal_get is the most-recent call; it's local-only, which clears.)
    assert!(api::meta::get().is_none(),
        "standalone signal_get should clear LAST_META, got {:?}", api::meta::get());
}
