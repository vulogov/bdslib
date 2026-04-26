use crate::common::error::{err_msg, Result};
use crate::common::jsonfingerprint::json_fingerprint;
use crate::common::uuid::generate_v7;
use crate::datastorage::{BlobStorage, JsonStorage, JsonStorageConfig};
use crate::EmbeddingEngine;
use crate::VectorEngine;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

// ── directory layout ──────────────────────────────────────────────────────────
//
//  {root}/
//  ├── metadata.db    JsonStorage  — per-document JSON metadata
//  ├── blobs.db       BlobStorage  — raw document bytes
//  └── vectors/       VectorEngine — combined index; IDs: "{uuid}:meta" and
//                                   "{uuid}:content" in the same HNSW store

/// Combined document store: JSON metadata, raw byte content, and a single
/// vector index that holds both metadata-fingerprint embeddings and
/// document-content embeddings.
///
/// Every document is identified by a single UUIDv7. Two vector entries share
/// that UUID as their prefix — `"{uuid}:meta"` for the metadata embedding and
/// `"{uuid}:content"` for the content embedding — so a single
/// [`search_document`] call can surface a document via either signal.
///
/// # Search model
///
/// [`search_document`] queries the unified vector index, deduplicates raw hits
/// by UUID (keeping the best score per document), sorts by descending
/// similarity, and returns the top `limit` documents as JSON objects with
/// three keys:
///
/// ```json
/// { "id": "<uuid>", "metadata": { … }, "document": "<utf-8 content>", "score": 0.97 }
/// ```
///
/// # Graceful degradation
///
/// When created via [`new`] (no embedding engine), [`add_document`] still
/// stores metadata and blob; vector indexing is silently skipped. Call
/// [`add_document_with_vectors`] to supply pre-computed vectors explicitly, or
/// use [`with_embedding`] to enable automatic embedding on every insert.
///
/// # `Clone`-able
///
/// All internal stores are backed by `Arc`; cloning is cheap and all clones
/// share the same underlying state.
///
/// [`new`]: DocumentStorage::new
/// [`with_embedding`]: DocumentStorage::with_embedding
/// [`add_document`]: DocumentStorage::add_document
/// [`add_document_with_vectors`]: DocumentStorage::add_document_with_vectors
/// [`search_document`]: DocumentStorage::search_document
#[derive(Clone)]
pub struct DocumentStorage {
    meta:    JsonStorage,
    blobs:   BlobStorage,
    vectors: VectorEngine,
}

impl DocumentStorage {
    /// Open or create a `DocumentStorage` rooted at `root`.
    ///
    /// The directory tree is created automatically. Vector indexing requires an
    /// embedding engine; use [`with_embedding`] to enable it.
    ///
    /// [`with_embedding`]: DocumentStorage::with_embedding
    pub fn new(root: &str) -> Result<Self> {
        let paths = Paths::from(root)?;
        Ok(Self {
            meta:    JsonStorage::new(&paths.metadata_db, 4, meta_config())?,
            blobs:   BlobStorage::new(&paths.blobs_db, 4)?,
            vectors: VectorEngine::new(&paths.vec)?,
        })
    }

    /// Open or create a `DocumentStorage` rooted at `root`, with an
    /// [`EmbeddingEngine`] for automatic vector indexing.
    ///
    /// [`add_document`] will embed the JSON metadata fingerprint as
    /// `"{uuid}:meta"` and the document text as `"{uuid}:content"` into the
    /// shared vector index.
    ///
    /// [`add_document`]: DocumentStorage::add_document
    pub fn with_embedding(root: &str, engine: EmbeddingEngine) -> Result<Self> {
        let paths = Paths::from(root)?;
        Ok(Self {
            meta:    JsonStorage::new(&paths.metadata_db, 4, meta_config())?,
            blobs:   BlobStorage::new(&paths.blobs_db, 4)?,
            vectors: VectorEngine::with_embedding(&paths.vec, engine)?,
        })
    }

    // ── writes ────────────────────────────────────────────────────────────────

