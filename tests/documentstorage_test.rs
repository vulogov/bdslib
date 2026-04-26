use bdslib::{results_to_strings, DocumentStorage};
use serde_json::json;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn tmp_store() -> (TempDir, DocumentStorage) {
    let dir = TempDir::new().unwrap();
    let store = DocumentStorage::new(dir.path().to_str().unwrap()).unwrap();
    (dir, store)
}

fn vec3(x: f32, y: f32, z: f32) -> Vec<f32> {
    vec![x, y, z]
}

// ── construction ──────────────────────────────────────────────────────────────

#[test]
fn test_new_creates_root_directory() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("docstore");
    DocumentStorage::new(root.to_str().unwrap()).unwrap();
    assert!(root.is_dir());
}

#[test]
fn test_new_creates_directory_layout() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("docstore");
    DocumentStorage::new(root.to_str().unwrap()).unwrap();
    assert!(root.join("metadata.db").exists(), "metadata.db must exist");
    assert!(root.join("blobs.db").exists(), "blobs.db must exist");
    assert!(root.join("vectors").is_dir(), "vectors/ must exist");
}

#[test]
fn test_new_accepts_existing_directory() {
    let dir = TempDir::new().unwrap();
    DocumentStorage::new(dir.path().to_str().unwrap()).unwrap();
    // Second open of the same root must succeed.
    DocumentStorage::new(dir.path().to_str().unwrap()).unwrap();
}

#[test]
fn test_clone_shares_stores() {
    let (_dir, store) = tmp_store();
    let clone = store.clone();
    let id = store
        .add_document(json!({"x": 1}), b"content")
        .unwrap();
    // Clone must see the same metadata and blob.
    assert_eq!(
        clone.get_metadata(id).unwrap().unwrap()["x"],
        json!(1)
    );
    assert_eq!(clone.get_content(id).unwrap().unwrap(), b"content");
}

// ── add_document ──────────────────────────────────────────────────────────────

#[test]
fn test_add_document_returns_non_nil_uuid() {
    let (_dir, store) = tmp_store();
    let id = store.add_document(json!({"key": "val"}), b"data").unwrap();
    assert!(!id.is_nil());
}

#[test]
fn test_add_document_metadata_roundtrip() {
    let (_dir, store) = tmp_store();
    let meta = json!({"title": "Hello", "version": 1});
    let id = store.add_document(meta.clone(), b"body").unwrap();
    let got = store.get_metadata(id).unwrap().expect("metadata must exist");
    assert_eq!(got, meta);
}

#[test]
fn test_add_document_content_roundtrip() {
    let (_dir, store) = tmp_store();
    let id = store
        .add_document(json!({}), b"the quick brown fox")
        .unwrap();
    let got = store.get_content(id).unwrap().expect("content must exist");
    assert_eq!(got, b"the quick brown fox");
}

#[test]
fn test_add_document_empty_content() {
    let (_dir, store) = tmp_store();
    let id = store.add_document(json!({"empty": true}), b"").unwrap();
    let got = store.get_content(id).unwrap().expect("should exist");
    assert!(got.is_empty());
}

#[test]
fn test_add_document_binary_content() {
    let (_dir, store) = tmp_store();
    let data: Vec<u8> = (0u8..=255).collect();
    let id = store.add_document(json!({"type": "binary"}), &data).unwrap();
    let got = store.get_content(id).unwrap().expect("should exist");
    assert_eq!(got, data);
}

#[test]
fn test_add_document_nested_metadata() {
    let (_dir, store) = tmp_store();
    let meta = json!({
        "author": {"name": "Alice", "role": "engineer"},
        "tags": ["rust", "storage"],
        "active": true
    });
    let id = store.add_document(meta.clone(), b"nested").unwrap();
    let got = store.get_metadata(id).unwrap().unwrap();
    assert_eq!(got["author"]["name"], json!("Alice"));
    assert_eq!(got["tags"][0], json!("rust"));
}

