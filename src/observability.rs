use crate::common::error::{err_msg, Result};
use crate::common::hex::to_hex;
use crate::common::jsonfingerprint::json_fingerprint;
use crate::common::math::cosine_similarity;
use crate::common::sql::sql_escape;
use crate::common::timerange::to_unix_secs;
use crate::common::uuid::generate_v7;
use crate::EmbeddingEngine;
use crate::StorageEngine;
use rust_dynamic::value::Value as DynamicValue;
use serde_json::{json, Value as JsonValue};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

// ── schema ────────────────────────────────────────────────────────────────────

const INIT_SQL: &str = "
    CREATE TABLE IF NOT EXISTS telemetry (
        id         TEXT    NOT NULL PRIMARY KEY,
        ts         BIGINT  NOT NULL,
        key        TEXT    NOT NULL,
        data       JSON    NOT NULL,
        metadata   JSON    NOT NULL,
        data_text  TEXT    NOT NULL,
        is_primary INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_tel_key     ON telemetry (key);
    CREATE INDEX IF NOT EXISTS idx_tel_ts      ON telemetry (ts);
    CREATE INDEX IF NOT EXISTS idx_tel_primary ON telemetry (is_primary);

    CREATE TABLE IF NOT EXISTS dedup_tracking (
        key       TEXT NOT NULL,
        data_text TEXT NOT NULL,
        timestamps JSON NOT NULL,
        PRIMARY KEY (key, data_text)
    );
    CREATE INDEX IF NOT EXISTS idx_dedup_key ON dedup_tracking (key);

    CREATE TABLE IF NOT EXISTS primary_secondary (
        primary_id   TEXT   NOT NULL,
        secondary_id TEXT   NOT NULL,
        ts           BIGINT NOT NULL,
        PRIMARY KEY (primary_id, secondary_id)
    );
    CREATE INDEX IF NOT EXISTS idx_ps_primary ON primary_secondary (primary_id);
    CREATE INDEX IF NOT EXISTS idx_ps_ts      ON primary_secondary (ts);

    CREATE TABLE IF NOT EXISTS primary_embeddings (
        primary_id TEXT NOT NULL PRIMARY KEY,
        embedding  BLOB NOT NULL
    );
";

// ── config ────────────────────────────────────────────────────────────────────

/// Configuration for [`ObservabilityStorage`].
#[derive(Debug, Clone)]
pub struct ObservabilityStorageConfig {
    /// Cosine similarity threshold for primary/secondary classification.
    ///
    /// When the similarity between a new record's embedding and the nearest
    /// existing primary is `>= similarity_threshold`, the record is stored as
    /// a secondary and linked to that primary. Otherwise it becomes a new
    /// primary.
    ///
    /// Range: `[0.0, 1.0]`. Default: `0.85`.
    pub similarity_threshold: f32,
}

impl Default for ObservabilityStorageConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.85,
        }
    }
}

// ── storage ───────────────────────────────────────────────────────────────────

/// Thread-safe observability store for telemetry events and time-series data.
///
/// Every record is a JSON document with three mandatory fields:
///
/// | Field | Type | Description |
/// |---|---|---|
/// | `timestamp` | integer (Unix secs) or numeric string | Event time |
/// | `key` | string | Signal identifier / metric name |
/// | `data` | any scalar or JSON | Measured value |
///
/// All other fields in the submitted document are preserved as opaque metadata.
/// If the submitted document has no `id` field a UUIDv7 is generated.
///
/// On each [`add`] call the storage:
///
/// 1. **Deduplicates** — if the same `key` + `data` combination was already
///    stored, the duplicate's timestamp is appended to the deduplication log
///    and the existing UUID is returned without storing the record again.
/// 2. **Classifies** via embedding similarity — if no duplicate is found the
///    `data` field is embedded with the attached [`EmbeddingEngine`] and
///    compared against all existing primary embeddings. Records whose
///    similarity to the nearest primary exceeds
///    [`similarity_threshold`][ObservabilityStorageConfig::similarity_threshold]
///    are stored as secondaries and linked to that primary; otherwise they
///    become new primaries.
///
/// `ObservabilityStorage` is `Clone`; all clones share the same underlying pool
/// and model.
///
/// [`add`]: ObservabilityStorage::add
#[derive(Clone)]
pub struct ObservabilityStorage {
    engine: Arc<StorageEngine>,
    embedding: Arc<EmbeddingEngine>,
    config: Arc<ObservabilityStorageConfig>,
}