    /// Store a document.
    ///
    /// - `metadata` is stored verbatim in the JSON store.
    /// - `content` is stored as raw bytes in the blob store.
    /// - If an embedding engine is configured, the `json_fingerprint` of
    ///   `metadata` is stored as `"{uuid}:meta"` and `content` (decoded as
    ///   UTF-8) is stored as `"{uuid}:content"` in the shared vector index.
    ///   Vector indexing is silently skipped when no engine is present.
    ///
    /// Returns the generated UUIDv7 that identifies this document.
    pub fn add_document(&self, metadata: JsonValue, content: &[u8]) -> Result<Uuid> {
        let id = generate_v7();
        let id_str = id.to_string();

        self.meta.add_json_with_id(id, metadata.clone())?;
        self.blobs.add_blob_with_key(id, content)?;

        let _ = self.vectors.store_document(&format!("{id_str}:meta"), metadata);
        let content_text = String::from_utf8_lossy(content).into_owned();
        let _ = self.vectors.store_document(&format!("{id_str}:content"), serde_json::json!(content_text));

        Ok(id)
    }

    /// Store a document with caller-supplied pre-computed vectors.
    ///
    /// This is the testable, embedding-free path. `meta_vec` is stored under
    /// `"{uuid}:meta"` with `metadata` as vecstore metadata; `content_vec` is
    /// stored under `"{uuid}:content"` with no extra metadata.
    ///
    /// Returns the generated UUIDv7.
    pub fn add_document_with_vectors(
        &self,
        metadata: JsonValue,
        content: &[u8],
        meta_vec: Vec<f32>,
        content_vec: Vec<f32>,
    ) -> Result<Uuid> {
        let id = generate_v7();
        let id_str = id.to_string();

        self.meta.add_json_with_id(id, metadata.clone())?;
        self.blobs.add_blob_with_key(id, content)?;
        self.vectors.store_vector(&format!("{id_str}:meta"), meta_vec, Some(metadata))?;
        self.vectors.store_vector(&format!("{id_str}:content"), content_vec, None)?;

        Ok(id)
    }

    /// Replace the metadata for `id` and set its `updated_at` to now.
    ///
    /// Returns `Ok(())` even if `id` does not exist (no-op).
    /// The vector index is **not** updated; call [`store_metadata_vector`] or
    /// [`store_content_vector`] explicitly if needed.
    ///
    /// [`store_metadata_vector`]: DocumentStorage::store_metadata_vector
    /// [`store_content_vector`]: DocumentStorage::store_content_vector
    pub fn update_metadata(&self, id: Uuid, metadata: JsonValue) -> Result<()> {
        self.meta.update_json(id, metadata)
    }

    /// Replace the raw content for `id` and set its `updated_at` to now.
    ///
    /// Returns `Ok(())` even if `id` does not exist (no-op).
    pub fn update_content(&self, id: Uuid, content: &[u8]) -> Result<()> {
        self.blobs.update_blob(id, content)
    }

    /// Explicitly (re-)index the metadata vector for `id`.
    ///
    /// Stored under `"{id}:meta"` in the shared vector index.
    pub fn store_metadata_vector(
        &self,
        id: Uuid,
        meta_vec: Vec<f32>,
        metadata: JsonValue,
    ) -> Result<()> {
        self.vectors.store_vector(&format!("{id}:meta"), meta_vec, Some(metadata))
    }

    /// Explicitly (re-)index the content vector for `id`.
    ///
    /// Stored under `"{id}:content"` in the shared vector index.
    pub fn store_content_vector(&self, id: Uuid, content_vec: Vec<f32>) -> Result<()> {
        self.vectors.store_vector(&format!("{id}:content"), content_vec, None)
    }

    /// Remove the document from all stores (metadata, blob, vector index).
    ///
    /// Returns `Ok(())` for non-existent `id` (no-op in each sub-store).
    pub fn delete_document(&self, id: Uuid) -> Result<()> {
        let id_str = id.to_string();
        self.meta.drop_json(id)?;
        self.blobs.drop_blob(id)?;
        self.vectors.delete_vector(&format!("{id_str}:meta"))?;
        self.vectors.delete_vector(&format!("{id_str}:content"))?;
        Ok(())
    }