#[test]
fn test_add_document_returns_unique_uuids() {
    let (_dir, store) = tmp_store();
    let id1 = store.add_document(json!({"n": 1}), b"a").unwrap();
    let id2 = store.add_document(json!({"n": 2}), b"b").unwrap();
    assert_ne!(id1, id2);
}

#[test]
fn test_add_document_uuids_are_time_ordered() {
    let (_dir, store) = tmp_store();
    let id1 = store.add_document(json!({"i": 1}), b"first").unwrap();
    let id2 = store.add_document(json!({"i": 2}), b"second").unwrap();
    assert!(id1 < id2, "UUIDv7 values must be monotonically increasing");
}

// ── add_document_with_vectors ─────────────────────────────────────────────────

#[test]
fn test_add_with_vectors_metadata_roundtrip() {
    let (_dir, store) = tmp_store();
    let meta = json!({"doc": "vec test"});
    let id = store
        .add_document_with_vectors(
            meta.clone(),
            b"content",
            vec3(1.0, 0.0, 0.0),
            vec3(0.0, 1.0, 0.0),
        )
        .unwrap();
    assert_eq!(store.get_metadata(id).unwrap().unwrap(), meta);
    assert_eq!(store.get_content(id).unwrap().unwrap(), b"content");
}

#[test]
fn test_add_with_vectors_searchable_via_meta() {
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"label": "alpha"}),
            b"alpha content",
            vec3(1.0, 0.0, 0.0),
            vec3(0.0, 0.0, 1.0),
        )
        .unwrap();
    // Query near the meta vector; the doc should appear in results.
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 5).unwrap();
    assert!(
        results.iter().any(|r| r["metadata"]["label"] == json!("alpha")),
        "doc should be found via its meta vector"
    );
}

#[test]
fn test_add_with_vectors_searchable_via_content() {
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"label": "beta"}),
            b"beta content",
            vec3(0.0, 1.0, 0.0),
            vec3(1.0, 0.0, 0.0),  // content vector close to query
        )
        .unwrap();
    // Query near the content vector; the doc should appear in results.
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 5).unwrap();
    assert!(
        results.iter().any(|r| r["metadata"]["label"] == json!("beta")),
        "doc should be found via its content vector"
    );
}

#[test]
fn test_search_document_nearest_first() {
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"label": "close"}),
            b"c",
            vec3(0.99, 0.01, 0.0),  // meta vector close to (1,0,0)
            vec3(0.0, 0.0, 1.0),
        )
        .unwrap();
    store
        .add_document_with_vectors(
            json!({"label": "far"}),
            b"f",
            vec3(0.0, 0.0, 1.0),   // meta vector far from (1,0,0)
            vec3(0.0, 0.0, 1.0),
        )
        .unwrap();
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 2).unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["metadata"]["label"], json!("close"));
}

#[test]
fn test_search_document_returns_metadata_and_content() {
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"color": "blue", "size": 42}),
            b"payload text",
            vec3(1.0, 0.0, 0.0),
            vec3(0.0, 1.0, 0.0),
        )
        .unwrap();
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 1).unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["metadata"]["color"], json!("blue"));
    assert_eq!(results[0]["metadata"]["size"], json!(42));
    assert_eq!(results[0]["document"], json!("payload text"));
}

#[test]
fn test_search_document_finds_via_both_meta_and_content() {
    // Doc A: meta close to query, content far.
    // Doc B: meta far from query, content close.
    // Both should be returned when searching for (1,0,0).
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"label": "meta-close"}),
            b"far content",
            vec3(1.0, 0.0, 0.0),  // meta close
            vec3(0.0, 0.0, 1.0),  // content far
        )
        .unwrap();
    store
        .add_document_with_vectors(
            json!({"label": "meta-far"}),
            b"close content",
            vec3(0.0, 0.0, 1.0),  // meta far
            vec3(1.0, 0.0, 0.0),  // content close
        )
        .unwrap();

    let results = store.search_document(vec3(1.0, 0.0, 0.0), 2).unwrap();
    assert_eq!(results.len(), 2, "both docs must appear in unified search");
    let labels: Vec<&str> = results
        .iter()
        .map(|r| r["metadata"]["label"].as_str().unwrap())
        .collect();
    assert!(labels.contains(&"meta-close"), "found via meta vector");
    assert!(labels.contains(&"meta-far"), "found via content vector");
}

