use crate::common::error::{Error as EasyError, Result};
use duckdb::{DuckdbConnectionManager, Error as DuckError, Row};
use r2d2::{Pool, PooledConnection};
use rust_dynamic::value::Value as DynamicValue;
use scheduled_thread_pool::ScheduledThreadPool;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Hard ceiling on how long `pool.get()` will block waiting for a free
/// connection.  r2d2's default is **30 s** — long enough that a
/// saturated pool looks like a hang.  10 s fails fast with a clear
/// error while still riding out a brief, legitimate contention burst.
///
/// Every checkout that hits this timeout increments
/// [`pool_checkout_timeouts`] — a non-zero counter there is the
/// signal that a pool is undersized for its workload (raise
/// `pool_size`, or shed the load that's holding connections).
const POOL_CHECKOUT_TIMEOUT: Duration = Duration::from_secs(10);

/// Process-wide count of `pool.get()` calls that exceeded
/// [`POOL_CHECKOUT_TIMEOUT`].  Surfaced via `v2/status.pool`.
static POOL_CHECKOUT_TIMEOUTS: OnceLock<AtomicU64> = OnceLock::new();

/// Lifetime count of connection-pool checkout timeouts across every
/// `StorageEngine` in the process.  `0` is healthy; non-zero means at
/// least one DuckDB pool ran out of connections under load.
pub fn pool_checkout_timeouts() -> u64 {
    POOL_CHECKOUT_TIMEOUTS
        .get_or_init(|| AtomicU64::new(0))
        .load(Ordering::Relaxed)
}

fn record_checkout_timeout() {
    POOL_CHECKOUT_TIMEOUTS
        .get_or_init(|| AtomicU64::new(0))
        .fetch_add(1, Ordering::Relaxed);
}

/// Process-wide shared r2d2 maintenance pool.
///
/// r2d2's default is to create a fresh `ScheduledThreadPool` with 3 threads
/// per `Pool::build()` call.  With many DuckDB pools open (one per StorageEngine,
/// many StorageEngines per Shard, many shards) that number compounds quickly and
/// can exhaust the OS thread limit (RLIMIT_NPROC / EAGAIN).
///
/// By sharing a single pool we keep the r2d2 maintenance thread count constant
/// regardless of how many connection pools are open.
///
/// Call [`init_r2d2_thread_pool`] once at startup (before any `StorageEngine` is
/// constructed) to set the thread count from config.  If it has not been called
/// by the time the first pool is needed, a fallback of 3 threads is used.
static R2D2_THREAD_POOL: OnceLock<Arc<ScheduledThreadPool>> = OnceLock::new();

/// Initialise the shared r2d2 thread pool with `num_threads` worker threads.
///
/// Must be called before any [`StorageEngine`] is constructed.  Subsequent calls
/// are no-ops (the pool is already set); the first call wins.
pub(crate) fn init_r2d2_thread_pool(num_threads: usize) {
    let _ = R2D2_THREAD_POOL.set(Arc::new(
        ScheduledThreadPool::builder()
            .num_threads(num_threads.max(1))
            .thread_name_pattern("r2d2-worker-{}")
            .build(),
    ));
}

fn shared_r2d2_thread_pool() -> Arc<ScheduledThreadPool> {
    R2D2_THREAD_POOL
        .get_or_init(|| {
            Arc::new(
                ScheduledThreadPool::builder()
                    .num_threads(3)
                    .thread_name_pattern("r2d2-worker-{}")
                    .build(),
            )
        })
        .clone()
}

/// Owned scalar parameter for the parameterized `*_params` query
/// methods.  A small concrete enum (rather than `&dyn ToSql`) keeps
/// callers free of lifetime juggling — they build `Vec<SqlParam>`
/// directly.  Covers every scalar the storage layers bind: text
/// (incl. UUID/JSON serialised to string), integers, floats, bools,
/// and SQL NULL.
#[derive(Debug, Clone)]
pub enum SqlParam {
    Text(String),
    Int(i64),
    Real(f64),
    Bool(bool),
    Null,
}

