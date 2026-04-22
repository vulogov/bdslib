use crate::common::error::{err_msg, Result};
use crate::EmbeddingEngine;
use parking_lot::Mutex;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;
use vecstore::reranking::Reranker;
use vecstore::{Metadata, Query, VecStore};

pub use vecstore::Neighbor as SearchResult;

/// Thread-safe vector store backed by vecstore's HNSW index.
///
/// Wraps `vecstore::VecStore` behind an `Arc<Mutex<_>>` so it can be cloned
/// and shared across threads. An optional [`EmbeddingEngine`] enables
/// automatic text-to-vector conversion via [`store_document`] and
/// [`search_json`].
///
/// [`store_document`]: VectorEngine::store_document
/// [`search_json`]: VectorEngine::search_json
#[derive(Clone)]
pub struct VectorEngine {
    store: Arc<Mutex<VecStore>>,
    embedding: Option<Arc<EmbeddingEngine>>,
}

impl VectorEngine {
    /// Open or create a vector store at `path`.
    ///
    /// The directory (and index files) are created automatically if they do
    /// not exist.
    ///
    /// `store_document` and `search_json` are not available on engines created
    /// with `new`; use [`with_embedding`] instead.
    ///
    /// [`with_embedding`]: VectorEngine::with_embedding
    pub fn new(path: &str) -> Result<Self> {
        let store = VecStore::open(path)
            .map_err(|e| err_msg(format!("Failed to open vector store at {path:?}: {e}")))?;
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            embedding: None,
        })
    }

    /// Open or create a vector store at `path`, with an [`EmbeddingEngine`]
    /// for automatic text embedding via [`store_document`] and [`search_json`].
    ///
    /// [`store_document`]: VectorEngine::store_document
    /// [`search_json`]: VectorEngine::search_json
    pub fn with_embedding(path: &str, engine: EmbeddingEngine) -> Result<Self> {
        let store = VecStore::open(path)
            .map_err(|e| err_msg(format!("Failed to open vector store at {path:?}: {e}")))?;
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            embedding: Some(Arc::new(engine)),
        })
    }

    // ── writes ────────────────────────────────────────────────────────────────

    /// Store an `id → vector` association.
    ///
    /// `metadata` is an optional JSON object whose fields are stored alongside
    /// the vector and returned in search results. Pass `None` for no metadata.
    ///
    /// If a record with the same `id` already exists it is replaced (upsert).
    pub fn store_vector(
        &self,
        id: &str,
        vector: Vec<f32>,
        metadata: Option<JsonValue>,
    ) -> Result<()> {
        let meta = json_to_metadata(metadata.unwrap_or(JsonValue::Object(Default::default())));
        self.store
            .lock()
            .upsert(id.to_string(), vector, meta)
            .map_err(|e| err_msg(format!("Failed to store vector {id:?}: {e}")))
    }

    /// Embed `document` using the attached [`EmbeddingEngine`] and store the
    /// resulting vector under `id`.
    ///
    /// The document is converted to a fingerprint string via [`json_fingerprint`]
    /// before embedding. The full JSON is persisted as metadata and returned in
    /// search results.
    ///
    /// Returns `Err` if no `EmbeddingEngine` was provided at construction time.
    pub fn store_document(&self, id: &str, document: JsonValue) -> Result<()> {
        let engine = self.require_embedding("store_document")?;
        let fingerprint = json_fingerprint(&document);
        let vector = engine.embed(&fingerprint)?;
        let meta = json_to_metadata(document);
        self.store
            .lock()
            .upsert(id.to_string(), vector, meta)
            .map_err(|e| err_msg(format!("Failed to store document {id:?}: {e}")))
    }

    // ── searches ──────────────────────────────────────────────────────────────

    /// Return the `limit` nearest neighbours to `query_vector`, ordered by
    /// descending similarity score (1.0 = identical, 0.0 = orthogonal).
    pub fn search(&self, query_vector: Vec<f32>, limit: usize) -> Result<Vec<SearchResult>> {
        let q = Query::new(query_vector).with_limit(limit);
        let mut results = self
            .store
            .lock()
            .query(q)
            .map_err(|e| err_msg(format!("Vector search failed: {e}")))?;
        distance_to_similarity(&mut results);
        Ok(results)
    }

    /// Search for the `candidate_pool` nearest neighbours, then re-rank with
    /// `reranker` and return the top `limit` results.
    ///
    /// `query_text` is forwarded to the reranker for semantic scoring (e.g.
    /// cross-encoder models). Pass an empty string for rerankers that do not
    /// use query text (e.g. MMR).
    pub fn search_reranked(
        &self,
        query_vector: Vec<f32>,
        query_text: &str,
        limit: usize,
        candidate_pool: usize,
        reranker: &dyn Reranker,
    ) -> Result<Vec<SearchResult>> {
        let pool = candidate_pool.max(limit);
        let q = Query::new(query_vector).with_limit(pool);
        let mut candidates = self
            .store
            .lock()
            .query(q)
            .map_err(|e| err_msg(format!("Vector search failed: {e}")))?;
        // Convert before reranking: rerankers treat score as similarity (higher = better).
        distance_to_similarity(&mut candidates);
        reranker
            .rerank(query_text, candidates, limit)
            .map_err(|e| err_msg(format!("Reranking failed: {e}")))
    }

    /// Fingerprint `query` using [`json_fingerprint`], embed the result, and
    /// return the `limit` nearest stored documents.
    ///
    /// Use the same JSON structure as was passed to [`store_document`] so that
    /// field paths in the query align with field paths in the index.
    ///
    /// Returns `Err` if no `EmbeddingEngine` was provided at construction time.
    pub fn search_json(&self, query: &JsonValue, limit: usize) -> Result<Vec<SearchResult>> {
        let engine = self.require_embedding("search_json")?;
        let fingerprint = json_fingerprint(query);
        let vector = engine.embed(&fingerprint)?;
        self.search(vector, limit)
    }

    /// Fingerprint `query`, embed it, search `candidate_pool` neighbours, then
    /// re-rank with `reranker` and return the top `limit` results.
    ///
    /// The fingerprint string is also passed as `query_text` to the reranker,
    /// so semantic rerankers (e.g. cross-encoder) receive meaningful input.
    ///
    /// Returns `Err` if no `EmbeddingEngine` was provided at construction time.
    pub fn search_json_reranked(
        &self,
        query: &JsonValue,
        limit: usize,
        candidate_pool: usize,
        reranker: &dyn Reranker,
    ) -> Result<Vec<SearchResult>> {
        let engine = self.require_embedding("search_json_reranked")?;
        let fingerprint = json_fingerprint(query);
        let vector = engine.embed(&fingerprint)?;
        self.search_reranked(vector, &fingerprint, limit, candidate_pool, reranker)
    }

    // ── persistence ───────────────────────────────────────────────────────────

    /// Flush the in-memory index and all records to disk.
    ///
    /// For file-backed stores this is necessary to persist changes across
    /// process restarts. For in-process stores the call succeeds but has no
    /// durable effect.
    pub fn sync(&self) -> Result<()> {
        self.store
            .lock()
            .save()
            .map_err(|e| err_msg(format!("Failed to sync vector store: {e}")))
    }

    // ── internal ──────────────────────────────────────────────────────────────

    fn require_embedding(&self, caller: &str) -> Result<Arc<EmbeddingEngine>> {
        self.embedding.clone().ok_or_else(|| {
            err_msg(format!(
                "{caller} requires an EmbeddingEngine — use VectorEngine::with_embedding"
            ))
        })
    }
}

