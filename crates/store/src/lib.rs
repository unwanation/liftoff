//! SQLite implementation of the `Store` port from `liftoff-core`.
//!
//! Owns the v2 schema and its migrations; nothing outside this crate
//! speaks SQL. [`SqliteStore`] opens the database in WAL mode with foreign
//! keys on, keeps one connection behind a mutex and runs every call on a
//! blocking thread (`spawn_blocking`): SQLite lives on the same host as
//! the daemon, so a pool would buy nothing. SQLite constraint violations
//! are mapped to the variants `InMemoryStore` uses (unique key taken →
//! `Conflict`, missing parent → `NotFound`, row still referenced →
//! `Conflict`); anything else is `Internal`.

mod blobs;
mod import_v1;
mod module_store;
mod rows;
mod sql;

pub use blobs::{FileBlobs, LinkMode};
pub use import_v1::{
    ImportReport, ImportedDomain, ImportedMember, ImportedTenant, Renamed, TenantRole, V1Import,
    import_v1,
};
pub use liftoff_module_sql::{MAX_MODULE_COLUMNS, ModuleQueryLimits};

use core::ops::RangeInclusive;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use liftoff_core::Error;
use liftoff_core::domain::{
    Artifact, ArtifactId, ArtifactVersion, Audit, Binding, BindingId, Branch, CachedTemplate,
    Deploy, DeployId, DeployNumber, DeployStatus, DeployToken, DeployTokenId, Domain, DomainId,
    EnvName, Environment, EnvironmentId, GateCode, GateSession, GitConnection, Host, HostPort,
    InstalledModule, Login, ModuleName, OneTimeLink, Project, ProjectId, ProjectName, RepoRef,
    ScopeKind, Secret, SecretId, SecretKey, SecretScope, SecretSyncRecord, SecretVersion, Session,
    SessionId, SetupToken, SyncTarget, TenantId, Timestamp, TokenHash, User, UserId, UserSource,
    VersionNumber,
};
use liftoff_core::ports::Store;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension as _, Transaction};
use rusqlite_migration::{M, Migrations};

use crate::rows::{
    ArtifactFileRow, ArtifactVersionRow, artifact_version_records, scope_args, version_record,
};
use crate::sql::{
    FromRow, Record, ToRecord, blob, delete_error, internal, opt_time, time, timestamp,
};

/// The schema, in the order it is applied. Append; never edit a shipped one.
fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("../migrations/0001_init.sql")),
        M::up(include_str!("../migrations/0002_settings.sql")),
        M::up(include_str!("../migrations/0003_import_v1.sql")),
        M::up(include_str!("../migrations/0004_module_keys.sql")),
        M::up(include_str!("../migrations/0005_modules.sql")),
        M::up(include_str!("../migrations/0006_users_subject.sql")),
        M::up(include_str!("../migrations/0007_gate.sql")),
        M::up(include_str!("../migrations/0008_template_cache.sql")),
        M::up(include_str!("../migrations/0009_secret_syncs.sql")),
        M::up(include_str!("../migrations/0010_links_kv_components.sql")),
        M::up(include_str!("../migrations/0011_artifacts.sql")),
        M::up(include_str!("../migrations/0012_artifact_blobs.sql")),
    ])
}

/// Refuses the liftoff v1 database at `path` before anything touches it.
///
/// v1 ends at `user_version = 3` as well, so the migrations alone would
/// take it for an up-to-date v2 database; its `targets` table, which v2
/// does not have, tells them apart.
fn refuse_v1(conn: &Connection, path: &Path) -> Result<(), Error> {
    let v1: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'targets')",
            [],
            |row| row.get(0),
        )
        .map_err(internal)?;
    if v1 {
        return Err(Error::internal_msg(format!(
            "{} is a liftoff v1 database: v2 does not open it; move it aside and import it \
             into a new database with `liftoffd setup --import <file>`",
            path.display()
        )));
    }
    Ok(())
}

/// The [`Store`] over a SQLite database in WAL mode. Clones share the
/// connection.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
    module_limits: ModuleQueryLimits,
}

impl SqliteStore {
    /// Opens (creating it if needed) the database at `path` in WAL mode
    /// with foreign keys on, and migrates it to the latest schema. Opening
    /// an up-to-date database changes nothing.
    ///
    /// Blocks while the file is opened and migrated; call it at startup.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the file cannot be opened, is not a
    /// database, is a liftoff v1 database (left as it is: import it with
    /// `liftoffd setup --import`), or a migration fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let conn = Connection::open(path.as_ref()).map_err(internal)?;
        refuse_v1(&conn, path.as_ref())?;
        let mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(internal)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::internal_msg(format!(
                "database did not enter WAL mode (journal_mode = {mode})"
            )));
        }
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(internal)?;
        Self::prepare(conn)
    }

    /// A private in-memory database, migrated like a file (for tests).
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when SQLite or a migration fails.
    pub fn open_in_memory() -> Result<Self, Error> {
        Self::prepare(Connection::open_in_memory().map_err(internal)?)
    }

    fn prepare(mut conn: Connection) -> Result<Self, Error> {
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(internal)?;
        conn.pragma_update(None, "busy_timeout", 5_000)
            .map_err(internal)?;
        migrations().to_latest(&mut conn).map_err(Error::internal)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            module_limits: ModuleQueryLimits::default(),
        })
    }

    /// The store with `limits` on every `store-query` of a module (the
    /// default is [`ModuleQueryLimits::default`]).
    #[must_use]
    pub const fn with_module_query_limits(mut self, limits: ModuleQueryLimits) -> Self {
        self.module_limits = limits;
        self
    }

    /// `SELECT 1` on the connection: the readiness probe's question.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the database does not answer.
    pub async fn ping(&self) -> Result<(), Error> {
        self.run(|conn| {
            conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .map(drop)
                .map_err(internal)
        })
        .await
    }

    /// Runs `work` on a blocking thread with the connection.
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