impl ObservabilityStorage {
    /// Open or create an observability store at `path` with default config.
    ///
    /// All required tables are created automatically. Pass `":memory:"` for an
    /// ephemeral in-process store.
    pub fn new(path: &str, pool_size: u32, embedding: EmbeddingEngine) -> Result<Self> {
        Self::with_config(path, pool_size, embedding, ObservabilityStorageConfig::default())
    }

    /// Open or create an observability store at `path` with a custom config.
    pub fn with_config(
        path: &str,
        pool_size: u32,
        embedding: EmbeddingEngine,
        config: ObservabilityStorageConfig,
    ) -> Result<Self> {
        let engine = StorageEngine::new(path, INIT_SQL, pool_size)?;
        Ok(Self {
            engine: Arc::new(engine),
            embedding: Arc::new(embedding),
            config: Arc::new(config),
        })
    }

    // ── writes ────────────────────────────────────────────────────────────────

    /// Store a telemetry record and return its UUID.
    ///
    /// ## Mandatory fields
    ///
    /// | Field | Accepted types |
    /// |---|---|
    /// | `timestamp` | integer or numeric string (Unix seconds) |
    /// | `key` | string |
    /// | `data` | any JSON value |
    ///
    /// ## Behaviour
    ///
    /// - If `id` is absent a UUIDv7 is generated automatically.
    /// - If `key` + `data` already exists in the store (exact match) the
    ///   duplicate's `timestamp` is appended to the deduplication log and the
    ///   existing record's UUID is returned — the record is **not** stored
    ///   again.
    /// - Otherwise the record is embedded, classified as primary or secondary,
    ///   and inserted.
    pub fn add(&self, doc: JsonValue) -> Result<Uuid> {
        // ── validate and extract mandatory fields ─────────────────────────────
        let ts_val = doc
            .get("timestamp")
            .ok_or_else(|| err_msg("missing mandatory field 'timestamp'"))?;
        let ts = parse_timestamp(ts_val)?;

        let key = doc
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| err_msg("missing or non-string mandatory field 'key'"))?
            .to_string();

        let data = doc
            .get("data")
            .ok_or_else(|| err_msg("missing mandatory field 'data'"))?
            .clone();

        // ── extract or generate id ────────────────────────────────────────────
        let id = if let Some(s) = doc.get("id").and_then(|v| v.as_str()) {
            Uuid::parse_str(s).map_err(|e| err_msg(format!("invalid 'id' field: {e}")))?
        } else {
            generate_v7()
        };

        let data_text = data_to_text(&data);
        let metadata = build_metadata(&doc);

        // ── deduplication: same key + same data already stored? ───────────────
        let existing = self.engine.select_all(&format!(
            "SELECT id FROM telemetry WHERE key = '{}' AND data_text = '{}'",
            sql_escape(&key),
            sql_escape(&data_text),
        ))?;

        if let Some(row) = existing.into_iter().next() {
            let existing_id = parse_uuid_field(row, 0, "telemetry.id")?;
            self.record_duplicate(&key, &data_text, ts)?;
            return Ok(existing_id);
        }

        // ── embed data and classify primary / secondary ────────────────────────
        let embed_input = format!("key: {key} {data_text}");
        let embedding = self.embedding.embed(&embed_input)?;
        let (is_primary, similar_primary) = self.classify(&embedding)?;

        // ── store telemetry record ─────────────────────────────────────────────
        let data_s = serde_json::to_string(&data)
            .map_err(|e| err_msg(format!("data serialisation failed: {e}")))?;
        let meta_s = serde_json::to_string(&metadata)
            .map_err(|e| err_msg(format!("metadata serialisation failed: {e}")))?;

