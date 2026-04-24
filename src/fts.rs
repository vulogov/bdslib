use crate::common::error::{err_msg, Result};
use parking_lot::Mutex;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, OwnedValue, Schema, STORED, STRING, TEXT};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};
use uuid::Uuid;

const WRITER_HEAP_BYTES: usize = 50_000_000;

/// Thread-safe full-text search engine backed by Tantivy.
///
/// Every document is assigned a UUIDv7 at insertion time. All three
/// operations (`add_document`, `drop_document`, `search`) are immediately
/// consistent: a commit + reader reload is issued after every write.
pub struct FTSEngine {
    index: Index,
    writer: Mutex<IndexWriter<TantivyDocument>>,
    reader: IndexReader,
    id_field: Field,
    body_field: Field,
}

impl FTSEngine {
    /// Create or open an FTS index.
    ///
    /// Pass `":memory:"` for a RAM-only index (lost on drop).
    /// Any other value is treated as a filesystem directory path;
    /// it is created if it does not exist.
    pub fn new(path: &str) -> Result<Self> {
        let mut builder = Schema::builder();
        let id_field = builder.add_text_field("id", STRING | STORED);
        let body_field = builder.add_text_field("body", TEXT | STORED);
        let schema = builder.build();

        let index = if path == ":memory:" {
            Index::create_in_ram(schema)
        } else {
            let dir = Path::new(path);
            std::fs::create_dir_all(dir)
                .map_err(|e| err_msg(format!("Cannot create index directory: {e}")))?;
            // Open the existing index if one is present; create otherwise.
            if dir.join("meta.json").exists() {
                Index::open_in_dir(dir)
                    .map_err(|e| err_msg(format!("Cannot open index at {path}: {e}")))?
            } else {
                Index::create_in_dir(dir, schema)
                    .map_err(|e| err_msg(format!("Cannot create index at {path}: {e}")))?
            }
        };

        let writer: IndexWriter<TantivyDocument> = index
            .writer(WRITER_HEAP_BYTES)
            .map_err(|e| err_msg(format!("Cannot create index writer: {e}")))?;

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| err_msg(format!("Cannot create index reader: {e}")))?;

        Ok(Self {
            index,
            writer: Mutex::new(writer),
            reader,
            id_field,
            body_field,
        })
    }

    /// Index `text` and return its assigned UUIDv7.
    pub fn add_document(&self, text: &str) -> Result<Uuid> {
        let id = Uuid::now_v7();
        let mut doc = TantivyDocument::default();
        doc.add_text(self.id_field, id.to_string());
        doc.add_text(self.body_field, text);

        {
            let mut writer = self.writer.lock();
            writer
                .add_document(doc)
                .map_err(|e| err_msg(format!("Failed to stage document: {e}")))?;
            writer
                .commit()
                .map_err(|e| err_msg(format!("Failed to commit add: {e}")))?;
        }

        self.reader
            .reload()
            .map_err(|e| err_msg(format!("Failed to reload reader after add: {e}")))?;

        Ok(id)
    }

    /// Remove the document with the given UUIDv7 from the index.
    ///
    /// Succeeds silently if the UUID does not exist.
    pub fn drop_document(&self, id: Uuid) -> Result<()> {
        let term = Term::from_field_text(self.id_field, &id.to_string());

        {
            let mut writer = self.writer.lock();
            writer.delete_term(term);
            writer
                .commit()
                .map_err(|e| err_msg(format!("Failed to commit delete: {e}")))?;
        }

        self.reader
            .reload()
            .map_err(|e| err_msg(format!("Failed to reload reader after delete: {e}")))?;

        Ok(())
    }

    /// Flush all pending changes to the on-disk index directory and reload the reader.
    ///
    /// For in-memory indexes this is a no-op in terms of persistence but still safe to call.
    /// Mirrors the `StorageEngine::sync` / DuckDB CHECKPOINT pattern.
    pub fn sync(&self) -> Result<()> {
        self.writer
            .lock()
            .commit()
            .map_err(|e| err_msg(format!("Sync commit failed: {e}")))?;
        self.reader
            .reload()
            .map_err(|e| err_msg(format!("Failed to reload reader after sync: {e}")))?;
        Ok(())
    }

    /// Index `text` under a caller-supplied `id`, replacing any existing entry for that id.
    ///
    /// Unlike [`add_document`], which generates a fresh UUIDv7, this method stores the
    /// document under the UUID you provide. If a document with the same `id` is already
    /// in the index it is deleted and re-inserted atomically within a single commit.
    ///
    /// [`add_document`]: FTSEngine::add_document
    pub fn add_document_with_id(&self, id: Uuid, text: &str) -> Result<()> {
        let term = Term::from_field_text(self.id_field, &id.to_string());
        let mut doc = TantivyDocument::default();
        doc.add_text(self.id_field, id.to_string());
        doc.add_text(self.body_field, text);

        {
            let mut writer = self.writer.lock();
            writer.delete_term(term);
            writer
                .add_document(doc)
                .map_err(|e| err_msg(format!("Failed to stage document {id}: {e}")))?;
            writer
                .commit()
                .map_err(|e| err_msg(format!("Failed to commit add_document_with_id: {e}")))?;
        }

        self.reader
            .reload()
            .map_err(|e| err_msg(format!("Failed to reload reader after add_document_with_id: {e}")))?;

        Ok(())
    }

    /// Search the index and return up to `limit` matching UUIDv7s, ranked by relevance.
    ///
    /// `query` uses Tantivy's query syntax (e.g. `"hello world"`, `hello AND world`).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Uuid>> {
        Ok(self
            .search_with_scores(query, limit)?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }

    /// Search the index and return up to `limit` `(UUID, BM25-score)` pairs,
    /// ordered by descending relevance score.
    pub fn search_with_scores(&self, query: &str, limit: usize) -> Result<Vec<(Uuid, f32)>> {
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.body_field]);
        let parsed = parser
            .parse_query(query)
            .map_err(|e| err_msg(format!("Invalid query \"{query}\": {e}")))?;

        let hits = searcher
            .search(&parsed, &TopDocs::with_limit(limit))
            .map_err(|e| err_msg(format!("Search failed: {e}")))?;

        let mut results = Vec::with_capacity(hits.len());
        for (score, addr) in hits {
            let doc: TantivyDocument = searcher
                .doc(addr)
                .map_err(|e| err_msg(format!("Failed to retrieve document: {e}")))?;

            if let Some(raw) = doc.get_first(self.id_field) {
                if let OwnedValue::Str(id_str) = OwnedValue::from(raw) {
                    if let Ok(uuid) = Uuid::parse_str(&id_str) {
                        results.push((uuid, score));
                    }
                }
            }
        }

        Ok(results)
    }
}
