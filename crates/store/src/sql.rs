//! Plumbing between domain records and SQLite rows: column values, row
//! decoding helpers and the mapping of SQLite failures to [`Error`].

use core::error::Error as StdError;
use core::fmt::Display;

use liftoff_core::Error;
use liftoff_core::domain::{Timestamp, TokenHash};
use rusqlite::types::{FromSql, Type, Value};
use rusqlite::{Connection, OptionalExtension as _, Row, ffi, params_from_iter};
use time::format_description::well_known::Rfc3339;

// ---- writing ---------------------------------------------------------------

/// The columns of one row. The first column is the primary key.
#[derive(Debug)]
pub(crate) struct Record {
    /// Singular name used in error messages ("user").
    pub(crate) what: &'static str,
    pub(crate) table: &'static str,
    pub(crate) columns: Vec<(&'static str, Value)>,
}

impl Record {
    pub(crate) fn insert(&self, conn: &Connection) -> rusqlite::Result<()> {
        let names: Vec<&str> = self.columns.iter().map(|(name, _)| *name).collect();
        let marks = vec!["?"; names.len()].join(", ");
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({marks})",
            self.table,
            names.join(", ")
        );
        let values = self.columns.iter().map(|(_, value)| value);
        conn.prepare_cached(&sql)?
            .execute(params_from_iter(values))
            .map(drop)
    }

    /// Replaces every column but the key; returns the number of rows hit.
    pub(crate) fn update(&self, conn: &Connection) -> rusqlite::Result<usize> {
        let Some(((key, key_value), rest)) = self.columns.split_first() else {
            return Ok(0);
        };
        let assignments: Vec<String> = rest.iter().map(|(name, _)| format!("{name} = ?")).collect();
        let sql = format!(
            "UPDATE {} SET {} WHERE {key} = ?",
            self.table,
            assignments.join(", ")
        );
        let values = rest.iter().map(|(_, value)| value).chain([key_value]);
        conn.prepare_cached(&sql)?.execute(params_from_iter(values))
    }
}

/// A domain record that is stored as one row.
pub(crate) trait ToRecord {
    /// # Errors
    ///
    /// [`Error::Internal`] when a value has no stored form (a year beyond
    /// RFC 3339, say).
    fn record(&self) -> Result<Record, Error>;
}

/// A domain record that is read from one row.
pub(crate) trait FromRow: Sized {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self>;
}

pub(crate) fn text(value: impl Display) -> Value {
    Value::Text(value.to_string())
}

pub(crate) fn opt_text(value: Option<impl Display>) -> Value {
    value.map_or(Value::Null, text)
}

pub(crate) fn int(value: impl Into<i64>) -> Value {
    Value::Integer(value.into())
}

pub(crate) fn opt_int(value: Option<impl Into<i64>>) -> Value {
    value.map_or(Value::Null, int)
}

pub(crate) fn flag(value: bool) -> Value {
    Value::Integer(i64::from(value))
}

pub(crate) fn blob(value: &TokenHash) -> Value {
    Value::Blob(value.as_bytes().to_vec())
}

pub(crate) fn time(value: Timestamp) -> Result<Value, Error> {
    value
        .format(&Rfc3339)
        .map(Value::Text)
        .map_err(Error::internal)
}

pub(crate) fn opt_time(value: Option<Timestamp>) -> Result<Value, Error> {
    value.map_or(Ok(Value::Null), time)
}

// ---- reading ---------------------------------------------------------------

fn conversion(
    row: &Row<'_>,
    column: &str,
    error: impl StdError + Send + Sync + 'static,
) -> rusqlite::Error {
    match row.as_ref().column_index(column) {
        Ok(index) => rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error)),
        Err(missing) => missing,
    }
}