// ── add_document without embedding ───────────────────────────────────────────

#[test]
fn test_add_document_without_embedding_not_in_vector_search() {
    // Documents added without an EmbeddingEngine have no vector entry; the
    // vector index stays empty so search_document returns nothing.
    let (_dir, store) = tmp_store();
    store.add_document(json!({"z": 1}), b"body").unwrap();
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 10).unwrap();
    assert!(
        results.is_empty(),
        "documents added without an embedding engine must not appear in vector search"
    );
}

// ── clone ─────────────────────────────────────────────────────────────────────

#[test]
fn test_clone_shares_vector_index() {
    let (_dir, store) = tmp_store();
    let clone = store.clone();
    store
        .add_document_with_vectors(
            json!({"shared": true}),
            b"via original",
            vec3(1.0, 0.0, 0.0),
            vec3(0.0, 1.0, 0.0),
        )
        .unwrap();
    // The clone must see the same vector index entry.
    let results = clone.search_document(vec3(1.0, 0.0, 0.0), 5).unwrap();
    assert!(
        results.iter().any(|r| r["metadata"]["shared"] == json!(true)),
        "clone must share the same HNSW vector index"
    );
}

// ── get (non-existent) ────────────────────────────────────────────────────────

#[test]
fn test_get_metadata_nonexistent_returns_none() {
    let (_dir, store) = tmp_store();
    let fake = uuid::Uuid::now_v7();
    assert!(store.get_metadata(fake).unwrap().is_none());
}

#[test]
fn test_get_content_nonexistent_returns_none() {
    let (_dir, store) = tmp_store();
    let fake = uuid::Uuid::now_v7();
    assert!(store.get_content(fake).unwrap().is_none());
}

// ── update ────────────────────────────────────────────────────────────────────

#[test]
fn test_update_metadata_changes_value() {
    let (_dir, store) = tmp_store();
    let id = store.add_document(json!({"v": 1}), b"body").unwrap();
    store.update_metadata(id, json!({"v": 2})).unwrap();
    let got = store.get_metadata(id).unwrap().unwrap();
    assert_eq!(got["v"], json!(2));
}

#[test]
fn test_update_metadata_nonexistent_is_ok() {
    let (_dir, store) = tmp_store();
    let fake = uuid::Uuid::now_v7();
    assert!(store.update_metadata(fake, json!({"x": 1})).is_ok());
}

#[test]
fn test_update_content_changes_value() {
    let (_dir, store) = tmp_store();
    let id = store.add_document(json!({}), b"original").unwrap();
    store.update_content(id, b"updated").unwrap();
    let got = store.get_content(id).unwrap().unwrap();
    assert_eq!(got, b"updated");
}

#[test]
fn test_update_content_nonexistent_is_ok() {
    let (_dir, store) = tmp_store();
    let fake = uuid::Uuid::now_v7();
    assert!(store.update_content(fake, b"data").is_ok());
}

// ── delete_document ───────────────────────────────────────────────────────────

#[test]
fn test_delete_removes_metadata() {
    let (_dir, store) = tmp_store();
    let id = store.add_document(json!({"del": true}), b"body").unwrap();
    store.delete_document(id).unwrap();
    assert!(store.get_metadata(id).unwrap().is_none());
}

