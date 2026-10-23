//! The SQL of a module's storage over SQLite: its key space (table
//! `module_keys`, [`KEYS_SCHEMA`]) and `store-query` behind an authorizer
//! (`Connection::authorizer`, rusqlite feature `hooks`), as functions over
//! a [`Connection`].
//!
//! Two adapters keep module tables: the daemon's store (`liftoff-store`,
//! in its one database) and the CLI's local module store (`liftoff`,
//! `~/.config/liftoff/modules.db`). Both hold modules to the same rules, so
//! the rules live here once: this crate is a library of SQL, not an
//! adapter of a port (it implements none), and depends on `liftoff-core`
//! only, so neither adapter depends on the other.
//!
//! SQLite asks the authorizer about every table, column, pragma and
//! function while it prepares a statement. For a module's statement it
//! allows, in the `main` database only:
//!
//! - reads, inserts, updates and deletes of the module's tables: the
//!   module's prefix and one lower-case word (`m_<module>_<word>`,
//!   [`ModuleName::owns_table`]), so `dns` never reaches the tables of
//!   `dns.cloudflare`;
//! - reads (only) of the tables a [`SharedTable`] of the caller's
//!   [`QueryRules`] gives one module (the daemon's: what a v1 import put aside for `tenants`);
//! - `CREATE`/`DROP TABLE` and `CREATE`/`DROP INDEX` there, and `ALTER
//!   TABLE` of such a table;
//! - the writes SQLite itself makes to `sqlite_master` while it runs those
//!   (a statement of the module cannot write it: `writable_schema` is a
//!   pragma, which is refused), and its reads of `sqlite_master` in a
//!   statement that changes the schema: SQLite runs nested statements on
//!   it for every `CREATE`, `DROP` and `ALTER`, and the authorizer cannot
//!   tell them from the module's SQL. None of those statements can carry
//!   a subquery of the module but `CREATE TABLE ... AS SELECT`, which is
//!   refused (create the table, then `INSERT ... SELECT`);
//! - `SELECT`, functions and recursive CTEs.
//!
//! Everything else is refused, and these explicitly because the authorizer
//! sees them as their own actions: `ATTACH`, `DETACH`, every `PRAGMA`
//! (reading ones too), transactions and savepoints (the connection is the
//! store's), temporary objects, views, triggers, virtual tables,
//! `REINDEX` of anything but its own indexes, `ANALYZE`, reads of `sqlite_master` (table names of the core
//! would leak) and `sqlite_sequence` (shared by every `AUTOINCREMENT`
//! table, so modules use `INTEGER PRIMARY KEY` without it).
//!
//! What the authorizer cannot see: `ALTER TABLE m_x_t RENAME TO users2`
//! is one `ALTER TABLE` action on `m_x_t`; the new name is not an
//! argument. So a statement that changes the schema runs inside a
//! savepoint, and when afterwards a table, index, view or trigger outside
//! the module's prefix appeared or disappeared, it is rolled back and
//! refused. The same check refuses a foreign key from a module's table to
//! any table that is not the module's (SQLite would then refuse the core's
//! deletes from its parent with a "foreign key mismatch"). Indexes SQLite
//! creates for a key of a module's table (`sqlite_autoindex_<table>_N`)
//! are the module's. One statement per call: SQL after the first `;` is
//! refused before anything runs.
//!
//! `VACUUM` never reaches the authorizer (and `VACUUM INTO` would write a
//! copy of the whole database to a file), so a statement whose first
//! keyword is `VACUUM` is refused before it is prepared.
//!
//! The savepoint is the outermost, so releasing it commits, and a
//! `DEFERRABLE INITIALLY DEFERRED` foreign key between the module's own
//! tables is checked only then. A release that fails leaves SQLite inside
//! the transaction; it is rolled back, and after every call the
//! connection is in autocommit again, or the core's next writes would
//! never be committed.
//!
//! Refusals never name the object, and SQLite's errors about a missing or
//! clashing name outside the module's prefix (and about any missing
//! column, whose table SQLite does not name) are answered the same way:
//! the answer does not tell which objects of the core exist.
//!
//! Limits ([`ModuleQueryLimits`]): the statement runs on the store's
//! connection, so a progress handler cuts it off when it runs longer than
//! its time, `SQLITE_LIMIT_LENGTH` bounds every value it computes,
//! `SQLITE_LIMIT_COLUMN` ([`MAX_MODULE_COLUMNS`]) the values of one row,
//! and an answer larger than the limit is refused, counted value by value
//! before each is converted; all are answered `validation`.