/// Row helpers the `Store` implementation is built on.
impl SqliteStore {
    async fn find<T: FromRow + Send + 'static>(
        &self,
        sql: &'static str,
        args: Vec<Value>,
    ) -> Result<Option<T>, Error> {
        self.run(move |conn| sql::find(conn, sql, &args).map_err(internal))
            .await
    }

    async fn list<T: FromRow + Send + 'static>(
        &self,
        sql: &'static str,
        args: Vec<Value>,
    ) -> Result<Vec<T>, Error> {
        self.run(move |conn| sql::list(conn, sql, &args).map_err(internal))
            .await
    }

    /// Inserts a row; `parent` names what a foreign-key failure means.
    async fn insert(&self, record: Record, parent: &'static str) -> Result<(), Error> {
        self.run(move |conn| {
            record
                .insert(conn)
                .map_err(|error| sql::write_error(error, record.what, parent))
        })
        .await
    }

    /// Replaces a row by its key.
    async fn update(&self, record: Record, parent: &'static str) -> Result<(), Error> {
        self.run(move |conn| match record.update(conn) {
            Ok(0) => Err(Error::not_found(record.what)),
            Ok(_) => Ok(()),
            Err(error) => Err(sql::write_error(error, record.what, parent)),
        })
        .await
    }

    /// Deletes the row of `table` with this key; `in_use` is the conflict
    /// message when other rows still reference it.
    async fn delete(
        &self,
        sql: &'static str,
        what: &'static str,
        id: impl ToString,
        in_use: &'static str,
    ) -> Result<(), Error> {
        let id = id.to_string();
        self.run(move |conn| match conn.execute(sql, [id]) {
            Ok(0) => Err(Error::not_found(what)),
            Ok(_) => Ok(()),
            Err(error) => Err(delete_error(error, in_use)),
        })
        .await
    }
}

/// The `settings` key of the live setup token.
const SETUP_TOKEN_KEY: &str = "setup_token";

/// Reads the `<user id>:<hex hash>` value of the `setup_token` setting.
fn setup_token(value: &str, created_at: Timestamp) -> Result<SetupToken, Error> {
    let (user, hash) = value
        .split_once(':')
        .ok_or_else(|| Error::internal_msg("corrupt setup_token setting"))?;
    Ok(SetupToken {
        token_hash: TokenHash::from_hex(hash).map_err(Error::internal)?,
        user_id: UserId::parse(user).map_err(Error::internal)?,
        created_at,
    })
}

fn arg(value: &impl ToString) -> Value {
    Value::Text(value.to_string())
}