impl duckdb::ToSql for SqlParam {
    fn to_sql(&self) -> duckdb::Result<duckdb::types::ToSqlOutput<'_>> {
        use duckdb::types::{ToSqlOutput, Value};
        Ok(match self {
            SqlParam::Text(s) => ToSqlOutput::Owned(Value::Text(s.clone())),
            SqlParam::Int(i)  => ToSqlOutput::Owned(Value::BigInt(*i)),
            SqlParam::Real(f) => ToSqlOutput::Owned(Value::Double(*f)),
            SqlParam::Bool(b) => ToSqlOutput::Owned(Value::Boolean(*b)),
            SqlParam::Null    => ToSqlOutput::Owned(Value::Null),
        })
    }
}

pub struct StorageEngine {
    pool: Pool<DuckdbConnectionManager>,
}

impl StorageEngine {
    pub fn new<P: AsRef<Path>>(path: P, init_sql: &'static str, pool_size: u32) -> Result<Self> {
        let manager = DuckdbConnectionManager::file(path)
            .map_err(|e| EasyError::new("Failed to create connection manager", e))?;

        let pool = Pool::builder()
            .max_size(pool_size)
            // Fail fast instead of r2d2's 30 s default — a checkout
            // that can't be satisfied in 10 s means the pool is
            // saturated, and a clear error beats a silent stall.
            .connection_timeout(POOL_CHECKOUT_TIMEOUT)
            .thread_pool(shared_r2d2_thread_pool())
            .build(manager)
            .map_err(|e| EasyError::new("Failed to initialize connection pool", e))?;

        // Initialize schema using a temporary connection from the pool
        {
            let conn = pool
                .get()
                .map_err(|e| EasyError::new("Could not get init connection", e))?;
            conn.execute_batch(init_sql)
                .map_err(|e| EasyError::new("Initialization SQL failed", e))?;
        }

        Ok(Self { pool })
    }

    /// Check a connection out of the pool, bounded by
    /// [`POOL_CHECKOUT_TIMEOUT`].  A timeout increments the global
    /// [`pool_checkout_timeouts`] counter and returns a clear error
    /// naming the calling operation — never an indefinite stall.
    fn checkout(&self, op: &str) -> Result<PooledConnection<DuckdbConnectionManager>> {
        self.pool.get().map_err(|e| {
            // r2d2 surfaces a checkout timeout as its generic pool
            // error; we can't cleanly distinguish "timed out" from
            // "manager broken", so we count every checkout failure —
            // in practice, with a healthy manager, the timeout is the
            // only way `get()` fails.
            record_checkout_timeout();
            EasyError::new(&format!("pool checkout failed for {op} (pool saturated?)"), e)
        })
    }