#[test]
fn test_delete_removes_content() {
    let (_dir, store) = tmp_store();
    let id = store.add_document(json!({}), b"gone").unwrap();
    store.delete_document(id).unwrap();
    assert!(store.get_content(id).unwrap().is_none());
}

#[test]
fn test_delete_removes_from_vector_index() {
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"marker": "gone"}),
            b"data",
            vec3(1.0, 0.0, 0.0),
            vec3(0.0, 1.0, 0.0),
        )
        .unwrap();
    // We need a second doc to distinguish "empty results" from "still there".
    store
        .add_document_with_vectors(
            json!({"marker": "stays"}),
            b"other",
            vec3(0.0, 0.0, 1.0),
            vec3(0.0, 0.0, 1.0),
        )
        .unwrap();

    // Delete the first doc and re-search near its vector.
    let results_before: Vec<_> = store.search_document(vec3(1.0, 0.0, 0.0), 10).unwrap();
    let had_it = results_before
        .iter()
        .any(|r| r["metadata"]["marker"] == json!("gone"));
    assert!(had_it, "doc must appear before deletion");

    // Find and delete it.
    let target = results_before
        .iter()
        .find(|r| r["metadata"]["marker"] == json!("gone"))
        .unwrap();
    let uuid = uuid::Uuid::parse_str(target["id"].as_str().unwrap()).unwrap();
    store.delete_document(uuid).unwrap();

    let results_after = store.search_document(vec3(1.0, 0.0, 0.0), 10).unwrap();
    assert!(
        !results_after
            .iter()
            .any(|r| r["metadata"]["marker"] == json!("gone")),
        "deleted document must not appear in search results"
    );
}

#[test]
fn test_delete_nonexistent_is_ok() {
    let (_dir, store) = tmp_store();
    let fake = uuid::Uuid::now_v7();
    assert!(store.delete_document(fake).is_ok());
}

#[test]
fn test_delete_does_not_affect_other_documents() {
    let (_dir, store) = tmp_store();
    let id_a = store.add_document(json!({"doc": "a"}), b"aaa").unwrap();
    let id_b = store.add_document(json!({"doc": "b"}), b"bbb").unwrap();
    store.delete_document(id_a).unwrap();
    assert!(store.get_metadata(id_b).unwrap().is_some());
    assert!(store.get_content(id_b).unwrap().is_some());
}

// ── store_*_vector helpers ────────────────────────────────────────────────────

#[test]
fn test_store_metadata_vector_makes_document_searchable() {
    let (_dir, store) = tmp_store();
    let id = store.add_document(json!({"label": "late"}), b"data").unwrap();
    store
        .store_metadata_vector(id, vec3(1.0, 0.0, 0.0), json!({"label": "late"}))
        .unwrap();
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 5).unwrap();
    assert!(
        results.iter().any(|r| r["metadata"]["label"] == json!("late")),
        "doc should be findable after explicit metadata vector indexing"
    );
}

#[test]
fn test_store_content_vector_makes_document_searchable() {
    let (_dir, store) = tmp_store();
    let id = store.add_document(json!({"label": "content-only"}), b"text").unwrap();
    store
        .store_content_vector(id, vec3(0.0, 1.0, 0.0))
        .unwrap();
    let results = store.search_document(vec3(0.0, 1.0, 0.0), 5).unwrap();
    assert!(
        results.iter().any(|r| r["metadata"]["label"] == json!("content-only")),
        "doc should be findable after explicit content vector indexing"
    );
}

// ── search ────────────────────────────────────────────────────────────────────