        self.engine.execute(&format!(
            "INSERT INTO telemetry (id, ts, key, data, metadata, data_text, is_primary) \
             VALUES ('{id}', {ts}, '{}', '{}'::JSON, '{}'::JSON, '{}', {})",
            sql_escape(&key),
            sql_escape(&data_s),
            sql_escape(&meta_s),
            sql_escape(&data_text),
            if is_primary { 1 } else { 0 },
        ))?;

        if is_primary {
            let hex = to_hex(&embedding_to_bytes(&embedding));
            self.engine.execute(&format!(
                "INSERT INTO primary_embeddings VALUES ('{id}', from_hex('{hex}'))"
            ))?;
        } else {
            let primary_id = similar_primary.unwrap();
            self.engine.execute(&format!(
                "INSERT INTO primary_secondary VALUES ('{primary_id}', '{id}', {ts})"
            ))?;
        }

        Ok(id)
    }

    /// Delete the record with `id` and all associated tracking rows.
    ///
    /// Deleting a primary also removes its embedding and all primary→secondary
    /// links (secondary records themselves remain in the telemetry table as
    /// unlinked entries). Returns `Ok(())` for unknown IDs.
    pub fn delete_by_id(&self, id: Uuid) -> Result<()> {
        // Fetch key + data_text before deleting the row so we can clean dedup_tracking.
        let rows = self.engine.select_all(&format!(
            "SELECT key, data_text FROM telemetry WHERE id = '{id}'"
        ))?;

        self.engine.execute(&format!(
            "DELETE FROM primary_secondary WHERE primary_id = '{id}' OR secondary_id = '{id}'"
        ))?;
        self.engine.execute(&format!(
            "DELETE FROM primary_embeddings WHERE primary_id = '{id}'"
        ))?;
        self.engine.execute(&format!(
            "DELETE FROM telemetry WHERE id = '{id}'"
        ))?;

        if let Some(row) = rows.into_iter().next() {
            let mut it = row.into_iter();
            let cast_err = |e: Box<dyn std::error::Error>| err_msg(e.to_string());
            let key = it
                .next()
                .ok_or_else(|| err_msg("telemetry row missing key"))?
                .cast_string()
                .map_err(cast_err)?;
            let data_text = it
                .next()
                .ok_or_else(|| err_msg("telemetry row missing data_text"))?
                .cast_string()
                .map_err(cast_err)?;
            self.engine.execute(&format!(
                "DELETE FROM dedup_tracking WHERE key = '{}' AND data_text = '{}'",
                sql_escape(&key),
                sql_escape(&data_text),
            ))?;
        }

        Ok(())
    }

    /// Delete all records with `key` and clear their deduplication log.
    ///
    /// Returns `Ok(())` even if no records exist for `key`.
    pub fn delete_by_key(&self, key: &str) -> Result<()> {
        let rows = self.engine.select_all(&format!(
            "SELECT id FROM telemetry WHERE key = '{}'",
            sql_escape(key)
        ))?;
        for row in rows {
            let id = parse_uuid_field(row, 0, "telemetry.id")?;
            self.delete_by_id(id)?;
        }
        self.engine.execute(&format!(
            "DELETE FROM dedup_tracking WHERE key = '{}'",
            sql_escape(key)
        ))
    }

    // ── reads ─────────────────────────────────────────────────────────────────

    /// Return the record for `id`, or `None` if not found.
    ///
    /// The returned document includes all original fields plus `id`.
    pub fn get_by_id(&self, id: Uuid) -> Result<Option<JsonValue>> {
        let rows = self.engine.select_all(&format!(
            "SELECT id, ts, key, data, metadata FROM telemetry WHERE id = '{id}'"
        ))?;
        rows.into_iter().next().map(row_to_doc).transpose()
    }

    /// Return all records whose `key` matches, ordered by `timestamp` ascending.
    pub fn get_by_key(&self, key: &str) -> Result<Vec<JsonValue>> {
        let rows = self.engine.select_all(&format!(
            "SELECT id, ts, key, data, metadata \
             FROM telemetry WHERE key = '{}' ORDER BY ts ASC",
            sql_escape(key)
        ))?;
        rows.into_iter().map(row_to_doc).collect()
    }

    /// Return the UUIDs of all records whose event timestamp falls in
    /// the half-open interval `[start, end)`, ordered by timestamp ascending.
    pub fn list_ids_by_time_range(
        &self,
        start: SystemTime,
        end: SystemTime,
    ) -> Result<Vec<Uuid>> {
        let s = to_unix_secs(start)?;
        let e = to_unix_secs(end)?;
        let rows = self.engine.select_all(&format!(
            "SELECT id FROM telemetry WHERE ts >= {s} AND ts < {e} ORDER BY ts ASC"
        ))?;
        parse_uuid_column(rows)
    }

    /// Return `true` if the record for `id` is classified as primary.
    ///
    /// Returns `false` for unknown IDs and for secondary records.
    pub fn is_primary(&self, id: Uuid) -> Result<bool> {
        let rows = self.engine.select_all(&format!(
            "SELECT is_primary FROM telemetry WHERE id = '{id}'"
        ))?;
        match rows.into_iter().next() {
            None => Ok(false),
            Some(row) => {
                let val = row
                    .into_iter()
                    .next()
                    .ok_or_else(|| err_msg("telemetry row missing is_primary"))?
                    .cast_int()
                    .map_err(|e| err_msg(e.to_string()))?;
                Ok(val != 0)
            }
        }
    }

    // ── deduplication ─────────────────────────────────────────────────────────

    /// Return the timestamps at which duplicate submissions were detected for
    /// `key`, across all data values under that key.
    ///
    /// A duplicate is any `add` call where `(key, data)` matched an existing
    /// record; the event `timestamp` from that call is recorded here instead
    /// of being stored in the telemetry table.
    pub fn get_duplicate_timestamps(&self, key: &str) -> Result<Vec<SystemTime>> {
        let rows = self.engine.select_all(&format!(
            "SELECT timestamps FROM dedup_tracking WHERE key = '{}'",
            sql_escape(key)
        ))?;
        self.parse_timestamps_rows(rows)
    }

    /// Return the deduplication timestamps for the exact-match entry that owns
    /// the same `(key, data_text)` as the primary record identified by `id`.
    ///
    /// Returns an empty `Vec` when no exact-match duplicates have been seen for
    /// that record, or when `id` does not exist in this shard.
    pub fn get_duplicate_timestamps_by_id(&self, id: Uuid) -> Result<Vec<SystemTime>> {
        let rows = self.engine.select_all(&format!(
            "SELECT d.timestamps \
             FROM dedup_tracking d \
             JOIN telemetry t ON t.key = d.key AND t.data_text = d.data_text \
             WHERE t.id = '{id}'"
        ))?;
        self.parse_timestamps_rows(rows)
    }

    /// Return all deduplication entries in this shard as
    /// `(primary_uuid, key, timestamps)` triples, ordered by the primary
    /// record's event timestamp ascending.
    ///
    /// Only primaries that have at least one exact-match duplicate appear here.
    pub fn list_all_dedup_entries(&self) -> Result<Vec<(Uuid, String, Vec<SystemTime>)>> {
        let rows = self.engine.select_all(
            "SELECT t.id, t.key, d.timestamps \
             FROM dedup_tracking d \
             JOIN telemetry t ON t.key = d.key AND t.data_text = d.data_text \
             WHERE t.is_primary = 1 \
             ORDER BY t.ts ASC",
        )?;
        let mut out = Vec::new();
        for row in rows {
            let mut it = row.into_iter();
            let id_str = it
                .next()
                .ok_or_else(|| err_msg("dedup row missing id"))?
                .cast_string()
                .map_err(|e| err_msg(e.to_string()))?;
            let id = Uuid::parse_str(&id_str)
                .map_err(|e| err_msg(format!("dedup row bad uuid: {e}")))?;
            let key = it
                .next()
                .ok_or_else(|| err_msg("dedup row missing key"))?
                .cast_string()
                .map_err(|e| err_msg(e.to_string()))?;
            let ts_json = it
                .next()
                .ok_or_else(|| err_msg("dedup row missing timestamps"))?
                .cast_string()
                .map_err(|e| err_msg(e.to_string()))?;
            let timestamps: Vec<i64> = serde_json::from_str(&ts_json)
                .map_err(|e| err_msg(format!("timestamps JSON parse failed: {e}")))?;
            let times: Vec<SystemTime> = timestamps
                .into_iter()
                .map(|ts| UNIX_EPOCH + Duration::from_secs(ts as u64))
                .collect();
            out.push((id, key, times));
        }
        Ok(out)
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn parse_timestamps_rows(&self, rows: Vec<Vec<DynamicValue>>) -> Result<Vec<SystemTime>> {
        let mut out = Vec::new();
        for row in rows {
            let ts_json = row
                .into_iter()
                .next()
                .ok_or_else(|| err_msg("dedup_tracking row missing timestamps"))?
                .cast_string()
                .map_err(|e| err_msg(e.to_string()))?;
            let timestamps: Vec<i64> = serde_json::from_str(&ts_json)
                .map_err(|e| err_msg(format!("timestamps JSON parse failed: {e}")))?;
            out.extend(
                timestamps
                    .into_iter()
                    .map(|ts| UNIX_EPOCH + Duration::from_secs(ts as u64)),
            );
        }
        Ok(out)
    }

    // ── primary / secondary ───────────────────────────────────────────────────

    /// Return the UUIDs of all primary records, ordered by timestamp ascending.
    pub fn list_primaries(&self) -> Result<Vec<Uuid>> {
        let rows = self.engine.select_all(
            "SELECT id FROM telemetry WHERE is_primary = 1 ORDER BY ts ASC",
        )?;
        parse_uuid_column(rows)
    }

    /// Return the UUIDs of primary records whose event timestamp falls in
    /// `[start, end)`, ordered by timestamp ascending.
    pub fn list_primaries_in_range(
        &self,
        start: SystemTime,
        end: SystemTime,
    ) -> Result<Vec<Uuid>> {
        let s = to_unix_secs(start)?;
        let e = to_unix_secs(end)?;
        let rows = self.engine.select_all(&format!(
            "SELECT id FROM telemetry \
             WHERE is_primary = 1 AND ts >= {s} AND ts < {e} ORDER BY ts ASC"
        ))?;
        parse_uuid_column(rows)
    }

    /// Return the UUIDs of all secondary records linked to `primary_id`,
    /// ordered by their event timestamp ascending.
    pub fn list_secondaries(&self, primary_id: Uuid) -> Result<Vec<Uuid>> {
        let rows = self.engine.select_all(&format!(
            "SELECT secondary_id FROM primary_secondary \
             WHERE primary_id = '{primary_id}' ORDER BY ts ASC"
        ))?;
        parse_uuid_column(rows)
    }

    /// Return the `(min_ts, max_ts)` of all records in this shard.
    ///
    /// Both values are Unix seconds (`i64`). Returns `(None, None)` when the
    /// shard contains no records.
    pub fn timestamp_range(&self) -> Result<(Option<i64>, Option<i64>)> {
        let rows = self
            .engine
            .select_all("SELECT MIN(ts), MAX(ts) FROM telemetry")?;
        if let Some(mut cols) = rows.into_iter().next() {
            let min = cols.drain(0..1).next().and_then(|v| v.cast_int().ok());
            let max = cols.into_iter().next().and_then(|v| v.cast_int().ok());
            Ok((min, max))
        } else {
            Ok((None, None))
        }
    }

    /// Count all records in this shard.
    pub fn count_all(&self) -> Result<u64> {
        self.count_rows("SELECT COUNT(*) FROM telemetry")
    }

    /// Count records whose event timestamp falls in `[start, end)`.
    pub fn count_in_range(&self, start: SystemTime, end: SystemTime) -> Result<u64> {
        let s = crate::common::timerange::to_unix_secs(start)?;
        let e = crate::common::timerange::to_unix_secs(end)?;
        self.count_rows(&format!(
            "SELECT COUNT(*) FROM telemetry WHERE ts >= {s} AND ts < {e}"
        ))
    }

    fn count_rows(&self, sql: &str) -> Result<u64> {
        let rows = self.engine.select_all(sql)?;
        let n = rows
            .into_iter()
            .next()
            .and_then(|mut cols| cols.drain(0..1).next())
            .and_then(|v| v.cast_int().ok())
            .unwrap_or(0);
        Ok(n as u64)
    }

    /// Flush the DuckDB WAL to disk (CHECKPOINT).
    pub fn sync(&self) -> Result<()> {
        self.engine.sync()
    }

    // ── internal ──────────────────────────────────────────────────────────────

    /// Compare `embedding` against all stored primary embeddings.
    ///
    /// Returns `(is_primary, Option<most_similar_primary_uuid>)`.
    fn classify(&self, embedding: &[f32]) -> Result<(bool, Option<Uuid>)> {
        let rows = self
            .engine
            .select_all("SELECT primary_id, embedding FROM primary_embeddings")?;

        if rows.is_empty() {
            return Ok((true, None));
        }

        let mut best_sim = f32::NEG_INFINITY;
        let mut best_id: Option<Uuid> = None;

        for row in rows {
            let mut it = row.into_iter();
            let pid = parse_uuid_value(
                it.next()
                    .ok_or_else(|| err_msg("primary_embeddings row missing primary_id"))?,
                "primary_embeddings.primary_id",
            )?;
            let emb_bytes = it
                .next()
                .ok_or_else(|| err_msg("primary_embeddings row missing embedding"))?
                .cast_bin()
                .map_err(|e| err_msg(e.to_string()))?;
            let prim_emb = bytes_to_embedding(&emb_bytes);
            let sim = cosine_similarity(embedding, &prim_emb)?;
            if sim > best_sim {
                best_sim = sim;
                best_id = Some(pid);
            }
        }

        if best_sim >= self.config.similarity_threshold {
            Ok((false, best_id))
        } else {
            Ok((true, None))
        }
    }

    /// Append `ts` to the deduplication log for `(key, data_text)`.
    fn record_duplicate(&self, key: &str, data_text: &str, ts: i64) -> Result<()> {
        let rows = self.engine.select_all(&format!(
            "SELECT timestamps FROM dedup_tracking \
             WHERE key = '{}' AND data_text = '{}'",
            sql_escape(key),
            sql_escape(data_text),
        ))?;

        if let Some(row) = rows.into_iter().next() {
            let ts_json = row
                .into_iter()
                .next()
                .ok_or_else(|| err_msg("dedup_tracking row missing timestamps"))?
                .cast_string()
                .map_err(|e| err_msg(e.to_string()))?;
            let mut tss: Vec<i64> = serde_json::from_str(&ts_json)
                .map_err(|e| err_msg(format!("dedup timestamps parse failed: {e}")))?;
            tss.push(ts);
            let updated = serde_json::to_string(&tss)
                .map_err(|e| err_msg(format!("dedup timestamps serialise failed: {e}")))?;
            self.engine.execute(&format!(
                "UPDATE dedup_tracking SET timestamps = '{}'::JSON \
                 WHERE key = '{}' AND data_text = '{}'",
                sql_escape(&updated),
                sql_escape(key),
                sql_escape(data_text),
            ))?;
        } else {
            let init = serde_json::to_string(&[ts])
                .map_err(|e| err_msg(format!("dedup timestamps serialise failed: {e}")))?;
            self.engine.execute(&format!(
                "INSERT INTO dedup_tracking VALUES ('{}', '{}', '{}'::JSON)",
                sql_escape(key),
                sql_escape(data_text),
                sql_escape(&init),
            ))?;
        }
        Ok(())
    }
}