fn limit(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// Whether the scope a secret points at exists (`scope_id` is not a
/// foreign key, so the store checks it). A scope kind this store has no
/// table for is an internal error, not a lookup in some other table.
fn scope_exists(tx: &Transaction<'_>, scope: &SecretScope) -> Result<bool, Error> {
    let sql = match scope.kind() {
        // The tenants live in the `tenants` module's tables: the core
        // knows a tenant by the projects in it.
        ScopeKind::Tenant => "SELECT 1 FROM projects WHERE tenant_id = ? LIMIT 1",
        ScopeKind::Project => "SELECT 1 FROM projects WHERE id = ?",
        ScopeKind::Environment => "SELECT 1 FROM environments WHERE id = ?",
        other => {
            return Err(Error::internal_msg(format!(
                "unsupported secret scope {}",
                other.as_str()
            )));
        }
    };
    tx.query_row(sql, [scope.id().to_string()], |_| Ok(()))
        .optional()
        .map(|found| found.is_some())
        .map_err(internal)
}

/// The versions of artifact `id` with their files, oldest first; with
/// `number`, that version alone.
fn read_artifact_versions(
    conn: &Connection,
    id: ArtifactId,
    number: Option<VersionNumber>,
) -> rusqlite::Result<Vec<ArtifactVersion>> {
    let args = [
        arg(&id),
        number.map_or(Value::Null, |number| {
            Value::Integer(i64::from(number.get()))
        }),
    ];
    let versions_sql = "SELECT * FROM artifact_versions \
                        WHERE artifact_id = ?1 AND (?2 IS NULL OR number = ?2) ORDER BY number";
    let files_sql = "SELECT * FROM artifact_files \
                     WHERE artifact_id = ?1 AND (?2 IS NULL OR number = ?2) \
                     ORDER BY number, position";
    let mut versions: Vec<ArtifactVersion> =
        sql::list::<ArtifactVersionRow>(conn, versions_sql, &args)?
            .into_iter()
            .map(|row| row.0)
            .collect();
    for ArtifactFileRow(number, file) in sql::list::<ArtifactFileRow>(conn, files_sql, &args)? {
        if let Some(version) = versions.iter_mut().find(|version| version.number == number) {
            version.files.push(file);
        }
    }
    Ok(versions)
}

/// Inserts a version and its files in `tx`.
fn insert_artifact_version(tx: &Transaction<'_>, version: &ArtifactVersion) -> Result<(), Error> {
    let (row, files) = artifact_version_records(version)?;
    row.insert(tx)
        .map_err(|error| sql::write_error(error, row.what, "artifact or user"))?;
    for file in files {
        file.insert(tx)
            .map_err(|error| sql::write_error(error, file.what, "artifact version"))?;
    }
    Ok(())
}

/// The current version of artifact `id` in `tx`.
fn current_artifact_version(tx: &Transaction<'_>, id: ArtifactId) -> Result<u32, Error> {
    tx.query_row(
        "SELECT current_version FROM artifacts WHERE id = ?",
        [id.to_string()],
        |row| row.get::<_, u32>(0),
    )
    .optional()
    .map_err(internal)?
    .ok_or_else(|| Error::not_found("artifact"))
}

impl Store for SqliteStore {
    // ---- artifacts -------------------------------------

    async fn insert_artifact(
        &self,
        artifact: &Artifact,
        first: &ArtifactVersion,
    ) -> Result<(), Error> {
        let record = artifact.record()?;
        let first = first.clone();
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            record
                .insert(&tx)
                .map_err(|error| sql::write_error(error, record.what, "user"))?;
            insert_artifact_version(&tx, &first)?;
            tx.commit().map_err(internal)
        })
        .await
    }

    async fn add_artifact_version(
        &self,
        artifact: &Artifact,
        version: &ArtifactVersion,
    ) -> Result<(), Error> {
        let record = artifact.record()?;
        let version = version.clone();
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            insert_artifact_version(&tx, &version)?;
            match record.update(&tx) {
                Ok(0) => return Err(Error::not_found(record.what)),
                Ok(_) => {}
                Err(error) => return Err(sql::write_error(error, record.what, "user")),
            }
            tx.commit().map_err(internal)
        })
        .await
    }

    async fn update_artifact(&self, artifact: &Artifact) -> Result<(), Error> {
        self.update(artifact.record()?, "user").await
    }

    async fn artifact(&self, id: ArtifactId) -> Result<Option<Artifact>, Error> {
        self.find("SELECT * FROM artifacts WHERE id = ?", vec![arg(&id)])
            .await
    }

    async fn artifacts(&self, owner: Option<UserId>) -> Result<Vec<Artifact>, Error> {
        match owner {
            Some(owner) => {
                self.list(
                    "SELECT * FROM artifacts WHERE owner_user_id = ? \
                     ORDER BY julianday(created_at) DESC, id DESC",
                    vec![arg(&owner)],
                )
                .await
            }
            None => {
                self.list(
                    "SELECT * FROM artifacts ORDER BY julianday(created_at) DESC, id DESC",
                    Vec::new(),
                )
                .await
            }
        }
    }

    async fn artifact_versions(&self, id: ArtifactId) -> Result<Vec<ArtifactVersion>, Error> {
        self.run(move |conn| read_artifact_versions(conn, id, None).map_err(internal))
            .await
    }

    async fn artifact_version(
        &self,
        id: ArtifactId,
        number: VersionNumber,
    ) -> Result<Option<ArtifactVersion>, Error> {
        self.run(move |conn| {
            read_artifact_versions(conn, id, Some(number))
                .map(|versions| versions.into_iter().next())
                .map_err(internal)
        })
        .await
    }

    async fn delete_artifact_version(
        &self,
        id: ArtifactId,
        number: VersionNumber,
    ) -> Result<(), Error> {
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            if current_artifact_version(&tx, id)? == number.get() {
                return Err(Error::conflict(format!(
                    "version {number} is the current version of artifact {id}"
                )));
            }
            let deleted = tx
                .execute(
                    "DELETE FROM artifact_versions WHERE artifact_id = ? AND number = ?",
                    rusqlite::params![id.to_string(), number.get()],
                )
                .map_err(internal)?;
            if deleted == 0 {
                return Err(Error::not_found("artifact version"));
            }
            tx.commit().map_err(internal)
        })
        .await
    }

    async fn prune_artifact_versions(
        &self,
        id: ArtifactId,
        keep: usize,
    ) -> Result<Vec<VersionNumber>, Error> {
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            let current = current_artifact_version(&tx, id)?;
            let doomed: Vec<u32> = {
                let mut statement = tx
                    .prepare_cached(
                        "SELECT number FROM artifact_versions WHERE artifact_id = ?1 \
                         AND number <> ?2 AND number NOT IN \
                           (SELECT number FROM artifact_versions WHERE artifact_id = ?1 \
                            ORDER BY number DESC LIMIT ?3) \
                         ORDER BY number",
                    )
                    .map_err(internal)?;
                let rows = statement
                    .query_map(
                        rusqlite::params![id.to_string(), current, limit(keep)],
                        |row| row.get::<_, u32>(0),
                    )
                    .map_err(internal)?;
                rows.collect::<rusqlite::Result<_>>().map_err(internal)?
            };
            let mut pruned = Vec::with_capacity(doomed.len());
            for number in doomed {
                tx.execute(
                    "DELETE FROM artifact_versions WHERE artifact_id = ? AND number = ?",
                    rusqlite::params![id.to_string(), number],
                )
                .map_err(internal)?;
                pruned.push(VersionNumber::new(number).map_err(Error::internal)?);
            }
            tx.commit().map_err(internal)?;
            Ok(pruned)
        })
        .await
    }

    async fn delete_artifact(&self, id: ArtifactId) -> Result<(), Error> {
        self.delete(
            "DELETE FROM artifacts WHERE id = ?",
            "artifact",
            id,
            "artifact in use",
        )
        .await
    }

    async fn expired_artifacts(&self, now: Timestamp) -> Result<Vec<Artifact>, Error> {
        self.list(
            "SELECT * FROM artifacts WHERE visibility = 'private' AND expires_at IS NOT NULL \
             AND julianday(expires_at) <= julianday(?) \
             ORDER BY julianday(expires_at), id",
            vec![time(now)?],
        )
        .await
    }

    // ---- users -------------------------------------------------------------

    async fn insert_user(&self, user: &User) -> Result<(), Error> {
        self.insert(user.record()?, "user").await
    }

    async fn update_user(&self, user: &User) -> Result<(), Error> {
        self.update(user.record()?, "user").await
    }

    async fn user(&self, id: UserId) -> Result<Option<User>, Error> {
        self.find("SELECT * FROM users WHERE id = ?", vec![arg(&id)])
            .await
    }

    async fn user_by_login(&self, login: &Login) -> Result<Option<User>, Error> {
        self.find("SELECT * FROM users WHERE login = ?", vec![arg(login)])
            .await
    }

    async fn user_by_subject(
        &self,
        source: &UserSource,
        subject: &str,
    ) -> Result<Option<User>, Error> {
        self.find(
            "SELECT * FROM users WHERE source = ? AND subject = ?",
            vec![arg(&source.as_string()), arg(&subject)],
        )
        .await
    }

    async fn users(&self) -> Result<Vec<User>, Error> {
        self.list("SELECT * FROM users ORDER BY login", Vec::new())
            .await
    }

    // ---- sessions ------------------------------------------------------------

    async fn insert_session(&self, session: &Session) -> Result<(), Error> {
        self.insert(session.record()?, "user").await
    }

    async fn update_session(&self, session: &Session) -> Result<(), Error> {
        self.update(session.record()?, "user").await
    }

    async fn session_by_hash(&self, hash: &TokenHash) -> Result<Option<Session>, Error> {
        self.find(
            "SELECT * FROM sessions WHERE token_hash = ?",
            vec![blob(hash)],
        )
        .await
    }

    async fn sessions_of_user(&self, user: UserId) -> Result<Vec<Session>, Error> {
        self.list(
            "SELECT * FROM sessions WHERE user_id = ? ORDER BY id",
            vec![arg(&user)],
        )
        .await
    }

    async fn delete_session(&self, id: SessionId) -> Result<(), Error> {
        self.delete(
            "DELETE FROM sessions WHERE id = ?",
            "session",
            id,
            "session in use",
        )
        .await
    }

    async fn delete_sessions_of_user(&self, user: UserId) -> Result<usize, Error> {
        let user = user.to_string();
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            // An unredeemed one-time link is a session to come: it goes
            // with the sessions (OTL-02).
            tx.execute("DELETE FROM one_time_links WHERE user_id = ?", [&user])
                .map_err(internal)?;
            let deleted = tx
                .execute("DELETE FROM sessions WHERE user_id = ?", [&user])
                .map_err(internal)?;
            tx.commit().map_err(internal)?;
            Ok(deleted)
        })
        .await
    }

    // ---- gate ----------------------------------------------------------------

    async fn insert_gate_code(&self, code: &GateCode) -> Result<(), Error> {
        self.insert(code.record()?, "session").await
    }

    async fn take_gate_code(&self, hash: &TokenHash) -> Result<Option<GateCode>, Error> {
        self.find(
            "DELETE FROM gate_codes WHERE code_hash = ? RETURNING *",
            vec![blob(hash)],
        )
        .await
    }

    async fn insert_gate_session(&self, session: &GateSession) -> Result<(), Error> {
        self.insert(session.record()?, "session").await
    }

    async fn gate_session(&self, hash: &TokenHash) -> Result<Option<GateSession>, Error> {
        self.find(
            "SELECT * FROM gate_sessions WHERE token_hash = ?",
            vec![blob(hash)],
        )
        .await
    }

    async fn prune_gate_records(&self, now: Timestamp) -> Result<(usize, usize), Error> {
        let now = time(now)?;
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            // The stored times are RFC 3339 with any number of fractional
            // digits, which do not sort as text; `julianday` reads them.
            let codes = tx
                .execute(
                    "DELETE FROM gate_codes WHERE julianday(expires_at) <= julianday(?)",
                    [&now],
                )
                .map_err(internal)?;
            let sessions = tx
                .execute(
                    "DELETE FROM gate_sessions WHERE julianday(expires_at) <= julianday(?)",
                    [&now],
                )
                .map_err(internal)?;
            tx.commit().map_err(internal)?;
            Ok((codes, sessions))
        })
        .await
    }

    // ---- one-time links -------------------------------

    async fn insert_one_time_link(&self, link: &OneTimeLink) -> Result<(), Error> {
        self.insert(link.record()?, "user").await
    }

    async fn one_time_link(&self, hash: &TokenHash) -> Result<Option<OneTimeLink>, Error> {
        self.find(
            "SELECT * FROM one_time_links WHERE token_hash = ?",
            vec![blob(hash)],
        )
        .await
    }

    async fn use_one_time_link(
        &self,
        hash: &TokenHash,
        at: Timestamp,
    ) -> Result<Option<OneTimeLink>, Error> {
        // One statement: of two uses only the first finds `used_at` empty.
        self.find(
            "UPDATE one_time_links SET used_at = ? \
             WHERE token_hash = ? AND used_at IS NULL RETURNING *",
            vec![time(at)?, blob(hash)],
        )
        .await
    }

    async fn prune_one_time_links(&self, now: Timestamp) -> Result<usize, Error> {
        let now = time(now)?;
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM one_time_links WHERE julianday(expires_at) <= julianday(?)",
                [&now],
            )
            .map_err(internal)
        })
        .await
    }

    // ---- setup token (settings) ---------------------------------------------

    async fn put_setup_token(&self, token: &SetupToken) -> Result<(), Error> {
        let user = token.user_id.to_string();
        let value = format!("{user}:{}", token.token_hash);
        let minted = time(token.created_at)?;
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            let known = tx
                .query_row("SELECT 1 FROM users WHERE id = ?", [&user], |_| Ok(()))
                .optional()
                .map_err(internal)?;
            if known.is_none() {
                return Err(Error::not_found("user"));
            }
            tx.execute(
                "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (key) DO UPDATE SET value = ?2, updated_at = ?3",
                rusqlite::params![SETUP_TOKEN_KEY, value, minted],
            )
            .map_err(internal)?;
            tx.commit().map_err(internal)
        })
        .await
    }

    async fn setup_token(&self, hash: &TokenHash) -> Result<Option<SetupToken>, Error> {
        let hash = *hash;
        self.run(move |conn| Ok(live_setup_token(conn)?.filter(|token| token.token_hash == hash)))
            .await
    }

    async fn exchange_setup_token(
        &self,
        hash: &TokenHash,
        session: &Session,
    ) -> Result<Option<SetupToken>, Error> {
        let hash = *hash;
        let user = session.user_id;
        let record = session.record()?;
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            let Some(token) = live_setup_token(&tx)?
                .filter(|token| token.token_hash == hash && token.user_id == user)
            else {
                return Ok(None);
            };
            tx.execute("DELETE FROM settings WHERE key = ?", [SETUP_TOKEN_KEY])
                .map_err(internal)?;
            record
                .insert(&tx)
                .map_err(|error| sql::write_error(error, record.what, "user"))?;
            tx.commit().map_err(internal)?;
            Ok(Some(token))
        })
        .await
    }

    // ---- git connections -----------------------------------------------------

    async fn put_git_connection(&self, connection: &GitConnection) -> Result<(), Error> {
        let record = connection.record()?;
        self.run(move |conn| {
            let names: Vec<&str> = record.columns.iter().map(|(name, _)| *name).collect();
            let marks = vec!["?"; names.len()].join(", ");
            let updates: Vec<String> = names
                .iter()
                .map(|name| format!("{name} = excluded.{name}"))
                .collect();
            let sql = format!(
                "INSERT INTO git_connections ({}) VALUES ({marks}) \
                 ON CONFLICT (user_id, host) DO UPDATE SET {}",
                names.join(", "),
                updates.join(", ")
            );
            conn.prepare_cached(&sql)
                .and_then(|mut statement| {
                    statement.execute(rusqlite::params_from_iter(
                        record.columns.iter().map(|(_, value)| value),
                    ))
                })
                .map(drop)
                .map_err(|error| sql::write_error(error, record.what, "user"))
        })
        .await
    }

    async fn git_connection(
        &self,
        user: UserId,
        host: &Host,
    ) -> Result<Option<GitConnection>, Error> {
        self.find(
            "SELECT * FROM git_connections WHERE user_id = ? AND host = ?",
            vec![arg(&user), arg(host)],
        )
        .await
    }

    async fn git_connections_of_user(&self, user: UserId) -> Result<Vec<GitConnection>, Error> {
        self.list(
            "SELECT * FROM git_connections WHERE user_id = ? ORDER BY host",
            vec![arg(&user)],
        )
        .await
    }

    async fn delete_git_connection(&self, user: UserId, host: &Host) -> Result<(), Error> {
        let args = [user.to_string(), host.to_string()];
        self.run(move |conn| {
            match conn.execute(
                "DELETE FROM git_connections WHERE user_id = ? AND host = ?",
                args,
            ) {
                Ok(0) => Err(Error::not_found("git connection")),
                Ok(_) => Ok(()),
                Err(error) => Err(internal(error)),
            }
        })
        .await
    }

    // ---- projects ------------------------------------------------------------

    async fn insert_project(&self, project: &Project) -> Result<(), Error> {
        self.insert(project.record()?, "user").await
    }

    async fn update_project(&self, project: &Project) -> Result<(), Error> {
        self.update(project.record()?, "user").await
    }

    async fn project(&self, id: ProjectId) -> Result<Option<Project>, Error> {
        self.find("SELECT * FROM projects WHERE id = ?", vec![arg(&id)])
            .await
    }

    async fn project_by_name(
        &self,
        tenant: Option<TenantId>,
        name: &ProjectName,
    ) -> Result<Option<Project>, Error> {
        self.find(
            "SELECT * FROM projects WHERE tenant_id IS ? AND name = ?",
            vec![sql::opt_text(tenant), arg(name)],
        )
        .await
    }

    async fn project_by_repo(
        &self,
        git_host: &Host,
        repo: &RepoRef,
    ) -> Result<Option<Project>, Error> {
        self.find(
            "SELECT * FROM projects WHERE git_host = ? AND repo = ?",
            vec![arg(git_host), arg(repo)],
        )
        .await
    }

    async fn projects(&self) -> Result<Vec<Project>, Error> {
        self.list("SELECT * FROM projects ORDER BY name, id", Vec::new())
            .await
    }

    async fn delete_project(&self, id: ProjectId) -> Result<(), Error> {
        let id = id.to_string();
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            // Environments and domains restrict; the project's secrets
            // point at their scope without a foreign key, and their
            // versions cascade from them.
            let _secrets: usize = tx
                .execute(
                    "DELETE FROM secrets WHERE scope = 'project' AND scope_id = ?",
                    [&id],
                )
                .map_err(internal)?;
            let _tokens: usize = tx
                .execute("DELETE FROM deploy_tokens WHERE project_id = ?", [&id])
                .map_err(internal)?;
            match tx.execute("DELETE FROM projects WHERE id = ?", [&id]) {
                Ok(0) => return Err(Error::not_found("project")),
                Ok(_) => {}
                Err(error) => {
                    return Err(delete_error(error, "project has environments or domains"));
                }
            }
            tx.commit().map_err(internal)
        })
        .await
    }

    // ---- environments --------------------------------------------------------

    async fn insert_environment(&self, environment: &Environment) -> Result<(), Error> {
        self.insert(environment.record()?, "project or user").await
    }

    async fn update_environment(&self, environment: &Environment) -> Result<(), Error> {
        let mut record = environment.record()?;
        // `protected` is `set_environment_protected`'s alone.
        record.columns.retain(|(column, _)| *column != "protected");
        self.update(record, "project or user").await
    }

    async fn set_environment_protected(
        &self,
        id: EnvironmentId,
        protected: bool,
    ) -> Result<(), Error> {
        let id = id.to_string();
        self.run(move |conn| {
            match conn.execute(
                "UPDATE environments SET protected = ? WHERE id = ?",
                rusqlite::params![protected, id],
            ) {
                Ok(0) => Err(Error::not_found("environment")),
                Ok(_) => Ok(()),
                Err(error) => Err(internal(error)),
            }
        })
        .await
    }

    async fn delete_environment(&self, id: EnvironmentId) -> Result<(), Error> {
        let id = id.to_string();
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            // Bindings and deploys cascade; secrets point at their scope
            // without a foreign key, and their versions cascade from them.
            let _secrets: usize = tx
                .execute(
                    "DELETE FROM secrets WHERE scope = 'environment' AND scope_id = ?",
                    [&id],
                )
                .map_err(internal)?;
            match tx.execute("DELETE FROM environments WHERE id = ?", [&id]) {
                Ok(0) => return Err(Error::not_found("environment")),
                Ok(_) => {}
                Err(error) => return Err(delete_error(error, "environment in use")),
            }
            tx.commit().map_err(internal)
        })
        .await
    }

    async fn environment(&self, id: EnvironmentId) -> Result<Option<Environment>, Error> {
        self.find("SELECT * FROM environments WHERE id = ?", vec![arg(&id)])
            .await
    }

    async fn environment_by_name(
        &self,
        project: ProjectId,
        name: &EnvName,
    ) -> Result<Option<Environment>, Error> {
        self.find(
            "SELECT * FROM environments WHERE project_id = ? AND name = ?",
            vec![arg(&project), arg(name)],
        )
        .await
    }

    async fn environments_of_project(&self, project: ProjectId) -> Result<Vec<Environment>, Error> {
        self.list(
            "SELECT * FROM environments WHERE project_id = ? ORDER BY name",
            vec![arg(&project)],
        )
        .await
    }

    async fn environments(&self) -> Result<Vec<Environment>, Error> {
        self.list("SELECT * FROM environments ORDER BY id", Vec::new())
            .await
    }

    async fn environments_on_branch(
        &self,
        project: ProjectId,
        branch: &Branch,
    ) -> Result<Vec<Environment>, Error> {
        self.list(
            "SELECT * FROM environments WHERE project_id = ? AND branch = ? ORDER BY name",
            vec![arg(&project), arg(branch)],
        )
        .await
    }

    // ---- domains and bindings ------------------------------------------------

    async fn insert_domain(&self, domain: &Domain) -> Result<(), Error> {
        self.insert(domain.record()?, "project").await
    }

    async fn update_domain(&self, domain: &Domain) -> Result<(), Error> {
        self.update(domain.record()?, "project").await
    }

    async fn delete_domain(&self, id: DomainId) -> Result<(), Error> {
        self.delete(
            "DELETE FROM domains WHERE id = ?",
            "domain",
            id,
            "domain is bound to an environment",
        )
        .await
    }

    async fn domain(&self, id: DomainId) -> Result<Option<Domain>, Error> {
        self.find("SELECT * FROM domains WHERE id = ?", vec![arg(&id)])
            .await
    }

    async fn domain_by_host(&self, host: &Host) -> Result<Option<Domain>, Error> {
        self.find("SELECT * FROM domains WHERE host = ?", vec![arg(host)])
            .await
    }

    async fn domains_of_project(&self, project: ProjectId) -> Result<Vec<Domain>, Error> {
        self.list(
            "SELECT * FROM domains WHERE project_id = ? ORDER BY host",
            vec![arg(&project)],
        )
        .await
    }

    async fn domains(&self) -> Result<Vec<Domain>, Error> {
        self.list("SELECT * FROM domains ORDER BY host", Vec::new())
            .await
    }

    async fn insert_binding(&self, binding: &Binding) -> Result<(), Error> {
        let record = binding.record()?;
        let port = i64::from(binding.host_port.get());
        self.run(move |conn| {
            let insert = |conn: &mut Connection| -> rusqlite::Result<()> {
                let tx = conn.transaction()?;
                record.insert(&tx)?;
                let _cursor: usize = tx.execute(
                    "INSERT INTO port_cursor (id, host_port) VALUES (1, ?) \
                     ON CONFLICT (id) DO UPDATE SET host_port = excluded.host_port",
                    [port],
                )?;
                tx.commit()
            };
            insert(conn)
                .map_err(|error| sql::write_error(error, record.what, "environment or domain"))
        })
        .await
    }

    async fn delete_binding(&self, id: BindingId) -> Result<(), Error> {
        self.delete(
            "DELETE FROM bindings WHERE id = ?",
            "binding",
            id,
            "binding in use",
        )
        .await
    }

    async fn binding_by_domain(&self, domain: DomainId) -> Result<Option<Binding>, Error> {
        self.find(
            "SELECT * FROM bindings WHERE domain_id = ?",
            vec![arg(&domain)],
        )
        .await
    }

    async fn bindings_of_environment(
        &self,
        environment: EnvironmentId,
    ) -> Result<Vec<Binding>, Error> {
        self.list(
            "SELECT * FROM bindings WHERE environment_id = ? ORDER BY host_port",
            vec![arg(&environment)],
        )
        .await
    }

    async fn bindings(&self) -> Result<Vec<Binding>, Error> {
        self.list("SELECT * FROM bindings ORDER BY host_port", Vec::new())
            .await
    }

    async fn free_host_port(
        &self,
        range: &RangeInclusive<HostPort>,
    ) -> Result<Option<HostPort>, Error> {
        let range = range.clone();
        self.run(move |conn| {
            let low = i64::from(range.start().get());
            let high = i64::from(range.end().get());
            let used: BTreeSet<u16> = conn
                .prepare_cached("SELECT host_port FROM bindings WHERE host_port BETWEEN ? AND ?")
                .and_then(|mut statement| {
                    statement
                        .query_map([low, high], |row| row.get(0))?
                        .collect()
                })
                .map_err(internal)?;
            let cursor: Option<u16> = conn
                .query_row(
                    "SELECT host_port FROM port_cursor WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(internal)?;
            let (after, up_to): (Vec<HostPort>, Vec<HostPort>) = HostPort::each_in(&range)
                .partition(|port| cursor.is_none_or(|last| port.get() > last));
            Ok(after
                .into_iter()
                .chain(up_to)
                .find(|port| !used.contains(&port.get())))
        })
        .await
    }

    // ---- deploys -------------------------------------------------------------

    async fn insert_deploy(&self, deploy: &Deploy) -> Result<(), Error> {
        self.insert(deploy.record()?, "environment").await
    }

    async fn update_deploy(&self, deploy: &Deploy) -> Result<(), Error> {
        self.update(deploy.record()?, "environment").await
    }

    async fn deploy(&self, id: DeployId) -> Result<Option<Deploy>, Error> {
        self.find("SELECT * FROM deploys WHERE id = ?", vec![arg(&id)])
            .await
    }

    async fn deploy_by_number(
        &self,
        environment: EnvironmentId,
        number: DeployNumber,
    ) -> Result<Option<Deploy>, Error> {
        self.find(
            "SELECT * FROM deploys WHERE environment_id = ? AND number = ?",
            vec![arg(&environment), Value::Integer(i64::from(number.get()))],
        )
        .await
    }

    async fn deploys_of_environment(
        &self,
        environment: EnvironmentId,
    ) -> Result<Vec<Deploy>, Error> {
        self.list(
            "SELECT * FROM deploys WHERE environment_id = ? ORDER BY number DESC",
            vec![arg(&environment)],
        )
        .await
    }

    async fn next_deploy_number(&self, environment: EnvironmentId) -> Result<DeployNumber, Error> {
        let environment = environment.to_string();
        self.run(move |conn| {
            let highest: Option<u32> = conn
                .query_row(
                    "SELECT MAX(number) FROM deploys WHERE environment_id = ?",
                    [environment],
                    |row| row.get(0),
                )
                .map_err(internal)?;
            highest.map_or(Ok(DeployNumber::FIRST), |max| {
                DeployNumber::new(max)
                    .map_err(Error::internal)?
                    .next()
                    .ok_or_else(|| Error::internal_msg("deploy number overflow"))
            })
        })
        .await
    }

    async fn last_succeeded_deploy(
        &self,
        environment: EnvironmentId,
    ) -> Result<Option<Deploy>, Error> {
        self.find(
            "SELECT * FROM deploys WHERE environment_id = ? AND status = 'succeeded' \
             ORDER BY number DESC LIMIT 1",
            vec![arg(&environment)],
        )
        .await
    }

    async fn deploys_with_status(&self, statuses: &[DeployStatus]) -> Result<Vec<Deploy>, Error> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; statuses.len()].join(", ");
        let sql = format!("SELECT * FROM deploys WHERE status IN ({marks}) ORDER BY id");
        let args: Vec<Value> = statuses.iter().map(arg).collect();
        self.run(move |conn| sql::list(conn, &sql, &args).map_err(internal))
            .await
    }

    async fn prune_deploys(&self, environment: EnvironmentId, keep: usize) -> Result<usize, Error> {
        let environment = environment.to_string();
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM deploys WHERE environment_id = ?1 \
                 AND number NOT IN (SELECT number FROM deploys WHERE environment_id = ?1 \
                                    ORDER BY number DESC LIMIT ?2) \
                 AND id IS NOT (SELECT current_deploy_id FROM environments WHERE id = ?1)",
                rusqlite::params![environment, limit(keep)],
            )
            .map_err(internal)
        })
        .await
    }

    // ---- deploy tokens -------------------------------------------------------

    async fn insert_deploy_token(&self, token: &DeployToken) -> Result<(), Error> {
        self.insert(token.record()?, "project or user").await
    }

    async fn deploy_token(&self, id: DeployTokenId) -> Result<Option<DeployToken>, Error> {
        self.find("SELECT * FROM deploy_tokens WHERE id = ?", vec![arg(&id)])
            .await
    }

    async fn live_deploy_token(&self, project: ProjectId) -> Result<Option<DeployToken>, Error> {
        self.find(
            "SELECT * FROM deploy_tokens WHERE project_id = ? AND revoked_at IS NULL",
            vec![arg(&project)],
        )
        .await
    }

    async fn live_deploy_token_by_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<DeployToken>, Error> {
        self.find(
            "SELECT * FROM deploy_tokens WHERE token_hash = ? AND revoked_at IS NULL",
            vec![blob(hash)],
        )
        .await
    }

    async fn revoke_deploy_token(&self, id: DeployTokenId, at: Timestamp) -> Result<(), Error> {
        let at = time(at)?;
        let id = id.to_string();
        self.run(move |conn| {
            match conn.execute(
                "UPDATE deploy_tokens SET revoked_at = ? WHERE id = ?",
                rusqlite::params![at, id],
            ) {
                Ok(0) => Err(Error::not_found("deploy token")),
                Ok(_) => Ok(()),
                Err(error) => Err(internal(error)),
            }
        })
        .await
    }

    async fn replace_deploy_token(
        &self,
        old: DeployTokenId,
        new: &DeployToken,
        at: Timestamp,
    ) -> Result<(), Error> {
        let record = new.record()?;
        let at = opt_time(Some(at))?;
        let old = old.to_string();
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            let revoked = tx
                .execute(
                    "UPDATE deploy_tokens SET revoked_at = ? WHERE id = ?",
                    rusqlite::params![at, old],
                )
                .map_err(internal)?;
            if revoked == 0 {
                return Err(Error::not_found("deploy token"));
            }
            record
                .insert(&tx)
                .map_err(|error| sql::write_error(error, record.what, "project or user"))?;
            tx.commit().map_err(internal)
        })
        .await
    }

    // ---- secrets -------------------------------------------------------------

    async fn put_secret(&self, secret: &Secret) -> Result<(), Error> {
        let record = secret.record()?;
        let version = version_record(secret)?;
        let scope = secret.scope;
        self.run(move |conn| {
            let tx = conn.transaction().map_err(internal)?;
            if !scope_exists(&tx, &scope)? {
                return Err(Error::not_found(scope.kind().as_str()));
            }
            let replaced = record
                .update(&tx)
                .map_err(|error| sql::write_error(error, record.what, "scope"))?;
            if replaced == 0 {
                record
                    .insert(&tx)
                    .map_err(|error| sql::write_error(error, record.what, "scope"))?;
            }
            version
                .insert(&tx)
                .map_err(|error| sql::write_error(error, version.what, "secret"))?;
            tx.commit().map_err(internal)
        })
        .await
    }

    async fn secret(&self, scope: &SecretScope, key: &SecretKey) -> Result<Option<Secret>, Error> {
        let [kind, id] = scope_args(scope);
        self.find(
            "SELECT * FROM secrets WHERE scope = ? AND scope_id = ? AND key = ?",
            vec![kind, id, arg(key)],
        )
        .await
    }

    async fn secrets_of_scope(&self, scope: &SecretScope) -> Result<Vec<Secret>, Error> {
        let [kind, id] = scope_args(scope);
        self.list(
            "SELECT * FROM secrets WHERE scope = ? AND scope_id = ? ORDER BY key",
            vec![kind, id],
        )
        .await
    }

    async fn delete_secret(&self, id: SecretId) -> Result<(), Error> {
        self.delete(
            "DELETE FROM secrets WHERE id = ?",
            "secret",
            id,
            "secret in use",
        )
        .await
    }

    async fn secret_versions(&self, id: SecretId) -> Result<Vec<SecretVersion>, Error> {
        self.list(
            "SELECT * FROM secret_versions WHERE secret_id = ? ORDER BY version DESC",
            vec![arg(&id)],
        )
        .await
    }

    async fn prune_secret_versions(&self, id: SecretId, keep: usize) -> Result<usize, Error> {
        let id = id.to_string();
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM secret_versions WHERE secret_id = ?1 \
                 AND version NOT IN (SELECT version FROM secret_versions WHERE secret_id = ?1 \
                                     ORDER BY version DESC LIMIT ?2)",
                rusqlite::params![id, limit(keep)],
            )
            .map_err(internal)
        })
        .await
    }

    // ---- key-value of functions ------------------------

    async fn kv_get(
        &self,
        environment: EnvironmentId,
        key: &str,
    ) -> Result<Option<Vec<u8>>, Error> {
        let args = [arg(&environment), Value::Text(key.to_owned())];
        self.run(move |conn| {
            conn.query_row(
                "SELECT value FROM kv WHERE environment_id = ? AND key = ?",
                rusqlite::params_from_iter(args),
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)
        })
        .await
    }

    async fn kv_set(
        &self,
        environment: EnvironmentId,
        key: &str,
        value: &[u8],
        at: Timestamp,
    ) -> Result<(), Error> {
        let args = [
            arg(&environment),
            Value::Text(key.to_owned()),
            Value::Blob(value.to_vec()),
            time(at)?,
        ];
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO kv (environment_id, key, value, updated_at) VALUES (?, ?, ?, ?) \
                 ON CONFLICT (environment_id, key) \
                 DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                rusqlite::params_from_iter(args),
            )
            .map(|_rows| ())
            .map_err(|error| sql::write_error(error, "key", "environment"))
        })
        .await
    }

    async fn kv_delete(&self, environment: EnvironmentId, key: &str) -> Result<(), Error> {
        let args = [arg(&environment), Value::Text(key.to_owned())];
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM kv WHERE environment_id = ? AND key = ?",
                rusqlite::params_from_iter(args),
            )
            .map(|_rows| ())
            .map_err(internal)
        })
        .await
    }

    async fn kv_usage(&self, environment: EnvironmentId) -> Result<u64, Error> {
        let args = [arg(&environment)];
        let bytes: i64 = self
            .run(move |conn| {
                conn.query_row(
                    "SELECT coalesce(sum(length(CAST(key AS BLOB)) + length(value)), 0) \
                     FROM kv WHERE environment_id = ?",
                    rusqlite::params_from_iter(args),
                    |row| row.get(0),
                )
                .map_err(internal)
            })
            .await?;
        u64::try_from(bytes).map_err(Error::internal)
    }

    // ---- audit ---------------------------------------------------------------

    async fn insert_audit(&self, entry: &Audit) -> Result<(), Error> {
        self.insert(entry.record()?, "audit").await
    }

    async fn audit_log(&self) -> Result<Vec<Audit>, Error> {
        self.list("SELECT * FROM audit ORDER BY id", Vec::new())
            .await
    }

    // ---- modules -----------------------------------

    async fn insert_module(&self, module: &InstalledModule) -> Result<(), Error> {
        self.insert(module.record()?, "module").await
    }

    async fn update_module(&self, module: &InstalledModule) -> Result<(), Error> {
        self.update(module.record()?, "module").await
    }

    async fn module(&self, name: &ModuleName) -> Result<Option<InstalledModule>, Error> {
        self.find("SELECT * FROM modules WHERE name = ?", vec![arg(name)])
            .await
    }

    async fn modules(&self) -> Result<Vec<InstalledModule>, Error> {
        self.list("SELECT * FROM modules ORDER BY rowid", Vec::new())
            .await
    }

    async fn delete_module(&self, name: &ModuleName) -> Result<(), Error> {
        self.delete(
            "DELETE FROM modules WHERE name = ?",
            "module",
            name,
            "module in use",
        )
        .await
    }

    // ---- secret syncs --------------------------------

    async fn put_secret_sync(&self, sync: &SecretSyncRecord) -> Result<(), Error> {
        let record = sync.record()?;
        self.run(move |conn| {
            let names: Vec<&str> = record.columns.iter().map(|(name, _)| *name).collect();
            let marks = vec!["?"; names.len()].join(", ");
            let sql = format!(
                "INSERT INTO secret_syncs ({}) VALUES ({marks}) \
                 ON CONFLICT (secret_id, target) DO UPDATE SET \
                 version = excluded.version, synced_at = excluded.synced_at, \
                 error = excluded.error",
                names.join(", ")
            );
            conn.prepare_cached(&sql)
                .and_then(|mut statement| {
                    statement.execute(rusqlite::params_from_iter(
                        record.columns.iter().map(|(_, value)| value),
                    ))
                })
                .map(drop)
                .map_err(|error| sql::write_error(error, record.what, "secret"))
        })
        .await
    }

    async fn secret_sync(
        &self,
        secret: SecretId,
        target: SyncTarget,
    ) -> Result<Option<SecretSyncRecord>, Error> {
        self.find(
            "SELECT * FROM secret_syncs WHERE secret_id = ? AND target = ?",
            vec![arg(&secret), Value::Text(target.as_str().to_owned())],
        )
        .await
    }

    // ---- template cache ------------------------------------------------------

    async fn put_cached_template(&self, entry: &CachedTemplate) -> Result<(), Error> {
        let record = entry.record()?;
        self.run(move |conn| {
            let names: Vec<&str> = record.columns.iter().map(|(name, _)| *name).collect();
            let marks = vec!["?"; names.len()].join(", ");
            let sql = format!(
                "INSERT INTO cached_templates ({}) VALUES ({marks}) \
                 ON CONFLICT (user_id, host, repo) DO UPDATE SET \
                 manifest = excluded.manifest, fetched_at = excluded.fetched_at",
                names.join(", ")
            );
            conn.prepare_cached(&sql)
                .and_then(|mut statement| {
                    statement.execute(rusqlite::params_from_iter(
                        record.columns.iter().map(|(_, value)| value),
                    ))
                })
                .map(drop)
                .map_err(|error| sql::write_error(error, record.what, "user"))
        })
        .await
    }

    async fn cached_template(
        &self,
        user: UserId,
        host: &Host,
        repo: &RepoRef,
    ) -> Result<Option<CachedTemplate>, Error> {
        self.find(
            "SELECT * FROM cached_templates WHERE user_id = ? AND host = ? AND repo = ?",
            vec![arg(&user), arg(host), arg(repo)],
        )
        .await
    }
}

/// The live setup token in `settings`, if any.
fn live_setup_token(conn: &Connection) -> Result<Option<SetupToken>, Error> {
    let row = conn
        .query_row(
            "SELECT value, updated_at FROM settings WHERE key = ?",
            [SETUP_TOKEN_KEY],
            |row| Ok((row.get::<_, String>(0)?, timestamp(row, "updated_at")?)),
        )
        .optional()
        .map_err(internal)?;
    row.map(|(value, created_at)| setup_token(&value, created_at))
        .transpose()
}