#[test]
fn test_search_document_returns_empty_on_empty_store() {
    let (_dir, store) = tmp_store();
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_search_document_respects_limit() {
    let (_dir, store) = tmp_store();
    for i in 0..5u32 {
        store
            .add_document_with_vectors(
                json!({"i": i}),
                b"x",
                vec![i as f32, 0.0, 0.0],
                vec![i as f32, 0.0, 0.0],
            )
            .unwrap();
    }
    let results = store.search_document(vec3(4.0, 0.0, 0.0), 2).unwrap();
    assert!(results.len() <= 2);
}

#[test]
fn test_search_document_result_has_score() {
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"s": "score test"}),
            b"data",
            vec3(1.0, 0.0, 0.0),
            vec3(0.0, 1.0, 0.0),
        )
        .unwrap();
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 1).unwrap();
    assert!(!results.is_empty());
    let score = results[0]["score"].as_f64().unwrap();
    // Self-query on the meta vector → cosine similarity ≈ 1.0
    assert!(
        score > 0.9,
        "self-query score should be ≈ 1.0, got {score}"
    );
}

#[test]
fn test_search_document_result_has_id_field() {
    let (_dir, store) = tmp_store();
    let id = store
        .add_document_with_vectors(json!({}), b"hi", vec3(1.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0))
        .unwrap();
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 1).unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["id"], json!(id.to_string()));
}

#[test]
fn test_search_document_json_without_embedding_returns_err() {
    let (_dir, store) = tmp_store();
    let result = store.search_document_json(&json!({"title": "test"}), 5);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("EmbeddingEngine"));
}

#[test]
fn test_search_document_text_without_embedding_returns_err() {
    let (_dir, store) = tmp_store();
    let result = store.search_document_text("query text", 5);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("EmbeddingEngine"));
}

// ── sync ──────────────────────────────────────────────────────────────────────

#[test]
fn test_sync_empty_store_is_ok() {
    let (_dir, store) = tmp_store();
    store.sync().unwrap();
}

#[test]
fn test_sync_after_adds_is_ok() {
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"synced": true}),
            b"data",
            vec3(1.0, 0.0, 0.0),
            vec3(0.0, 1.0, 0.0),
        )
        .unwrap();
    store.sync().unwrap();
}

// ── persistence across reopens ────────────────────────────────────────────────

#[test]
fn test_metadata_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_str().unwrap();
    let meta = json!({"persisted": true, "value": 42});

    let id = {
        let store = DocumentStorage::new(root).unwrap();
        store.add_document(meta.clone(), b"body").unwrap()
    };

    let store2 = DocumentStorage::new(root).unwrap();
    let got = store2.get_metadata(id).unwrap().expect("must survive reopen");
    assert_eq!(got, meta);
}

#[test]
fn test_content_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_str().unwrap();
    let data = b"persisted content bytes";

    let id = {
        let store = DocumentStorage::new(root).unwrap();
        store.add_document(json!({}), data).unwrap()
    };

    let store2 = DocumentStorage::new(root).unwrap();
    let got = store2.get_content(id).unwrap().expect("must survive reopen");
    assert_eq!(got, data);
}

#[test]
fn test_vector_index_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_str().unwrap();

    {
        let store = DocumentStorage::new(root).unwrap();
        store
            .add_document_with_vectors(
                json!({"k": "v"}),
                b"body",
                vec3(1.0, 0.0, 0.0),
                vec3(0.0, 1.0, 0.0),
            )
            .unwrap();
        store.sync().unwrap();
    }

    let store2 = DocumentStorage::new(root).unwrap();
    let results = store2.search_document(vec3(1.0, 0.0, 0.0), 5).unwrap();
    assert!(
        results.iter().any(|r| r["metadata"]["k"] == json!("v")),
        "vector index must survive reopen after sync"
    );
}

// ── results_to_strings / search_*_strings ────────────────────────────────────

#[test]
fn test_results_to_strings_empty() {
    assert!(results_to_strings(&[]).is_empty());
}