    fn map_to_duck<E: std::fmt::Display>(e: E) -> DuckError {
        let safe_err = std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string());
        DuckError::ToSqlConversionFailure(Box::new(safe_err))
    }

    fn row_to_dynamic(row: &Row) -> duckdb::Result<Vec<DynamicValue>> {
        let column_count = row.as_ref().column_count();
        let mut values = Vec::with_capacity(column_count);

        for i in 0..column_count {
            let duck_val = row.get::<_, duckdb::types::Value>(i)?;

            let val = match duck_val {
                duckdb::types::Value::Boolean(b) => {
                    DynamicValue::from(b).map_err(Self::map_to_duck)?
                }
                duckdb::types::Value::Int(iv) => {
                    DynamicValue::from(iv as i64).map_err(Self::map_to_duck)?
                }
                duckdb::types::Value::BigInt(iv) => {
                    DynamicValue::from(iv).map_err(Self::map_to_duck)?
                }
                duckdb::types::Value::Float(f) => {
                    DynamicValue::from(f as f64).map_err(Self::map_to_duck)?
                }
                duckdb::types::Value::Double(d) => {
                    DynamicValue::from(d).map_err(Self::map_to_duck)?
                }
                duckdb::types::Value::Text(t) => {
                    DynamicValue::from(t).map_err(Self::map_to_duck)?
                }
                duckdb::types::Value::Blob(b) => DynamicValue::from_bin(b),
                _ => DynamicValue::nodata(),
            };
            values.push(val);
        }
        Ok(values)
    }

    pub fn select_all(&self, sql: &str) -> Result<Vec<Vec<DynamicValue>>> {
        let conn = self.checkout("select_all")?;

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EasyError::new("Query preparation failed", e))?;

        let rows = stmt
            .query_map([], |row| Self::row_to_dynamic(row))
            .map_err(|e| EasyError::new("Execution of select_all failed", e))?;

        let mut results: Vec<Vec<DynamicValue>> = Vec::new();
        for row_result in rows {
            let row: Vec<DynamicValue> = row_result.map_err(|e| EasyError::new("Error fetching row", e))?;
            results.push(row);
        }
        Ok(results)
    }

    pub fn select_foreach<F>(&self, sql: &str, mut f: F) -> Result<()>
    where
        F: FnMut(Vec<DynamicValue>) -> Result<()>,
    {
        let conn = self.checkout("select_foreach")?;

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EasyError::new("Query preparation failed", e))?;

        let mut rows = stmt
            .query([])
            .map_err(|e| EasyError::new("Query execution failed", e))?;

        while let Some(row_result) = rows
            .next()
            .map_err(|e| EasyError::new("Iteration error", e))?
        {
            let dynamic_row = Self::row_to_dynamic(row_result)
                .map_err(|e| EasyError::new("Row conversion failed", e))?;
            f(dynamic_row)?;
        }
        Ok(())
    }

    pub fn execute(&self, sql: &str) -> Result<()> {
        let conn = self.checkout("execute")?;

        conn.execute(sql, [])
            .map_err(|e| EasyError::new("SQL execution failed", e))?;
        Ok(())
    }

    /// Execute multiple SQL statements inside a single `BEGIN … COMMIT` transaction.
    ///
    /// All statements are sent to one connection in one round-trip, eliminating
    /// the per-statement pool-checkout + WAL-flush overhead. No-op when
    /// `statements` is empty.
    pub fn execute_many(&self, statements: &[String]) -> Result<()> {
        if statements.is_empty() {
            return Ok(());
        }
        let conn = self.checkout("execute_many")?;
        let sql = format!("BEGIN;\n{};\nCOMMIT;", statements.join(";\n"));
        conn.execute_batch(&sql)
            .map_err(|e| EasyError::new("Batch transaction failed", e))?;
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        self.execute("CHECKPOINT;")
    }

    // ── parameterized variants ────────────────────────────────────────────────
    //
    // Point-lookup workloads (e.g. the graph layer) bind external,
    // possibly-untrusted string keys.  These variants pass values as
    // bound parameters — injection-safe and prepared-statement
    // friendly — instead of string-interpolated SQL.

    /// `select_all` with bound parameters.  Placeholders in `sql` are
    /// positional `?`.
    pub fn select_all_params(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<Vec<Vec<DynamicValue>>> {
        let conn = self.checkout("select_all_params")?;

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EasyError::new("Query preparation failed", e))?;

        let rows = stmt
            .query_map(duckdb::params_from_iter(params.iter()), |row| {
                Self::row_to_dynamic(row)
            })
            .map_err(|e| EasyError::new("Execution of select_all_params failed", e))?;

        let mut results: Vec<Vec<DynamicValue>> = Vec::new();
        for row_result in rows {
            results.push(row_result.map_err(|e| EasyError::new("Error fetching row", e))?);
        }
        Ok(results)
    }

    /// `execute` with bound parameters.  Returns the number of rows
    /// affected.
    pub fn execute_params(&self, sql: &str, params: &[SqlParam]) -> Result<usize> {
        let conn = self.checkout("execute_params")?;
        conn.execute(sql, duckdb::params_from_iter(params.iter()))
            .map_err(|e| EasyError::new("Parameterized SQL execution failed", e))
    }

    /// Run several parameterized statements inside a single
    /// `BEGIN … COMMIT` on one pooled connection.  No-op when empty.
    /// On any statement failure the transaction is rolled back.
    pub fn execute_many_params(&self, statements: &[(String, Vec<SqlParam>)]) -> Result<()> {
        if statements.is_empty() {
            return Ok(());
        }
        let mut conn = self.checkout("execute_many_params")?;
        let tx = conn
            .transaction()
            .map_err(|e| EasyError::new("Could not open transaction", e))?;
        for (sql, params) in statements {
            tx.execute(sql, duckdb::params_from_iter(params.iter()))
                .map_err(|e| EasyError::new("Parameterized batch statement failed", e))?;
        }
        tx.commit()
            .map_err(|e| EasyError::new("Batch transaction commit failed", e))?;
        Ok(())
    }
}