use core::time::Duration;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use liftoff_core::Error;
use liftoff_core::domain::ModuleName;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OptionalExtension as _, params, params_from_iter};
use serde_json::{Map, Number, Value as Json};

/// What one `store-query` of a module may use: it holds the store's
/// connection while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleQueryLimits {
    /// How long the statement may run before it is cut off.
    pub timeout: Duration,
    /// The largest answer (its JSON) and the largest value the statement
    /// may compute, in bytes.
    pub max_result_bytes: usize,
}

impl Default for ModuleQueryLimits {
    /// Two seconds and 4 MiB.
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(2),
            max_result_bytes: 4 * 1024 * 1024,
        }
    }
}

/// The most result columns (and columns of a table or index) a module's
/// statement may have (`SQLITE_LIMIT_COLUMN` while it runs): one row holds
/// at most this many values at once.
pub const MAX_MODULE_COLUMNS: usize = 64;

/// How many SQLite VM instructions run between two checks of the time.
const PROGRESS_STEPS: i32 = 1_000;
/// The table of every module's key space, as `CREATE TABLE`.
///
/// One row per module and key (`store-get`, `store-set`). A module's own
/// tables (`m_<module>_*`) are created by the module itself through
/// `store-query`.
pub const KEYS_SCHEMA: &str = "CREATE TABLE module_keys (\n    module TEXT NOT NULL,\n    key    TEXT NOT NULL,\n    value  BLOB NOT NULL,\n    PRIMARY KEY (module, key)\n) STRICT, WITHOUT ROWID;";

/// A table outside every module's prefix that one module may read (never
/// write) through `store-query`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedTable {
    /// The table, compared without regard to case.
    pub table: &'static str,
    /// The one module that may read it.
    pub reader: &'static str,
}

/// What one `store-query` is held to besides the module's prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryRules {
    /// Its time and size.
    pub limits: ModuleQueryLimits,
    /// The tables outside every prefix it may read.
    pub shared: &'static [SharedTable],
}

impl QueryRules {
    /// `limits`, and no table outside the module's prefix.
    #[must_use]
    pub const fn own_tables(limits: ModuleQueryLimits) -> Self {
        Self {
            limits,
            shared: &[],
        }
    }
}

/// `store-get`: the value of `key` in the key space of `module`.
///
/// # Errors
///
/// [`Error::Internal`] when SQLite fails.
pub fn get(conn: &Connection, module: &ModuleName, key: &str) -> Result<Option<Vec<u8>>, Error> {
    conn.prepare_cached("SELECT value FROM module_keys WHERE module = ?1 AND key = ?2")
        .and_then(|mut statement| {
            statement
                .query_row(params![module.as_str(), key], |row| row.get(0))
                .optional()
        })
        .map_err(internal)
}

/// `store-set`: `key` of `module` becomes `value`.
///
/// # Errors
///
/// [`Error::Internal`] when SQLite fails.
pub fn set(conn: &Connection, module: &ModuleName, key: &str, value: &[u8]) -> Result<(), Error> {
    conn.prepare_cached(
        "INSERT INTO module_keys (module, key, value) VALUES (?1, ?2, ?3) \
         ON CONFLICT (module, key) DO UPDATE SET value = excluded.value",
    )
    .and_then(|mut statement| statement.execute(params![module.as_str(), key, value]))
    .map(drop)
    .map_err(internal)
}

/// Drops every table of `module` and its keys (`remove --purge`, REG-07).
///
/// One transaction; their indexes go with them. Foreign keys between its
/// own tables are checked at commit (`defer_foreign_keys`), when none of
/// them is left, so the order of the drops does not matter.
///
/// # Errors
///
/// [`Error::Internal`] when SQLite fails.
pub fn purge(conn: &mut Connection, module: &ModuleName) -> Result<(), Error> {
    purge_tables(conn, module).map_err(internal)
}