/// A text column decoded by a domain constructor.
pub(crate) fn parsed<T, E>(
    row: &Row<'_>,
    column: &str,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> rusqlite::Result<T>
where
    E: StdError + Send + Sync + 'static,
{
    let raw: String = row.get(column)?;
    parse(&raw).map_err(|error| conversion(row, column, error))
}

/// A nullable text column decoded by a domain constructor.
pub(crate) fn parsed_opt<T, E>(
    row: &Row<'_>,
    column: &str,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> rusqlite::Result<Option<T>>
where
    E: StdError + Send + Sync + 'static,
{
    let raw: Option<String> = row.get(column)?;
    raw.map(|raw| parse(&raw).map_err(|error| conversion(row, column, error)))
        .transpose()
}

/// An integer column decoded by a domain constructor.
pub(crate) fn number<N, T, E>(
    row: &Row<'_>,
    column: &str,
    make: impl FnOnce(N) -> Result<T, E>,
) -> rusqlite::Result<T>
where
    N: FromSql,
    E: StdError + Send + Sync + 'static,
{
    let raw: N = row.get(column)?;
    make(raw).map_err(|error| conversion(row, column, error))
}

/// A nullable integer column decoded by a domain constructor.
pub(crate) fn number_opt<N, T, E>(
    row: &Row<'_>,
    column: &str,
    make: impl FnOnce(N) -> Result<T, E>,
) -> rusqlite::Result<Option<T>>
where
    N: FromSql,
    E: StdError + Send + Sync + 'static,
{
    let raw: Option<N> = row.get(column)?;
    raw.map(|raw| make(raw).map_err(|error| conversion(row, column, error)))
        .transpose()
}

pub(crate) fn timestamp(row: &Row<'_>, column: &str) -> rusqlite::Result<Timestamp> {
    parsed(row, column, |raw| Timestamp::parse(raw, &Rfc3339))
}

pub(crate) fn timestamp_opt(row: &Row<'_>, column: &str) -> rusqlite::Result<Option<Timestamp>> {
    parsed_opt(row, column, |raw| Timestamp::parse(raw, &Rfc3339))
}

pub(crate) fn token_hash(row: &Row<'_>, column: &str) -> rusqlite::Result<TokenHash> {
    let raw: Vec<u8> = row.get(column)?;
    <[u8; 32]>::try_from(raw)
        .map(TokenHash::from_bytes)
        .map_err(|bytes| conversion(row, column, BadHashLength { found: bytes.len() }))
}

#[derive(Debug)]
struct BadHashLength {
    found: usize,
}

impl Display for BadHashLength {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "token hash of {} bytes, expected 32", self.found)
    }
}

impl StdError for BadHashLength {}

pub(crate) fn find<T: FromRow>(
    conn: &Connection,
    sql: &str,
    args: &[Value],
) -> rusqlite::Result<Option<T>> {
    conn.prepare_cached(sql)?
        .query_row(params_from_iter(args), T::from_row)
        .optional()
}

pub(crate) fn list<T: FromRow>(
    conn: &Connection,
    sql: &str,
    args: &[Value],
) -> rusqlite::Result<Vec<T>> {
    let mut statement = conn.prepare_cached(sql)?;
    let rows = statement.query_map(params_from_iter(args), T::from_row)?;
    rows.collect()
}

// ---- errors ----------------------------------------------------------------

const fn constraint(error: &rusqlite::Error) -> Option<i32> {
    match error {
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(failure.code, rusqlite::ErrorCode::ConstraintViolation) =>
        {
            Some(failure.extended_code)
        }
        _ => None,
    }
}

/// The failure of an insert or update of a `what`: a taken key is a
/// [`Error::Conflict`], a missing parent (`parent`) an [`Error::NotFound`].
pub(crate) fn write_error(error: rusqlite::Error, what: &str, parent: &str) -> Error {
    match constraint(&error) {
        Some(ffi::SQLITE_CONSTRAINT_UNIQUE | ffi::SQLITE_CONSTRAINT_PRIMARYKEY) => {
            Error::conflict(format!("{what} already exists"))
        }
        Some(ffi::SQLITE_CONSTRAINT_FOREIGNKEY) => Error::not_found(parent),
        _ => internal(error),
    }
}

/// The failure of a delete: rows that still point at it are a
/// [`Error::Conflict`] saying `in_use`.
pub(crate) fn delete_error(error: rusqlite::Error, in_use: &str) -> Error {
    match constraint(&error) {
        // SQLite implements `ON DELETE RESTRICT` with an internal trigger, so
        // it reports CONSTRAINT_TRIGGER rather than CONSTRAINT_FOREIGNKEY.
        Some(ffi::SQLITE_CONSTRAINT_FOREIGNKEY | ffi::SQLITE_CONSTRAINT_TRIGGER) => {
            Error::conflict(in_use)
        }
        _ => internal(error),
    }
}

/// Any failure that is not the caller's doing.
pub(crate) fn internal(error: rusqlite::Error) -> Error {
    tracing::error!(%error, "sqlite store failure");
    Error::internal(error)
}