    // ── reads ─────────────────────────────────────────────────────────────────

    /// Return the metadata stored under `id`, or `None` if no such document
    /// exists.
    pub fn get_metadata(&self, id: Uuid) -> Result<Option<JsonValue>> {
        self.meta.get_json(id)
    }

    /// Return the raw content stored under `id`, or `None` if no such document
    /// exists.
    pub fn get_content(&self, id: Uuid) -> Result<Option<Vec<u8>>> {
        self.blobs.get_blob(id)
    }

    // ── vector search ─────────────────────────────────────────────────────────

    /// Return the `limit` most relevant documents for a pre-computed query
    /// vector.
    ///
    /// Both `":meta"` and `":content"` entries are searched in the shared
    /// vector index. Hits are deduplicated by UUID (keeping the best score per
    /// document), sorted by descending similarity, and resolved to full
    /// documents by loading metadata from [`JsonStorage`] and content from
    /// [`BlobStorage`].
    ///
    /// Each returned element is a JSON object with four keys:
    /// - `"id"`: the document UUID string
    /// - `"metadata"`: the JSON metadata (or `null` if the document was deleted)
    /// - `"document"`: the content decoded as UTF-8 (invalid bytes replaced)
    /// - `"score"`: cosine similarity in `[0.0, 1.0]`
    pub fn search_document(&self, query_vec: Vec<f32>, limit: usize) -> Result<Vec<JsonValue>> {
        // Over-fetch so both :meta and :content slots compete fairly.
        let pool = limit.max(1) * 4;
        let candidates = self.vectors.search(query_vec, pool)?;
        self.build_results(candidates, limit)
    }

    /// Fingerprint `query` with [`json_fingerprint`], embed it, and return the
    /// `limit` most relevant documents.
    ///
    /// Returns `Err` if no embedding engine is present.
    ///
    /// [`json_fingerprint`]: crate::vectorengine::json_fingerprint
    pub fn search_document_json(
        &self,
        query: &JsonValue,
        limit: usize,
    ) -> Result<Vec<JsonValue>> {
        let pool = limit.max(1) * 4;
        let candidates = self.vectors.search_json(query, pool)?;
        self.build_results(candidates, limit)
    }

    /// Embed `query` as plain text and return the `limit` most relevant
    /// documents.
    ///
    /// Returns `Err` if no embedding engine is present.
    pub fn search_document_text(&self, query: &str, limit: usize) -> Result<Vec<JsonValue>> {
        self.search_document_json(&serde_json::json!(query), limit)
    }

    /// Like [`search_document`], but returns each result serialised to a
    /// [`json_fingerprint`] string instead of a raw `JsonValue`.
    ///
    /// Convenient for passing results directly to an embedding pipeline or
    /// full-text index without an extra mapping step.
    ///
    /// [`search_document`]: DocumentStorage::search_document
    /// [`json_fingerprint`]: crate::common::jsonfingerprint::json_fingerprint
    pub fn search_document_strings(
        &self,
        query_vec: Vec<f32>,
        limit: usize,
    ) -> Result<Vec<String>> {
        Ok(results_to_strings(&self.search_document(query_vec, limit)?))
    }

    /// Like [`search_document_json`], but returns fingerprint strings.
    ///
    /// Returns `Err` if no embedding engine is present.
    ///
    /// [`search_document_json`]: DocumentStorage::search_document_json
    pub fn search_document_json_strings(
        &self,
        query: &JsonValue,
        limit: usize,
    ) -> Result<Vec<String>> {
        Ok(results_to_strings(&self.search_document_json(query, limit)?))
    }

    /// Like [`search_document_text`], but returns fingerprint strings.
    ///
    /// Returns `Err` if no embedding engine is present.
    ///
    /// [`search_document_text`]: DocumentStorage::search_document_text
    pub fn search_document_text_strings(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        Ok(results_to_strings(&self.search_document_text(query, limit)?))
    }

