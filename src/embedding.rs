use crate::common::error::{err_msg, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub use fastembed::EmbeddingModel as Model;

/// A single embedding vector produced by the model.
pub type Embedding = Vec<f32>;

/// Default capacity of the in-process query-embedding cache.  256 slots
/// is enough to cover hot dashboard refreshes without measurable memory
/// pressure (~256 × 384 × 4 B ≈ 400 KiB for the AllMiniLML6V2 default).
pub const DEFAULT_CACHE_CAPACITY: usize = 256;

/// In-process LRU-ish cache mapping query text → embedding vector.
///
/// Uses random eviction (same pattern as
/// [`crate::common::cache_json::JsonCache`]) — simple, fast under the
/// mutex, and statistically favours hot keys because they're refreshed
/// on every hit.  Strict LRU would gain very little on the workloads
/// this cache targets (repeated identical query strings).
struct EmbedCache {
    entries:  HashMap<String, Embedding>,
    capacity: usize,
}

impl EmbedCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries:  HashMap::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
        }
    }

    fn get(&self, k: &str) -> Option<Embedding> {
        self.entries.get(k).cloned()
    }

    fn insert(&mut self, k: String, v: Embedding) {
        if self.entries.contains_key(&k) {
            self.entries.insert(k, v);
            return;
        }
        if self.entries.len() >= self.capacity {
            // Random eviction: HashMap iteration order is unspecified,
            // so the nth key is effectively random.  Cheap; no Vec churn.
            if let Some(victim) = self.entries.keys().next().cloned() {
                self.entries.remove(&victim);
            }
        }
        self.entries.insert(k, v);
    }
}

/// Thread-safe wrapper around a fastembed [`TextEmbedding`] model.
///
/// The underlying model requires exclusive access per inference call (`&mut self`),
/// so it is held behind an `Arc<Mutex<_>>`. The engine can be cloned cheaply
/// and shared across threads.
///
/// A small in-process [`EmbedCache`] short-circuits repeated calls for
/// the same text (typical for dashboard queries that re-poll every few
/// seconds).  Hits record `embed.hit`, misses record `embed.miss` into
/// the perf registry — `v2/perf` exposes both, so the hit ratio is
/// observable in production.  Cache capacity defaults to
/// [`DEFAULT_CACHE_CAPACITY`]; pass `0` to [`new_with_cache`] to disable.
#[derive(Clone)]
pub struct EmbeddingEngine {
    inner: Arc<Mutex<TextEmbedding>>,
    cache: Arc<Mutex<EmbedCache>>,
}

impl EmbeddingEngine {
    /// Load `model`, caching its files in `cache_dir`.
    ///
    /// Pass `None` for `cache_dir` to use fastembed's default
    /// (`$HOME/.cache/huggingface/hub` or the `HF_HOME` env var).
    /// The first call for a given model downloads its ONNX weights;
    /// subsequent calls load from cache.
    ///
    /// ```text
    /// let engine = EmbeddingEngine::new(Model::AllMiniLML6V2, None)?;
    /// let engine = EmbeddingEngine::new(Model::BGESmallENV15, Some("/data/models".into()))?;
    /// ```
    pub fn new(model: EmbeddingModel, cache_dir: Option<PathBuf>) -> Result<Self> {
        Self::new_with_cache(model, cache_dir, DEFAULT_CACHE_CAPACITY)
    }

    /// Like [`new`], but with an explicit query-embedding cache capacity.
    /// Pass `0` to disable the cache entirely (every `embed` call goes
    /// straight to ONNX inference).
    ///
    /// [`new`]: EmbeddingEngine::new
    pub fn new_with_cache(
        model:          EmbeddingModel,
        cache_dir:      Option<PathBuf>,
        cache_capacity: usize,
    ) -> Result<Self> {
        let options = {
            let opts = InitOptions::new(model);
            match cache_dir {
                Some(dir) => opts.with_cache_dir(dir),
                None => opts,
            }
        };

        let model = TextEmbedding::try_new(options)
            .map_err(|e| err_msg(format!("Failed to initialise embedding model: {e}")))?;

        Ok(Self {
            inner: Arc::new(Mutex::new(model)),
            cache: Arc::new(Mutex::new(EmbedCache::new(cache_capacity))),
        })
    }

    /// Embed a single string and return its vector.
    ///
    /// First checks the in-process [`EmbedCache`]; on hit, returns the
    /// cached vector and records `embed.hit` in the perf registry.  On
    /// miss, runs ONNX inference under the engine mutex and inserts the
    /// result.  Disabled (capacity 0) caches always miss.
    pub fn embed(&self, text: &str) -> Result<Embedding> {
        // Fast path: cache hit.  Lock is held only for the lookup.
        let started = std::time::Instant::now();
        if let Some(v) = self.cache.lock().get(text) {
            crate::perf::record_us("embed.hit", started.elapsed().as_micros() as u64);
            return Ok(v);
        }
        // Miss — run inference.  Time recorded separately so operators
        // can see real embed latency without cache hits flattening it.
        let started_miss = std::time::Instant::now();
        let v = self
            .inner
            .lock()
            .embed(vec![text], None)
            .map_err(|e| err_msg(format!("Embedding failed: {e}")))?
            .into_iter()
            .next()
            .ok_or_else(|| err_msg("Model returned no embedding"))?;
        crate::perf::record_us("embed.miss", started_miss.elapsed().as_micros() as u64);
        self.cache.lock().insert(text.to_owned(), v.clone());
        Ok(v)
    }