/// A failure of SQLite itself, logged here: it never reaches the module.
fn internal(error: rusqlite::Error) -> Error {
    tracing::error!(%error, "module storage failure");
    Error::internal(error)
}

/// The drops and the delete of [`purge`].
fn purge_tables(conn: &mut Connection, module: &ModuleName) -> rusqlite::Result<()> {
    let transaction = conn.transaction()?;
    transaction.pragma_update(None, "defer_foreign_keys", true)?;
    let tables: Vec<String> = transaction
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?
        .into_iter()
        .filter(|table| module.owns_table(table))
        .collect();
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        transaction.execute_batch(&format!("DROP TABLE \"{quoted}\""))?;
    }
    let _deleted = transaction.execute(
        "DELETE FROM module_keys WHERE module = ?1",
        params![module.as_str()],
    )?;
    transaction.commit()
}

/// What the authorizer saw of one statement.
#[derive(Debug, Default)]
struct Seen {
    /// The first action it refused, for the error.
    refused: Option<String>,
    /// Whether the statement changes the schema (an allowed `CREATE`,
    /// `DROP` or `ALTER` was seen).
    changes_schema: bool,
    /// Whether it is a `CREATE TABLE`.
    creates_table: bool,
}

/// `store-query`: the one statement `sql` of `module`, with `params`.
///
/// It runs on `conn` behind the authorizer of the [crate
/// documentation](crate), held to `rules`; the answer is its rows as a
/// JSON array of objects (a blob as a base64 string).
///
/// `conn` must be in autocommit: the statement runs in the outermost
/// savepoint, and after the call the connection is in autocommit again.
///
/// # Errors
///
/// [`Error::Forbidden`] for a statement the rules refuse;
/// [`Error::Validation`] on `sql` for a statement SQLite refuses, more
/// than one statement, one cut off by its time or larger than its limit;
/// [`Error::Internal`] when SQLite itself fails.
pub fn query(
    conn: &Connection,
    module: &ModuleName,
    sql: &str,
    params: &[String],
    rules: QueryRules,
) -> Result<String, Error> {
    let QueryRules { limits, shared } = rules;
    precheck(conn, module, sql)?;
    let seen = Arc::new(Mutex::new(Seen::default()));
    let rule = {
        let seen = Arc::clone(&seen);
        let module = module.clone();
        move |context: AuthContext<'_>| {
            let mut seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
            let verdict = decide(&module, shared, &context, &seen);
            match verdict {
                Verdict::Allow { changes_schema } => {
                    seen.changes_schema |= changes_schema;
                    seen.creates_table |= matches!(context.action, AuthAction::CreateTable { .. });
                    Authorization::Allow
                }
                Verdict::Deny => {
                    if seen.refused.is_none() {
                        seen.refused = Some(describe(&context.action).to_owned());
                    }
                    Authorization::Deny
                }
            }
        }
    };
    // The savepoint and the schema snapshot are the store's, outside the
    // authorizer.
    let schema_before = outside_schema(conn, module)?;
    conn.execute_batch("SAVEPOINT module_query")
        .map_err(internal)?;
    let max_length = i32::try_from(limits.max_result_bytes).unwrap_or(i32::MAX);
    let max_columns = i32::try_from(MAX_MODULE_COLUMNS).unwrap_or(i32::MAX);
    let deadline = Instant::now().checked_add(limits.timeout);
    let installed = conn
        .authorizer(Some(rule))
        .and_then(|()| {
            conn.progress_handler(
                PROGRESS_STEPS,
                Some(move || deadline.is_none_or(|deadline| Instant::now() >= deadline)),
            )
        })
        .and_then(|()| {
            Ok((
                conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, max_length)?,
                conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, max_columns)?,
            ))
        });
    let ran = installed.map_err(internal).map(|before| {
        (
            before,
            run_statement(conn, module, sql, params, &seen, limits),
        )
    });
    // The authorizer, the progress handler and the limits are removed
    // before anything else runs on the connection, whatever
    // the statement did.
    let removed = conn
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .and_then(|()| conn.progress_handler(0, None::<fn() -> bool>))
        .and_then(|()| match &ran {
            Ok(((length, columns), _)) => conn
                .set_limit(Limit::SQLITE_LIMIT_LENGTH, *length)
                .and_then(|_| conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, *columns))
                .map(drop),
            Err(_) => Ok(()),
        });
    let changed_schema = seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .changes_schema;
    let outcome = removed
        .map_err(internal)
        .and(ran)
        .and_then(|(_, ran)| ran)
        .and_then(|rows| {
            if changed_schema {
                check_schema(conn, module, &schema_before)?;
            }
            Ok(rows)
        });
    finish(conn, module, outcome)
}