// ── free helpers ──────────────────────────────────────────────────────────────

fn parse_timestamp(val: &JsonValue) -> Result<i64> {
    match val {
        JsonValue::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .ok_or_else(|| err_msg("'timestamp' number is out of i64 range")),
        JsonValue::String(s) => s
            .parse::<i64>()
            .map_err(|_| err_msg(format!("'timestamp' string is not a valid integer: {s}"))),
        _ => Err(err_msg("'timestamp' must be a number or numeric string")),
    }
}

/// Convert the `data` field to a plain string for deduplication and embedding.
fn data_to_text(data: &JsonValue) -> String {
    match data {
        JsonValue::String(s) => s.clone(),
        JsonValue::Number(n) => n.to_string(),
        JsonValue::Bool(b) => b.to_string(),
        JsonValue::Null => String::new(),
        other => json_fingerprint(other),
    }
}

/// Build a metadata object from the document, excluding mandatory telemetry keys.
fn build_metadata(doc: &JsonValue) -> JsonValue {
    const SKIP: &[&str] = &["id", "timestamp", "key", "data"];
    let map = match doc {
        JsonValue::Object(m) => m
            .iter()
            .filter(|(k, _)| !SKIP.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        _ => serde_json::Map::new(),
    };
    JsonValue::Object(map)
}

fn embedding_to_bytes(emb: &[f32]) -> Vec<u8> {
    emb.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn bytes_to_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Reconstruct the full telemetry document from a `SELECT id,ts,key,data,metadata` row.
fn row_to_doc(row: Vec<DynamicValue>) -> Result<JsonValue> {
    let mut it = row.into_iter();
    let cast_err = |e: Box<dyn std::error::Error>| err_msg(e.to_string());

    let id_str = it
        .next()
        .ok_or_else(|| err_msg("telemetry row missing id"))?
        .cast_string()
        .map_err(cast_err)?;
    let ts = it
        .next()
        .ok_or_else(|| err_msg("telemetry row missing ts"))?
        .cast_int()
        .map_err(cast_err)?;
    let key = it
        .next()
        .ok_or_else(|| err_msg("telemetry row missing key"))?
        .cast_string()
        .map_err(cast_err)?;
    let data_s = it
        .next()
        .ok_or_else(|| err_msg("telemetry row missing data"))?
        .cast_string()
        .map_err(cast_err)?;
    let meta_s = it
        .next()
        .ok_or_else(|| err_msg("telemetry row missing metadata"))?
        .cast_string()
        .map_err(cast_err)?;

    let data: JsonValue = serde_json::from_str(&data_s)
        .map_err(|e| err_msg(format!("data JSON parse failed: {e}")))?;
    let metadata: JsonValue = serde_json::from_str(&meta_s)
        .map_err(|e| err_msg(format!("metadata JSON parse failed: {e}")))?;

    let mut doc = match metadata {
        JsonValue::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    doc.insert("id".to_string(), json!(id_str));
    doc.insert("timestamp".to_string(), json!(ts));
    doc.insert("key".to_string(), json!(key));
    doc.insert("data".to_string(), data);
    Ok(JsonValue::Object(doc))
}

fn parse_uuid_value(v: DynamicValue, ctx: &str) -> Result<Uuid> {
    let s = v.cast_string().map_err(|e| err_msg(e.to_string()))?;
    Uuid::parse_str(&s).map_err(|e| err_msg(format!("invalid UUID in {ctx}: {e}")))
}

fn parse_uuid_field(row: Vec<DynamicValue>, idx: usize, ctx: &str) -> Result<Uuid> {
    parse_uuid_value(
        row.into_iter()
            .nth(idx)
            .ok_or_else(|| err_msg(format!("row missing column {idx} for {ctx}")))?,
        ctx,
    )
}

fn parse_uuid_column(rows: Vec<Vec<DynamicValue>>) -> Result<Vec<Uuid>> {
    rows.into_iter()
        .map(|row| parse_uuid_field(row, 0, "id column"))
        .collect()
}

// Suppress unused import warning — extract_key is re-exported for callers who
// want to inspect the same dot-notation path logic used internally.
pub use crate::common::jsonfingerprint::extract_key as json_extract_key;
