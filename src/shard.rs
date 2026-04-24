use crate::common::error::{err_msg, Result};
use crate::fts::FTSEngine;
use crate::observability::{ObservabilityStorage, ObservabilityStorageConfig};
use crate::vectorengine::{json_fingerprint, VectorEngine};
use crate::EmbeddingEngine;
use serde_json::{json, Value as JsonValue};
use std::sync::Arc;
use uuid::Uuid;
use vecstore::reranking::MMRReranker;

/// Combined observability, full-text search, and vector shard.
///
/// `Shard` stores every telemetry event in [`ObservabilityStorage`] and
/// maintains two search indexes that cover **primary records only**:
///
/// | Index | Engine | Scope |
/// |---|---|---|
/// | Full-text | [`FTSEngine`] | Primary records only |
/// | Vector | [`VectorEngine`] | Primary records only |
///
/// Secondary records are stored in `ObservabilityStorage` but are **not**
/// added to the FTS or vector indexes. They are returned as embedded
/// `"secondaries"` arrays inside their parent primary in search results.
///
/// All three indexes share the same UUID namespace: the UUIDv7 returned by
/// [`add`] is the identifier used in all stores.
///
/// `Shard` is `Clone`; all clones share the same underlying connection pool,
/// FTS writer, and vector index.
///
/// [`add`]: Shard::add
#[derive(Clone)]
pub struct Shard {
    observability: ObservabilityStorage,
    fts: Arc<FTSEngine>,
    vector: VectorEngine,
}

impl Shard {
    /// Open or create a shard rooted at `path` with default config.
    ///
    /// Three sub-paths are created automatically:
    ///
    /// | Sub-path | Engine |
    /// |---|---|
    /// | `{path}/obs.db` | ObservabilityStorage (DuckDB) |
    /// | `{path}/fts/` | FTSEngine (Tantivy) |
    /// | `{path}/vec/` | VectorEngine (HNSW) |
    ///
    /// `pool_size` is forwarded to `ObservabilityStorage`.
    pub fn new(path: &str, pool_size: u32, embedding: EmbeddingEngine) -> Result<Self> {
        Self::with_config(path, pool_size, embedding, ObservabilityStorageConfig::default())
    }

    /// Open or create a shard at `path` with a custom `ObservabilityStorageConfig`.
    pub fn with_config(
        path: &str,
        pool_size: u32,
        embedding: EmbeddingEngine,
        config: ObservabilityStorageConfig,
    ) -> Result<Self> {
        std::fs::create_dir_all(path)
            .map_err(|e| err_msg(format!("cannot create shard directory '{path}': {e}")))?;

        let obs_path = format!("{path}/obs.db");
        let fts_path = format!("{path}/fts");
        let vec_path = format!("{path}/vec");

        let observability =
            ObservabilityStorage::with_config(&obs_path, pool_size, embedding.clone(), config)?;
        let fts = FTSEngine::new(&fts_path)?;
        let vector = VectorEngine::with_embedding(&vec_path, embedding)?;

        Ok(Self {
            observability,
            fts: Arc::new(fts),
            vector,
        })
    }

    // ── writes ────────────────────────────────────────────────────────────────

    /// Store a telemetry event and, if the record is classified as primary,
    /// index it in the FTS and vector engines.
    ///
    /// The document must satisfy `ObservabilityStorage::add` requirements
    /// (`timestamp`, `key`, `data` mandatory fields). The JSON fingerprint of
    /// the full document is used as the FTS body and as the embedding input for
    /// the vector index.
    ///
    /// Secondary records are stored in `ObservabilityStorage` only; they are
    /// not added to the FTS or vector indexes. They are accessible via their
    /// parent primary through [`search_fts`] and [`search_vector`] results.
    ///
    /// Duplicate `(key, data)` pairs return the existing record's UUID and
    /// update the deduplication log without touching the search indexes.
    ///
    /// [`search_fts`]: Shard::search_fts
    /// [`search_vector`]: Shard::search_vector
    pub fn add(&self, doc: JsonValue) -> Result<Uuid> {
        let fingerprint = json_fingerprint(&doc);
        let (id, is_primary, opt_emb) = self.observability.add(doc.clone())?;
        if is_primary {
            self.fts.add_document_with_id(id, &fingerprint)?;
            // Reuse the embedding already computed by observability — no second embed.
            self.vector
                .store_vector(&id.to_string(), opt_emb.unwrap(), Some(doc))?;
        }
        Ok(id)
    }

    /// Store a batch of telemetry events with a single embedding pass, a single
    /// FTS commit, and a single DuckDB write transaction for all primaries.
    ///
    /// Documents classified as duplicates or secondaries are written to
    /// `ObservabilityStorage` but are not staged for FTS/vector indexing.
    /// Returns UUIDs in the same order as the input documents.
    pub fn add_batch(&self, docs: Vec<JsonValue>) -> Result<Vec<Uuid>> {
        let fingerprints: Vec<String> = docs.iter().map(|d| json_fingerprint(d)).collect();

        // observability.add_batch handles batch embedding + single transaction.
        let results = self.observability.add_batch(&docs)?;

        let mut ids = Vec::with_capacity(results.len());
        let mut fts_batch: Vec<(Uuid, String)> = Vec::new();

        for (i, (id, is_primary, opt_emb)) in results.into_iter().enumerate() {
            ids.push(id);
            if is_primary {
                fts_batch.push((id, fingerprints[i].clone()));
                // Reuse embedding from observability — no re-embed.
                self.vector
                    .store_vector(&id.to_string(), opt_emb.unwrap(), Some(docs[i].clone()))?;
            }
        }

        self.fts.add_documents_batch(&fts_batch)?;
        Ok(ids)
    }