/// What is refused before the authorizer is installed: `VACUUM`, which
/// never reaches it (`VACUUM INTO` writes a copy of the whole database to
/// a file), and a second statement; and a connection that is not in
/// autocommit, where the savepoint would not be the outermost.
fn precheck(conn: &Connection, module: &ModuleName, sql: &str) -> Result<(), Error> {
    if first_keyword(sql).eq_ignore_ascii_case("vacuum") {
        return Err(Error::forbidden(format!(
            "store-query of {module}: VACUUM is not allowed"
        )));
    }
    if !conn.is_autocommit() {
        return Err(Error::internal_msg(
            "store-query started inside a transaction of the daemon",
        ));
    }
    // One statement per call, checked while the authorizer is off:
    // rusqlite tells a second statement apart by preparing it (which runs
    // nothing); behind the authorizer a refusal there would hide the
    // reason. Any other error is left to the authorized prepare.
    if matches!(
        conn.prepare(sql).map(drop),
        Err(rusqlite::Error::MultipleStatement)
    ) {
        return Err(sql_error(module, rusqlite::Error::MultipleStatement, None));
    }
    Ok(())
}

/// Ends the savepoint of a module's statement: released (committed) when
/// `outcome` is a success, rolled back otherwise. A release that fails
/// (the statement broke a `DEFERRABLE INITIALLY DEFERRED` foreign key,
/// checked at commit) leaves SQLite inside the transaction, so it is
/// rolled back too: the connection is in autocommit again however the
/// statement ended, or the store's next writes would never be committed.
fn finish(
    conn: &Connection,
    module: &ModuleName,
    outcome: Result<String, Error>,
) -> Result<String, Error> {
    let outcome = outcome.and_then(|rows| {
        conn.execute_batch("RELEASE module_query")
            .map(|()| rows)
            .map_err(|error| sql_error(module, error, None))
    });
    if !conn.is_autocommit()
        && let Err(error) = conn.execute_batch("ROLLBACK TO module_query; RELEASE module_query")
    {
        tracing::error!(%module, %error, "store-query could not roll back its savepoint");
        conn.execute_batch("ROLLBACK").map_err(internal)?;
    }
    if !conn.is_autocommit() {
        return Err(Error::internal_msg(
            "store-query left the connection inside a transaction",
        ));
    }
    outcome
}

/// The first keyword of `sql`, after whitespace and comments.
fn first_keyword(sql: &str) -> &str {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, next)| next);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, next)| next);
        } else {
            break;
        }
    }
    rest.split(|char: char| !char.is_ascii_alphanumeric() && char != '_')
        .next()
        .unwrap_or("")
}

/// Refuses a schema change of `module` that made or removed an object
/// that is not its own, or gave one of its tables a foreign key to a
/// table that is not its own; read while the authorizer is off.
fn check_schema(
    conn: &Connection,
    module: &ModuleName,
    before: &BTreeSet<(String, String)>,
) -> Result<(), Error> {
    if outside_schema(conn, module)? != *before {
        return Err(Error::forbidden(format!(
            "store-query of {module}: the statement changes the schema outside {}*",
            module.table_prefix()
        )));
    }
    if let Some((table, parent)) = foreign_key_outside(conn, module)? {
        return Err(Error::forbidden(format!(
            "store-query of {module}: {table} references {parent}, which is not its table"
        )));
    }
    Ok(())
}

/// The schema objects that are not `module`'s, by type and name; read
/// while the authorizer is off. An index SQLite made for a key of one of
/// its tables is the module's.
fn outside_schema(
    conn: &Connection,
    module: &ModuleName,
) -> Result<BTreeSet<(String, String)>, Error> {
    let mut statement = conn
        .prepare("SELECT type, name, tbl_name FROM sqlite_master")
        .map_err(internal)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(internal)?;
    let mut outside = BTreeSet::new();
    for row in rows {
        let (kind, name, table) = row.map_err(internal)?;
        let automatic = kind == "index" && is_automatic_index(&name);
        let own = module.owns_table(&name) || (automatic && module.owns_table(&table));
        if !own {
            outside.insert((kind, name));
        }
    }
    Ok(outside)
}

