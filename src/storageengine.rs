use duckdb::{DuckdbConnectionManager, Error as DuckError, Row};
use easy_error::Error as EasyError;
use r2d2::Pool;
use rust_dynamic::value::Value as DynamicValue;
use std::path::Path;

type EngineResult<T> = std::result::Result<T, EasyError>;

pub struct StorageEngine {
    pool: Pool<DuckdbConnectionManager>,
}

impl StorageEngine {
    pub fn new<P: AsRef<Path>>(path: P, init_sql: &'static str) -> EngineResult<Self> {
        let manager = DuckdbConnectionManager::file(path)
            .map_err(|e| EasyError::new("Failed to create connection manager", e))?;

        let pool = Pool::builder()
            .max_size(16) // Adjust based on your CPU cores/concurrency needs
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

    pub fn select_all(&self, sql: &str) -> EngineResult<Vec<Vec<DynamicValue>>> {
        let conn = self
            .pool
            .get()
            .map_err(|e| EasyError::new("Pool checkout failed for select_all", e))?;

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

    pub fn select_foreach<F>(&self, sql: &str, mut f: F) -> EngineResult<()>
    where
        F: FnMut(Vec<DynamicValue>) -> EngineResult<()>,
    {
        let conn = self
            .pool
            .get()
            .map_err(|e| EasyError::new("Pool checkout failed for select_foreach", e))?;

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

    pub fn execute(&self, sql: &str) -> EngineResult<()> {
        let conn = self
            .pool
            .get()
            .map_err(|e| EasyError::new("Pool checkout failed for execute", e))?;

        conn.execute(sql, [])
            .map_err(|e| EasyError::new("SQL execution failed", e))?;
        Ok(())
    }

    pub fn sync(&self) -> EngineResult<()> {
        self.execute("CHECKPOINT;")
    }
}