#[test]
fn test_results_to_strings_contains_field_values() {
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"title": "fingerprint test"}),
            b"hello world",
            vec3(1.0, 0.0, 0.0),
            vec3(0.0, 1.0, 0.0),
        )
        .unwrap();
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 1).unwrap();
    let strings = results_to_strings(&results);
    assert_eq!(strings.len(), 1);
    // json_fingerprint produces "path: value" pairs; check key fields appear.
    assert!(
        strings[0].contains("fingerprint test"),
        "title value must appear in fingerprint: {}",
        strings[0]
    );
    assert!(
        strings[0].contains("hello world"),
        "document content must appear in fingerprint: {}",
        strings[0]
    );
}

#[test]
fn test_results_to_strings_includes_score() {
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"k": "v"}),
            b"data",
            vec3(1.0, 0.0, 0.0),
            vec3(0.0, 1.0, 0.0),
        )
        .unwrap();
    let results = store.search_document(vec3(1.0, 0.0, 0.0), 1).unwrap();
    let strings = results_to_strings(&results);
    assert!(strings[0].contains("score"), "score field must appear in fingerprint");
}

#[test]
fn test_search_document_strings_returns_same_count_as_search_document() {
    let (_dir, store) = tmp_store();
    for i in 0..3u32 {
        store
            .add_document_with_vectors(
                json!({"i": i}),
                b"x",
                vec![i as f32, 0.0, 0.0],
                vec![0.0, 0.0, 1.0],
            )
            .unwrap();
    }
    let json_results = store.search_document(vec3(2.0, 0.0, 0.0), 3).unwrap();
    let str_results = store
        .search_document_strings(vec3(2.0, 0.0, 0.0), 3)
        .unwrap();
    assert_eq!(str_results.len(), json_results.len());
}

#[test]
fn test_search_document_strings_returns_strings() {
    let (_dir, store) = tmp_store();
    store
        .add_document_with_vectors(
            json!({"lang": "rust"}),
            b"systems",
            vec3(1.0, 0.0, 0.0),
            vec3(0.0, 1.0, 0.0),
        )
        .unwrap();
    let strings = store
        .search_document_strings(vec3(1.0, 0.0, 0.0), 1)
        .unwrap();
    assert_eq!(strings.len(), 1);
    assert!(strings[0].contains("rust"));
    assert!(strings[0].contains("systems"));
}

#[test]
fn test_search_document_json_strings_without_embedding_returns_err() {
    let (_dir, store) = tmp_store();
    let result = store.search_document_json_strings(&json!({"q": "x"}), 5);
    assert!(result.is_err());
}

#[test]
fn test_search_document_text_strings_without_embedding_returns_err() {
    let (_dir, store) = tmp_store();
    let result = store.search_document_text_strings("query", 5);
    assert!(result.is_err());
}

// ── with_embedding (live model) ───────────────────────────────────────────────

#[test]
#[ignore]
fn test_with_embedding_add_document_indexes_vectors() {
    use bdslib::embedding::Model;
    use bdslib::EmbeddingEngine;

    let dir = TempDir::new().unwrap();
    let emb = EmbeddingEngine::new(Model::AllMiniLML6V2, None).unwrap();
    let store = DocumentStorage::with_embedding(dir.path().to_str().unwrap(), emb).unwrap();

    store
        .add_document(
            json!({"title": "Rust programming", "domain": "systems"}),
            b"memory safe systems language",
        )
        .unwrap();
    store
        .add_document(
            json!({"title": "Python data science", "domain": "ml"}),
            b"machine learning numpy pandas",
        )
        .unwrap();

    // Metadata search: closest to "systems language" should be the Rust doc.
    let meta_results = store
        .search_document_json(&json!({"title": "Rust systems", "domain": "systems"}), 2)
        .unwrap();
    assert!(!meta_results.is_empty());
    assert_eq!(meta_results[0]["metadata"]["domain"], json!("systems"));

    // Content search: closest to "machine learning" should be the Python doc.
    let content_results = store
        .search_document_text("machine learning library", 2)
        .unwrap();
    assert!(!content_results.is_empty());
    assert_eq!(content_results[0]["metadata"]["domain"], json!("ml"));
}