/// The first foreign key of one of `module`'s tables whose parent is not
/// its table, as (table, parent); read while the authorizer is off. SQLite
/// matches the parent's name without regard to case.
fn foreign_key_outside(
    conn: &Connection,
    module: &ModuleName,
) -> Result<Option<(String, String)>, Error> {
    let mut tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .map_err(internal)?;
    let names = tables
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    let mut keys = conn
        .prepare("SELECT \"table\" FROM pragma_foreign_key_list(?1)")
        .map_err(internal)?;
    for table in names.into_iter().filter(|name| module.owns_table(name)) {
        let parents = keys
            .query_map([&table], |row| row.get::<_, String>(0))
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;
        if let Some(parent) = parents
            .into_iter()
            .find(|parent| !module.owns_table(&parent.to_ascii_lowercase()))
        {
            return Ok(Some((table, parent)));
        }
    }
    Ok(None)
}

/// Prepares and runs one statement behind the authorizer; its rows as
/// JSON, at most `limits.max_result_bytes` of it.
fn run_statement(
    conn: &Connection,
    module: &ModuleName,
    sql: &str,
    params: &[String],
    seen: &Mutex<Seen>,
    limits: ModuleQueryLimits,
) -> Result<String, Error> {
    let refused = || {
        seen.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .refused
            .clone()
    };
    let mut statement = conn
        .prepare(sql)
        .map_err(|error| sql_error(module, error, refused()))?;
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut rows = statement
        .query(params_from_iter(params))
        .map_err(|error| sql_error(module, error, refused()))?;
    let mut out = b"[".to_vec();
    while let Some(row) = rows
        .next()
        .map_err(|error| sql_error(module, error, refused()))?
    {
        // Counted value by value before each is converted: one row of
        // many large values must not be built whole before the check.
        let mut object = Map::new();
        let mut size = out.len();
        for (index, name) in names.iter().enumerate() {
            let value = row
                .get_ref(index)
                .map_err(|error| sql_error(module, error, refused()))?;
            size = size
                .saturating_add(name.len())
                .saturating_add(json_size(value));
            if size >= limits.max_result_bytes {
                return Err(too_large(limits));
            }
            object.insert(name.clone(), json(value));
        }
        if out.len() > 1 {
            out.push(b',');
        }
        serde_json::to_writer(&mut out, &Json::Object(object)).map_err(Error::internal)?;
        if out.len() >= limits.max_result_bytes {
            return Err(too_large(limits));
        }
    }
    out.push(b']');
    String::from_utf8(out).map_err(Error::internal)
}

/// The error of an answer over `limits.max_result_bytes`.
fn too_large(limits: ModuleQueryLimits) -> Error {
    Error::invalid(
        "sql",
        format!(
            "the answer of store-query is larger than {} bytes",
            limits.max_result_bytes
        ),
    )
}

/// At least the size of `value` as [`json`] writes it, in bytes.
const fn json_size(value: ValueRef<'_>) -> usize {
    match value {
        ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => 4,
        ValueRef::Text(text) => text.len().saturating_add(2),
        ValueRef::Blob(blob) => blob.len().div_ceil(3).saturating_mul(4).saturating_add(2),
    }
}

/// A column value as JSON: `null`, a number, a string, or a blob as a
/// base64 string.
fn json(value: ValueRef<'_>) -> Json {
    match value {
        ValueRef::Null => Json::Null,
        ValueRef::Integer(int) => Json::Number(int.into()),
        ValueRef::Real(real) => Number::from_f64(real).map_or(Json::Null, Json::Number),
        ValueRef::Text(text) => Json::String(String::from_utf8_lossy(text).into_owned()),
        ValueRef::Blob(blob) => Json::String(BASE64.encode(blob)),
    }
}