    /// Delete a record from `ObservabilityStorage` and, if it was a primary,
    /// also remove it from the FTS and vector indexes.
    ///
    /// Deleting a primary leaves its linked secondaries in `ObservabilityStorage`
    /// as unlinked records (they are not automatically promoted or removed).
    /// Returns `Ok(())` for unknown IDs.
    pub fn delete(&self, id: Uuid) -> Result<()> {
        let was_primary = self.observability.is_primary(id)?;
        self.observability.delete_by_id(id)?;
        if was_primary {
            self.fts.drop_document(id)?;
            self.vector.delete_vector(&id.to_string())?;
        }
        Ok(())
    }

    // ── reads ─────────────────────────────────────────────────────────────────

    /// Return the full JSON record for `id`, or `None` if not found.
    pub fn get(&self, id: Uuid) -> Result<Option<JsonValue>> {
        self.observability.get_by_id(id)
    }

    /// Return all records whose `key` matches, ordered by timestamp ascending.
    pub fn get_by_key(&self, key: &str) -> Result<Vec<JsonValue>> {
        self.observability.get_by_key(key)
    }

    /// Flush all three engines to disk.
    ///
    /// Calls `ObservabilityStorage::sync` (DuckDB CHECKPOINT),
    /// `FTSEngine::sync` (Tantivy commit + reload), and
    /// `VectorEngine::sync` (HNSW save) in that order.
    pub fn sync(&self) -> Result<()> {
        self.observability.sync()?;
        self.fts.sync()?;
        self.vector.sync()?;
        Ok(())
    }

    // ── passthrough accessors ─────────────────────────────────────────────────

    /// Borrow the underlying `ObservabilityStorage` for direct access to
    /// deduplication, primary/secondary, and time-range APIs.
    pub fn observability(&self) -> &ObservabilityStorage {
        &self.observability
    }

    // ── search ────────────────────────────────────────────────────────────────

    /// Full-text search over the JSON fingerprints of primary records.
    ///
    /// `query` uses Tantivy query syntax (e.g. `cpu AND usage`, `"disk full"`).
    /// Results are returned in Tantivy relevance order.
    ///
    /// Each returned document is the full JSON of the matching primary with a
    /// `"secondaries"` field containing the full JSON of every secondary linked
    /// to that primary.
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<JsonValue>> {
        let ids = self.fts.search(query, limit)?;
        let mut docs = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(doc) = self.observability.get_by_id(id)? {
                docs.push(self.attach_secondaries(id, doc)?);
            }
        }
        Ok(docs)
    }

    /// Full-text search returning `(primary_id, BM25_score)` pairs only — no
    /// document body is fetched. Use this when you only need IDs and relevance
    /// scores without the overhead of retrieving and deserialising full records.
    pub fn search_fts_scored(&self, query: &str, limit: usize) -> Result<Vec<(Uuid, f32)>> {
        self.fts.search_with_scores(query, limit)
    }

    /// Full-text search returning `(primary_id, unix_ts, BM25_score)` triples.
    ///
    /// The timestamp is fetched from `ObservabilityStorage` for each hit via a
    /// single indexed PK lookup. Records whose IDs have been deleted between
    /// the FTS search and the timestamp lookup are silently skipped.
    pub fn search_fts_with_ts(&self, query: &str, limit: usize) -> Result<Vec<(Uuid, i64, f32)>> {
        let hits = self.fts.search_with_scores(query, limit)?;
        let mut results = Vec::with_capacity(hits.len());
        for (id, score) in hits {
            if let Some(ts) = self.observability.get_ts_by_id(id)? {
                results.push((id, ts, score));
            }
        }
        Ok(results)
    }

    /// Semantic vector search with MMR reranking over primary records.
    ///
    /// `query` is fingerprinted, embedded, and used to search the HNSW index.
    /// A candidate pool of `max(limit * 2, 10)` nearest neighbours is fetched
    /// and reranked with `MMRReranker(λ = 0.7)` before the top `limit` results
    /// are selected.
    ///
    /// Each returned document is the full JSON of the matching primary with a
    /// `"_score"` field (cosine similarity, higher = more similar) and a
    /// `"secondaries"` field containing the full JSON of every linked secondary.
    /// Results are ordered by descending score.
    pub fn search_vector(&self, query: &JsonValue, limit: usize) -> Result<Vec<JsonValue>> {
        let candidate_pool = (limit * 2).max(10);
        let reranker = MMRReranker::new(0.7);
        let neighbors =
            self.vector
                .search_json_reranked(query, limit, candidate_pool, &reranker)?;

        let mut docs = Vec::with_capacity(neighbors.len());
        for neighbor in neighbors {
            let id = Uuid::parse_str(&neighbor.id).map_err(|e| {
                err_msg(format!(
                    "vector index contains invalid UUID '{}': {e}",
                    neighbor.id
                ))
            })?;
            if let Some(mut doc) = self.observability.get_by_id(id)? {
                if let JsonValue::Object(ref mut map) = doc {
                    map.insert("_score".to_string(), json!(neighbor.score));
                }
                docs.push(self.attach_secondaries(id, doc)?);
            }
        }
        Ok(docs)
    }

    // ── internal ──────────────────────────────────────────────────────────────

    fn attach_secondaries(&self, primary_id: Uuid, mut doc: JsonValue) -> Result<JsonValue> {
        let secondary_ids = self.observability.list_secondaries(primary_id)?;
        let mut secondaries = Vec::with_capacity(secondary_ids.len());
        for sid in secondary_ids {
            if let Some(sdoc) = self.observability.get_by_id(sid)? {
                secondaries.push(sdoc);
            }
        }
        if let JsonValue::Object(ref mut map) = doc {
            map.insert("secondaries".to_string(), json!(secondaries));
        }
        Ok(doc)
    }
}