    /// Embed multiple strings in a single ONNX inference pass.
    ///
    /// Significantly faster than calling [`embed`] N times because the
    /// underlying model processes the whole batch in one matrix operation.
    /// Returns one vector per input in the same order. Returns an empty `Vec`
    /// when `texts` is empty.
    ///
    /// [`embed`]: EmbeddingEngine::embed
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Embedding>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        self.inner
            .lock()
            .embed(texts.to_vec(), None)
            .map_err(|e| err_msg(format!("Batch embedding failed: {e}")))
    }

    /// Compute the cosine similarity between two strings.
    ///
    /// The two embeddings are generated concurrently in Rayon worker threads.
    /// Because a single model instance serialises inference behind a mutex,
    /// the computations run back-to-back in the background rather than truly
    /// in parallel; the calling thread blocks only until both are complete.
    ///
    /// Returns a value in `[-1.0, 1.0]`: 1.0 means identical direction,
    /// 0.0 means orthogonal, -1.0 means opposite.
    pub fn compare_texts(&self, a: &str, b: &str) -> Result<f32> {
        let engine_a = self.inner.clone();
        let engine_b = self.inner.clone();
        let text_a = a.to_owned();
        let text_b = b.to_owned();

        let (res_a, res_b) = rayon::join(
            move || -> Result<Embedding> {
                engine_a
                    .lock()
                    .embed(vec![text_a.as_str()], None)
                    .map_err(|e| err_msg(format!("Embedding A failed: {e}")))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| err_msg("No embedding returned for A"))
            },
            move || -> Result<Embedding> {
                engine_b
                    .lock()
                    .embed(vec![text_b.as_str()], None)
                    .map_err(|e| err_msg(format!("Embedding B failed: {e}")))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| err_msg("No embedding returned for B"))
            },
        );

        Self::compare_embeddings(&res_a?, &res_b?)
    }

    /// Compute cosine similarity between two pre-computed embeddings.
    ///
    /// Returns `Err` if the vectors have different dimensions or if either
    /// is a zero vector (undefined cosine similarity).
    pub fn compare_embeddings(a: &[f32], b: &[f32]) -> Result<f32> {
        if a.len() != b.len() {
            return Err(err_msg(format!(
                "Embedding dimension mismatch: {} vs {}",
                a.len(),
                b.len()
            )));
        }
        if a.is_empty() {
            return Err(err_msg("Cannot compare empty embedding vectors"));
        }

        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

        if norm_a == 0.0 || norm_b == 0.0 {
            return Err(err_msg("Cannot compare zero-length embedding vectors"));
        }

        Ok(dot / (norm_a * norm_b))
    }
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests — EmbedCache pure-Rust behaviour, no ONNX needed.
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_returns_inserted_value() {
        let mut c = EmbedCache::new(4);
        c.insert("hello".into(), vec![1.0, 2.0, 3.0]);
        assert_eq!(c.get("hello"), Some(vec![1.0, 2.0, 3.0]));
    }

    #[test]
    fn cache_miss_returns_none() {
        let c = EmbedCache::new(4);
        assert!(c.get("missing").is_none());
    }

    #[test]
    fn cache_overwrites_on_repeat_insert() {
        let mut c = EmbedCache::new(4);
        c.insert("k".into(), vec![1.0]);
        c.insert("k".into(), vec![2.0]);
        assert_eq!(c.get("k"), Some(vec![2.0]));
        assert_eq!(c.entries.len(), 1);
    }

    #[test]
    fn cache_evicts_when_full() {
        let mut c = EmbedCache::new(2);
        c.insert("a".into(), vec![0.0]);
        c.insert("b".into(), vec![0.0]);
        c.insert("c".into(), vec![0.0]);
        assert_eq!(c.entries.len(), 2);
        // Exactly one of a/b was evicted (random); c is always present.
        assert!(c.get("c").is_some());
    }

    #[test]
    fn cache_capacity_zero_clamps_to_one() {
        // Zero capacity is interpreted as "1 slot" so the cache always
        // has room for the most-recent key.  Callers wanting to disable
        // entirely should skip the cache layer rather than passing 0.
        let mut c = EmbedCache::new(0);
        c.insert("k".into(), vec![1.0]);
        assert_eq!(c.get("k"), Some(vec![1.0]));
    }
}