/// The error a module gets for `error` from its statement.
fn sql_error(module: &ModuleName, error: rusqlite::Error, refused: Option<String>) -> Error {
    if matches!(error, rusqlite::Error::MultipleStatement) {
        return Error::invalid("sql", "store-query runs one statement per call".to_owned());
    }
    if let Some(what) = refused {
        return Error::forbidden(format!("store-query of {module}: {what} is not allowed"));
    }
    match &error {
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == ErrorCode::OperationInterrupted =>
        {
            Error::invalid(
                "sql",
                "store-query ran longer than its time and was cut off".to_owned(),
            )
        }
        rusqlite::Error::SqliteFailure(failure, _) if failure.code == ErrorCode::TooBig => {
            Error::invalid(
                "sql",
                "store-query computed a value larger than its limit".to_owned(),
            )
        }
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == ErrorCode::AuthorizationForStatementDenied =>
        {
            Error::forbidden(format!("store-query of {module}: not allowed"))
        }
        rusqlite::Error::SqliteFailure(failure, message)
            if matches!(
                failure.code,
                ErrorCode::Unknown | ErrorCode::ConstraintViolation | ErrorCode::TypeMismatch
            ) =>
        {
            let message = message.clone().unwrap_or_else(|| failure.to_string());
            module_message(module, message)
        }
        rusqlite::Error::SqlInputError { msg, .. } => module_message(module, msg.clone()),
        rusqlite::Error::InvalidParameterCount(..) | rusqlite::Error::InvalidParameterName(_) => {
            Error::invalid("sql", error.to_string())
        }
        _ => internal(error),
    }
}

/// What a refusal says about a name that is not the module's: the same
/// whether such a table, index or column exists or not.
const OUTSIDE: &str = "using a table, index or column that is not its own";

/// SQLite's `message` about a module's statement, as the module's error.
/// A message about a missing or existing name outside the module's prefix
/// would tell which objects of the core exist, so it is answered like an
/// authorizer's refusal; a missing column is, too, because SQLite does
/// not say of which table.
fn module_message(module: &ModuleName, message: String) -> Error {
    if names_outside(module, &message) {
        Error::forbidden(format!("store-query of {module}: {OUTSIDE} is not allowed"))
    } else {
        Error::invalid("sql", message)
    }
}

/// Whether SQLite's `message` names a schema object that is not
/// `module`'s, or a column.
fn names_outside(module: &ModuleName, message: &str) -> bool {
    if message.starts_with("no such column") {
        return true;
    }
    let named = [
        "no such table: ",
        "no such index: ",
        "no such view: ",
        "no such trigger: ",
        "there is already another table or index with this name: ",
    ]
    .iter()
    .find_map(|prefix| message.strip_prefix(prefix))
    .or_else(|| {
        message
            .strip_suffix(" already exists")
            .and_then(|kind_and_name| kind_and_name.split_once(' '))
            .map(|(_, name)| name)
    });
    named.is_some_and(|name| {
        let name = name.strip_prefix("main.").unwrap_or(name);
        !module.owns_table(name)
    })
}

/// The authorizer's answer about one action.
enum Verdict {
    Allow { changes_schema: bool },
    Deny,
}