// ── score conversion ──────────────────────────────────────────────────────────

// vecstore returns cosine *distance* (lower = more similar). Convert in-place
// to cosine *similarity* (higher = more similar) so that callers and rerankers
// both see the natural convention: score 1.0 = identical, 0.0 = orthogonal.
fn distance_to_similarity(results: &mut Vec<SearchResult>) {
    for r in results.iter_mut() {
        r.score = 1.0 - r.score;
    }
}

// ── JSON fingerprinting ───────────────────────────────────────────────────────

/// Convert a JSON value into a flat, human-readable fingerprint string suitable
/// for embedding.
///
/// The algorithm walks the JSON tree recursively and emits `path: value` pairs
/// for every leaf, preserving field-name context at every depth:
///
/// ```text
/// { "title": "Rust",
///   "meta": { "year": 2015, "tags": ["systems", "safe"] } }
/// →
/// "title: Rust meta.year: 2015 meta.tags[0]: systems meta.tags[1]: safe"
/// ```
///
/// Rules:
/// - **Objects** — recurse with dot-separated path prefix.
/// - **Arrays** — recurse with `[i]` index appended to the path.
/// - **Strings** — emitted as `path: value` (field name retained for context).
/// - **Numbers / booleans** — emitted as `path: value`.
/// - **Null** — skipped (carries no semantic content).
/// - **Top-level primitives** — emitted as-is without a path prefix.
pub fn json_fingerprint(json: &JsonValue) -> String {
    let mut parts = Vec::new();
    collect_leaves(json, "", &mut parts);
    parts.join(" ")
}

fn collect_leaves(value: &JsonValue, path: &str, out: &mut Vec<String>) {
    match value {
        JsonValue::Object(map) => {
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                collect_leaves(child, &child_path, out);
            }
        }
        JsonValue::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let child_path = if path.is_empty() {
                    format!("[{i}]")
                } else {
                    format!("{path}[{i}]")
                };
                collect_leaves(item, &child_path, out);
            }
        }
        JsonValue::String(s) => {
            if path.is_empty() {
                out.push(s.clone());
            } else {
                out.push(format!("{path}: {s}"));
            }
        }
        JsonValue::Number(n) => {
            if path.is_empty() {
                out.push(n.to_string());
            } else {
                out.push(format!("{path}: {n}"));
            }
        }
        JsonValue::Bool(b) => {
            if path.is_empty() {
                out.push(b.to_string());
            } else {
                out.push(format!("{path}: {b}"));
            }
        }
        JsonValue::Null => {} // no semantic content
    }
}

// ── metadata conversion ───────────────────────────────────────────────────────

fn json_to_metadata(json: JsonValue) -> Metadata {
    let fields = match json {
        JsonValue::Object(map) => map.into_iter().collect(),
        other => {
            let mut m = HashMap::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    Metadata { fields }
}
