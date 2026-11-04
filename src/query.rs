//! Query helpers that keep SQL errors and decoder errors as their original types.

use std::error;
use std::fmt;

use crate::{Connection, Error, Params, Row};

/// A query failed in SQLite or while decoding a result row.
///
/// The decoder error has no trait bounds. This wrapper implements
/// [`std::error::Error`] when `E` does. For other error containers, match the
/// variants to recover the original value and query context.
#[derive(Debug)]
pub enum QueryError<E> {
    /// Preparing, binding, or reading the query failed.
    Sql {
        /// The query being executed.
        sql: String,
        /// The original SQLite error.
        source: Error,
    },
    /// The caller's decoder failed.
    Decode {
        /// The query being executed.
        sql: String,
        /// The zero-based result row.
        row: usize,
        /// The original decoder error, with no conversion required.
        source: E,
        /// A failure while finishing the statement after decoding failed.
        finish_error: Option<Box<Error>>,
    },
}
impl<E: fmt::Display> fmt::Display for QueryError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql { sql, source } => write!(f, "query {sql:?}: {source}"),
            Self::Decode {
                sql,
                row,
                source,
                finish_error,
            } => {
                write!(f, "decode row {row} from {sql:?}: {source}")?;
                if let Some(error) = finish_error {
                    write!(f, ", statement completion failed: {error}")?;
                }

                Ok(())
            }
        }
    }
}
impl<E: error::Error + 'static> error::Error for QueryError<E> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Sql { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
        }
    }
}

impl Connection {
    /// Decode all result rows, stopping at the first SQL or decoder error.
    ///
    /// Decoder errors need no conversion into a rusqlite error. Statements use
    /// the connection's cache when the `cache` feature is enabled.
    pub fn query_all<T, E, P, F>(
        &self,
        sql: &str,
        params: P,
        mut decode: F,
    ) -> Result<Vec<T>, QueryError<E>>
    where
        P: Params,
        F: FnMut(&Row<'_>) -> Result<T, E>,
    {
        let sql_error = |source| QueryError::Sql {
            sql: sql.into(),
            source,
        };
        #[cfg(feature = "cache")]
        let mut stmt = self.prepare_cached(sql).map_err(sql_error)?;
        #[cfg(not(feature = "cache"))]
        let mut stmt = self.prepare(sql).map_err(sql_error)?;
        let mut rows = stmt.query(params).map_err(sql_error)?;
        let mut values = Vec::new();

        while let Some(row) = rows.next().map_err(sql_error)? {
            let value = match decode(row) {
                Ok(value) => value,
                Err(source) => {
                    return Err(QueryError::Decode {
                        sql: sql.into(),
                        row: values.len(),
                        source,
                        finish_error: rows.finish().err().map(Box::new),
                    });
                }
            };
            values.push(value);
        }

        Ok(values)
    }

    /// Decode only the first result row, returning `None` for an empty result.
    ///
    /// Later rows are neither read nor decoded. Statements use the connection's
    /// cache when the `cache` feature is enabled.
    pub fn query_first<T, E, P, F>(
        &self,
        sql: &str,
        params: P,
        decode: F,
    ) -> Result<Option<T>, QueryError<E>>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T, E>,
    {
        let sql_error = |source| QueryError::Sql {
            sql: sql.into(),
            source,
        };
        #[cfg(feature = "cache")]
        let mut stmt = self.prepare_cached(sql).map_err(sql_error)?;
        #[cfg(not(feature = "cache"))]
        let mut stmt = self.prepare(sql).map_err(sql_error)?;
        let mut rows = stmt.query(params).map_err(sql_error)?;

        let result = rows.next().map_err(sql_error)?.map(decode).transpose();
        let finish = rows.finish();

        match result {
            Ok(value) => {
                finish.map_err(sql_error)?;
                Ok(value)
            }
            Err(source) => Err(QueryError::Decode {
                sql: sql.into(),
                row: 0,
                source,
                finish_error: finish.err().map(Box::new),
            }),
        }
    }

    /// Execute a statement using the prepared statement cache.
    #[cfg(feature = "cache")]
    pub fn execute_cached<P: Params>(&self, sql: &str, params: P) -> crate::Result<usize> {
        self.prepare_cached(sql)?.execute(params)
    }

    /// Decode the first row using the prepared statement cache.
    #[cfg(feature = "cache")]
    pub fn query_row_cached<T, P, F>(&self, sql: &str, params: P, decode: F) -> crate::Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> crate::Result<T>,
    {
        self.prepare_cached(sql)?.query_row(params, decode)
    }

    /// Decode exactly one row using the prepared statement cache.
    #[cfg(feature = "cache")]
    pub fn query_one_cached<T, P, F>(&self, sql: &str, params: P, decode: F) -> crate::Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> crate::Result<T>,
    {
        self.prepare_cached(sql)?.query_one(params, decode)
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn finish_write() -> crate::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.db");
        let writer = Connection::open(&path)?;
        let reader = Connection::open(&path)?;
        writer.busy_timeout(std::time::Duration::ZERO)?;
        writer.execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1)")?;
        reader.execute_batch("BEGIN; SELECT * FROM t")?;

        let error = writer
            .query_first("INSERT INTO t VALUES (2) RETURNING a", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_err();
        let QueryError::Sql { source, .. } = error else {
            panic!("expected finishing to fail");
        };

        assert_eq!(
            source.sqlite_error_code(),
            Some(crate::ErrorCode::DatabaseBusy)
        );
        Ok(())
    }

    #[test]
    fn first_stops() -> crate::Result<()> {
        let db = Connection::open_in_memory()?;
        let row = db
            .query_first("SELECT 1 UNION ALL SELECT 2", [], |row| {
                let value: i64 = row.get(0)?;
                if value == 2 {
                    return Err(Error::InvalidQuery);
                }

                Ok(value)
            })
            .unwrap();

        assert_eq!(row, Some(1));
        Ok(())
    }

    #[test]
    fn arbitrary_error() -> crate::Result<()> {
        struct DecodeFailure;
        let db = Connection::open_in_memory()?;
        let error = db.query_all("SELECT 1", [], |_| Err::<(), _>(DecodeFailure));

        match error {
            Err(QueryError::Decode {
                source: DecodeFailure,
                ..
            }) => (),
            Err(QueryError::Sql { source, .. }) => panic!("unexpected SQL error: {source}"),
            Ok(_) => panic!("decoder error was discarded"),
        }
        Ok(())
    }
}