    // ── persistence ───────────────────────────────────────────────────────────

    /// Flush the vector index to disk.
    ///
    /// The DuckDB-backed stores (`metadata.db`, `blobs.db`) checkpoint
    /// automatically; calling this is only necessary for the vecstore index.
    pub fn sync(&self) -> Result<()> {
        self.vectors.sync()
    }
}

// ── internals ─────────────────────────────────────────────────────────────────

impl DocumentStorage {
    fn build_results(
        &self,
        candidates: Vec<crate::vectorengine::SearchResult>,
        limit: usize,
    ) -> Result<Vec<JsonValue>> {
        // Deduplicate by UUID; keep the highest score per document.
        let mut best: HashMap<String, f32> = HashMap::new();
        for r in &candidates {
            let uuid_str = strip_suffix(&r.id).to_string();
            let entry = best.entry(uuid_str).or_insert(f32::NEG_INFINITY);
            if r.score > *entry {
                *entry = r.score;
            }
        }

        // Sort by descending score, then truncate to the requested limit.
        let mut ranked: Vec<(String, f32)> = best.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);

        // Load metadata + content for each UUID and assemble result objects.
        let mut out = Vec::with_capacity(ranked.len());
        for (uuid_str, score) in ranked {
            let uuid = Uuid::parse_str(&uuid_str)
                .map_err(|e| err_msg(format!("invalid UUID in vector index: {e}")))?;
            let metadata = self.meta.get_json(uuid)?.unwrap_or(JsonValue::Null);
            let content_bytes = self.blobs.get_blob(uuid)?.unwrap_or_default();
            let document = String::from_utf8_lossy(&content_bytes).into_owned();
            out.push(serde_json::json!({
                "id":       uuid_str,
                "metadata": metadata,
                "document": document,
                "score":    score,
            }));
        }
        Ok(out)
    }
}

/// Convert a slice of search-result JSON objects (as returned by
/// [`DocumentStorage::search_document`] and its variants) into a `Vec<String>`
/// by applying [`json_fingerprint`] to each element.
///
/// Useful for feeding results directly into an embedding pipeline or a
/// full-text index without an extra mapping step.
///
/// # Example
///
/// ```rust,no_run
/// # use bdslib::documentstorage::{DocumentStorage, results_to_strings};
/// # use tempfile::TempDir;
/// # use serde_json::json;
/// let dir = TempDir::new().unwrap();
/// let store = DocumentStorage::new(dir.path().to_str().unwrap()).unwrap();
/// let results = store.search_document(vec![1.0, 0.0, 0.0], 5).unwrap();
/// let strings = results_to_strings(&results);
/// ```
pub fn results_to_strings(results: &[JsonValue]) -> Vec<String> {
    results.iter().map(|r| json_fingerprint(r)).collect()
}

fn strip_suffix(id: &str) -> &str {
    id.strip_suffix(":meta")
        .or_else(|| id.strip_suffix(":content"))
        .unwrap_or(id)
}

fn meta_config() -> JsonStorageConfig {
    JsonStorageConfig {
        key_field:   None,
        default_key: "doc".to_string(),
    }
}

struct Paths {
    metadata_db: String,
    blobs_db:    String,
    vec:         String,
}

impl Paths {
    fn from(root: &str) -> Result<Self> {
        let root = Path::new(root);
        std::fs::create_dir_all(root)
            .map_err(|e| err_msg(format!("cannot create root dir {root:?}: {e}")))?;
        std::fs::create_dir_all(root.join("vectors"))
            .map_err(|e| err_msg(format!("cannot create vectors dir: {e}")))?;
        Ok(Self {
            metadata_db: root.join("metadata.db").to_string_lossy().into_owned(),
            blobs_db:    root.join("blobs.db").to_string_lossy().into_owned(),
            vec:         root.join("vectors").to_string_lossy().into_owned(),
        })
    }
}