/// The rule of the [module documentation](self) for one action.
fn decide(
    module: &ModuleName,
    shared: &[SharedTable],
    context: &AuthContext<'_>,
    seen: &Seen,
) -> Verdict {
    let own = |table: &str| module.owns_table(table);
    let main = matches!(context.database_name, None | Some("main"));
    let allow = |yes: bool| {
        if yes && main {
            Verdict::Allow {
                changes_schema: false,
            }
        } else {
            Verdict::Deny
        }
    };
    let schema = |yes: bool| {
        if yes && main {
            Verdict::Allow {
                changes_schema: true,
            }
        } else {
            Verdict::Deny
        }
    };
    match context.action {
        // `CREATE TABLE ... AS SELECT` is the one way the module's SQL
        // could select inside a statement that may read `sqlite_master`.
        AuthAction::Select => allow(!seen.creates_table),
        AuthAction::Recursive | AuthAction::Function { .. } => allow(true),
        // SQLite's own statements on the schema tables, `main` and `temp`
        // (see the module documentation).
        AuthAction::Read { table_name, .. }
            if seen.changes_schema && is_schema_table(table_name) =>
        {
            Verdict::Allow {
                changes_schema: false,
            }
        }
        AuthAction::Insert { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::Delete { table_name }
            if is_schema_table(table_name) =>
        {
            Verdict::Allow {
                changes_schema: false,
            }
        }
        // A shared table is readable by its one module, never writable
        // (the daemon's: what a v1 import put aside for `tenants`, which adopts `import_v1_tenants` on install).
        AuthAction::Read { table_name, .. } => {
            allow(own(table_name) || reads_shared(module, shared, table_name))
        }
        AuthAction::Insert { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::Delete { table_name } => allow(own(table_name)),
        AuthAction::CreateTable { table_name } | AuthAction::DropTable { table_name } => {
            schema(own(table_name))
        }
        AuthAction::CreateIndex {
            index_name,
            table_name,
        }
        | AuthAction::DropIndex {
            index_name,
            table_name,
        } => schema((own(index_name) || is_automatic_index(index_name)) && own(table_name)),
        AuthAction::AlterTable { table_name, .. } => schema(own(table_name)),
        // `CREATE INDEX` rebuilds the new index.
        AuthAction::Reindex { index_name } => allow(own(index_name)),
        // Everything else, explicitly among it what the authorizer sees as
        // an action of its own and a module must never do on the store's
        // connection: `ATTACH`, `DETACH`, `PRAGMA`, transactions and
        // savepoints, temporary objects, views, triggers, virtual tables,
        // `ANALYZE`, and actions this SQLite does not name.
        _ => Verdict::Deny,
    }
}

/// Whether `module` may read `table`, one of `shared`.
fn reads_shared(module: &ModuleName, shared: &[SharedTable], table: &str) -> bool {
    shared
        .iter()
        .any(|entry| table.eq_ignore_ascii_case(entry.table) && module.as_str() == entry.reader)
}

/// Whether `index` is one SQLite names itself for a key of a table
/// (`sqlite_autoindex_<table>_N`): names starting with `sqlite_` are
/// SQLite's own, a statement cannot give them to its objects.
fn is_automatic_index(index: &str) -> bool {
    index.starts_with("sqlite_autoindex_")
}

/// The schema tables of `main` and `temp` (and their newer names): SQLite
/// writes and reads them itself while it creates, alters or drops a table
/// of the module.
fn is_schema_table(table: &str) -> bool {
    [
        "sqlite_master",
        "sqlite_schema",
        "sqlite_temp_master",
        "sqlite_temp_schema",
    ]
    .iter()
    .any(|name| table.eq_ignore_ascii_case(name))
}

/// The refused action in words, for the module's error. It never names
/// the object: a refusal about a name outside the module's prefix reads
/// the same as SQLite's error about a missing one ([`OUTSIDE`]).
const fn describe(action: &AuthAction<'_>) -> &'static str {
    match action {
        AuthAction::Read { .. }
        | AuthAction::Insert { .. }
        | AuthAction::Update { .. }
        | AuthAction::Delete { .. }
        | AuthAction::CreateTable { .. }
        | AuthAction::DropTable { .. }
        | AuthAction::CreateIndex { .. }
        | AuthAction::DropIndex { .. }
        | AuthAction::AlterTable { .. }
        | AuthAction::Reindex { .. } => OUTSIDE,
        AuthAction::Attach { .. } => "ATTACH",
        AuthAction::Detach { .. } => "DETACH",
        AuthAction::Pragma { .. } => "PRAGMA",
        AuthAction::Transaction { .. } | AuthAction::Savepoint { .. } => "a transaction",
        AuthAction::CreateView { .. } | AuthAction::DropView { .. } => "a view",
        AuthAction::CreateTrigger { .. } | AuthAction::DropTrigger { .. } => "a trigger",
        AuthAction::CreateTempIndex { .. }
        | AuthAction::CreateTempTable { .. }
        | AuthAction::CreateTempTrigger { .. }
        | AuthAction::CreateTempView { .. }
        | AuthAction::DropTempIndex { .. }
        | AuthAction::DropTempTable { .. }
        | AuthAction::DropTempTrigger { .. }
        | AuthAction::DropTempView { .. } => "a temporary object",
        AuthAction::CreateVtable { .. } | AuthAction::DropVtable { .. } => "a virtual table",
        AuthAction::Analyze { .. } => "ANALYZE",
        _ => "this statement",
    }
}
