//! [`LocalModuleStore`]: the module tables of the CLI's module host
//! (`~/.config/liftoff/modules.db`).
//!
//! The same rules as the daemon's store (MOD-01, MOD-02): a module's key
//! space and its own tables only, `store-query` behind the one SQL
//! authorizer the daemon's store uses too (`liftoff-module-sql`). Unlike
//! the daemon's database it holds nothing but modules' storage: no table
//! of the core, and no table outside every prefix that a module may read.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use liftoff_core::Error;
use liftoff_core::domain::ModuleName;
use liftoff_core::ports::ModuleStore;
use liftoff_module_sql::{KEYS_SCHEMA, ModuleQueryLimits, QueryRules};
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

use super::files::private_file;

/// The schema of `modules.db`, in the order it is applied. Append; never
/// edit a shipped one.
fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(KEYS_SCHEMA)])
}

/// The module storage of local mode: one SQLite file. Clones share the
/// connection.
#[derive(Debug, Clone)]
pub struct LocalModuleStore {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl LocalModuleStore {
    /// Opens (or creates, readable by its owner only) the store at `path`
    /// and migrates it.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the file cannot be opened or migrated.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        private_file(&path)?;
        let mut conn = Connection::open(&path).map_err(internal)?;
        conn.pragma_update(None, "busy_timeout", 5_000)
            .map_err(internal)?;
        migrations().to_latest(&mut conn).map_err(Error::internal)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs `work` on a blocking thread with the connection, as the
    /// daemon's store does: a statement of a module may run for up to its
    /// time limit.
    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(PoisonError::into_inner);
            work(&mut guard)
        })
        .await
        .map_err(Error::internal)?
    }
}

/// A failure of SQLite itself.
fn internal(error: rusqlite::Error) -> Error {
    Error::internal(error)
}

impl ModuleStore for LocalModuleStore {
    async fn module_get(&self, module: &ModuleName, key: &str) -> Result<Option<Vec<u8>>, Error> {
        let module = module.clone();
        let key = key.to_owned();
        self.run(move |conn| liftoff_module_sql::get(conn, &module, &key))
            .await
    }

    async fn module_set(&self, module: &ModuleName, key: &str, value: &[u8]) -> Result<(), Error> {
        let module = module.clone();
        let key = key.to_owned();
        let value = value.to_vec();
        self.run(move |conn| liftoff_module_sql::set(conn, &module, &key, &value))
            .await
    }

    async fn module_query(
        &self,
        module: &ModuleName,
        sql: &str,
        params: &[String],
    ) -> Result<String, Error> {
        let module = module.clone();
        let sql = sql.to_owned();
        let params = params.to_vec();
        let rules = QueryRules::own_tables(ModuleQueryLimits::default());
        self.run(move |conn| liftoff_module_sql::query(conn, &module, &sql, &params, rules))
            .await
    }

    async fn module_purge(&self, module: &ModuleName) -> Result<(), Error> {
        let module = module.clone();
        self.run(move |conn| liftoff_module_sql::purge(conn, &module))
            .await
    }
}
