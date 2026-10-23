use crate::domain::ModuleName;
use crate::error::Error;

/// The storage a module reaches through its `host` imports: a key space of its own (`store-get`, `store-set`) and SQL over
/// its own tables (`store-query`).
///
/// Nothing a module sends reaches another module's keys or the core's
/// tables (MOD-01, MOD-02). `liftoff-store` implements it over the
/// daemon's SQLite database; `InMemoryStore` implements the key space only.
pub trait ModuleStore: Send + Sync {
    /// The value of `key` in `module`'s key space.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the store fails.
    fn module_get(
        &self,
        module: &ModuleName,
        key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send;

    /// Sets `key` in `module`'s key space to `value`, replacing it.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the store fails.
    fn module_set(
        &self,
        module: &ModuleName,
        key: &str,
        value: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Runs one SQL statement for `module` with `params` bound to `?1`,
    /// `?2`, ... and returns its rows as a JSON array of objects keyed by
    /// column name (`[{"name":"apple"}]`; `[]` for a statement without
    /// rows). Values are JSON `null`, numbers and strings; a blob is a
    /// base64 string.
    ///
    /// The statement may read, write, create and drop only the module's
    /// tables ([`ModuleName::owns_table`]); any other table (the core's,
    /// another module's, `sqlite_master`), a foreign key to one, `ATTACH`,
    /// `DETACH`, `PRAGMA` and every other operation is refused (MOD-02).
    /// Its time and the size of its answer are bounded.
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] for a statement that touches anything outside
    ///   the module's tables;
    /// - [`Error::Validation`] for SQL that does not parse, more than one
    ///   statement, or a statement over its time or size limit;
    /// - [`Error::Internal`] when the store fails.
    fn module_query(
        &self,
        module: &ModuleName,
        sql: &str,
        params: &[String],
    ) -> impl Future<Output = Result<String, Error>> + Send;

    /// Drops everything `module` stored: every table it owns
    /// ([`ModuleName::owns_table`]) and its whole key space; another
    /// module's tables and keys and the core's tables stay
    /// (`remove --purge`, REG-07). Nothing when it stored nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the store fails.
    fn module_purge(&self, module: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send;
}
