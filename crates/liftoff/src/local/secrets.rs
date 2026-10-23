//! [`LocalSecretStore`]: the local store of the CLI, `~/.config/liftoff/secrets.db` under the key
//! `~/.config/liftoff/key`.
//!
//! It keeps what local mode owns, and every secret of it sealed with the
//! core's [`crypto`](liftoff_core::crypto) under the local key (LCL-02):
//!
//! - the secrets of local projects, by scope: a project's, one of its
//!   environments', and its tenant's (the tenant the project will be in on
//!   a server). A local project is named, not stored: there is no project
//!   record before `lof server connect`. `set` writes a new version of a
//!   key ([`LocalSecret`] is the current one), the newest
//!   [`SecretsService::MAX_VERSIONS`] are kept for `history` and
//!   `rollback`; `secret push` copies the current values to a
//!   server (LCL-05);
//! - as the core's [`Store`] port, the records the core services of local
//!   mode keep: the forge connections of the local user, their tokens
//!   sealed (LCL-01), the installed modules and the template cache. The
//!   other records of a [`Store`] (users, sessions, projects,
//!   environments, domains, deploys, tokens, audit) belong to an
//!   installation: locally there are none, so a lookup finds nothing and
//!   a write is [`Error::Unavailable`] with a hint to `lof server
//!   connect`. The one user is [`local_user`], the admin of the machine.
//!
//! The `Store` contract of `fakes::store_contract` describes an
//! installation's store and does not apply as a whole; the rules of the
//! records this store keeps (one connection per user and host, modules in
//! installation order, one cache entry per user, host and repository)
//! are tested on it in `tests/cli/local/`.

use core::future::{Future, ready};
use core::ops::RangeInclusive;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use liftoff_core::Error;
use liftoff_core::crypto::MasterKey;
use liftoff_core::domain::{
    Artifact, ArtifactId, ArtifactVersion, Audit, Binding, BindingId, Branch, CachedTemplate,
    ConfigValue, CredentialKind, Deploy, DeployId, DeployNumber, DeployStatus, DeployToken,
    DeployTokenId, Domain, DomainId, EnvName, Environment, EnvironmentId, GateCode, GateSession,
    GitConnection, GitConnectionId, GitProvider, Host, HostPort, InstalledModule, Login,
    ModuleConfigRecord, ModuleName, ModuleVersion, OneTimeLink, Project, ProjectId, ProjectName,
    RepoRef, ScopeKind, ScopeRef, SealedEnv, Secret, SecretId, SecretKey, SecretScope,
    SecretSyncRecord, SecretValue, SecretVersion, Session, SessionId, SetupToken, SyncTarget,
    TenantId, Timestamp, TokenHash, User, UserId, UserRole, UserSource, VersionNumber,
};
use liftoff_core::ports::{Secrets as _, Store};
use liftoff_core::service::SecretsService;
use rusqlite::{Connection, OptionalExtension as _, Row, params};
use rusqlite_migration::{M, Migrations};

use super::files::{Home, private_file};

/// The schema of `secrets.db`, in the order it is applied. Append; never
/// edit a shipped one.
fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(
            "CREATE TABLE secrets (
            scope      TEXT NOT NULL,
            project    TEXT NOT NULL,
            env        TEXT NOT NULL,
            key        TEXT NOT NULL,
            value      TEXT NOT NULL,
            version    INTEGER NOT NULL,
            ci         INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (scope, project, env, key)
        ) STRICT, WITHOUT ROWID;
        CREATE TABLE git_connections (
            user_id       TEXT NOT NULL,
            host          TEXT NOT NULL,
            id            TEXT NOT NULL,
            provider      TEXT NOT NULL,
            account_login TEXT NOT NULL,
            credential    TEXT NOT NULL,
            kind          TEXT NOT NULL,
            expires_at    INTEGER,
            created_at    INTEGER NOT NULL,
            PRIMARY KEY (user_id, host)
        ) STRICT, WITHOUT ROWID;
        CREATE TABLE modules (
            name         TEXT NOT NULL PRIMARY KEY,
            version      TEXT NOT NULL,
            sha256       TEXT NOT NULL,
            enabled      INTEGER NOT NULL,
            config       TEXT NOT NULL,
            error        TEXT,
            installed_at INTEGER NOT NULL
        ) STRICT;
        CREATE TABLE template_cache (
            user_id    TEXT NOT NULL,
            host       TEXT NOT NULL,
            repo       TEXT NOT NULL,
            manifest   TEXT,
            fetched_at INTEGER NOT NULL,
            PRIMARY KEY (user_id, host, repo)
        ) STRICT, WITHOUT ROWID;",
        ),
        // `set` writes a new version, `history` and `rollback`
        // work on the kept ones (the newest `MAX_VERSIONS`, as a server
        // keeps them); `secrets` stays the current one.
        M::up(
            "CREATE TABLE secret_versions (
            scope      TEXT NOT NULL,
            project    TEXT NOT NULL,
            env        TEXT NOT NULL,
            key        TEXT NOT NULL,
            version    INTEGER NOT NULL,
            value      TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (scope, project, env, key, version)
        ) STRICT, WITHOUT ROWID;
        INSERT INTO secret_versions (scope, project, env, key, version, value, updated_at)
            SELECT scope, project, env, key, version, value, updated_at FROM secrets;",
        ),
    ])
}

/// The one user of local mode: the admin of the machine, so the core's
/// [`Authorizer`](liftoff_core::service::Authorizer) admits every local
/// operation as it admits the admin of a single-user installation.
#[must_use]
pub fn local_user() -> User {
    User {
        id: local_id(),
        login: local_login(),
        email: String::new(),
        role: UserRole::Admin,
        source: UserSource::Local,
        subject: None,
        created_at: Timestamp::UNIX_EPOCH,
    }
}

/// The id of [`local_user`]: the nil ULID.
#[expect(clippy::expect_used, reason = "a constant id that is valid")]
fn local_id() -> UserId {
    UserId::parse("00000000000000000000000000").expect("the nil ULID is a valid id")
}

/// The login of [`local_user`].
#[expect(clippy::expect_used, reason = "a constant login that is valid")]
fn local_login() -> Login {
    Login::new("local").expect("`local` is a valid login")
}

/// A secret of a local project as `ls` shows it: everything but the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSecret {
    /// The env variable.
    pub key: SecretKey,
    /// Its scope.
    pub scope: ScopeRef,
    /// The current version: 1, and one more with each `set`.
    pub version: u32,
    /// Whether it goes to the forge's CI (a project secret).
    pub ci: bool,
    /// When the current version was written.
    pub updated_at: Timestamp,
}

/// One kept version of a local secret as `history` shows it: no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalRevision {
    /// Its number.
    pub version: u32,
    /// When it was written.
    pub updated_at: Timestamp,
}

/// How many versions of a key are kept: as many as a server keeps.
fn kept_versions() -> i64 {
    i64::try_from(SecretsService::<super::LocalPorts>::MAX_VERSIONS).unwrap_or(i64::MAX)
}

/// Writes `sealed` as the next version of `key` in the scope `columns`
/// (version 1 for a new key), keeps it in the history and prunes it to
/// the newest [`kept_versions`]; the version written.
fn write_version(
    conn: &mut Connection,
    (kind, project, env): (&str, &str, &str),
    key: &SecretKey,
    sealed: &str,
    ci: bool,
    now: i64,
) -> rusqlite::Result<u32> {
    let tx = conn.transaction()?;
    let version: u32 = tx.query_row(
        "INSERT INTO secrets (scope, project, env, key, value, version, ci, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)
         ON CONFLICT (scope, project, env, key) DO UPDATE SET
            value = excluded.value, version = version + 1,
            ci = excluded.ci, updated_at = excluded.updated_at
         RETURNING version",
        params![kind, project, env, key.as_str(), sealed, ci, now],
        |row| row.get(0),
    )?;
    let _inserted = tx.execute(
        "INSERT INTO secret_versions (scope, project, env, key, version, value, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![kind, project, env, key.as_str(), version, sealed, now],
    )?;
    let _pruned = tx.execute(
        "DELETE FROM secret_versions
         WHERE scope = ?1 AND project = ?2 AND env = ?3 AND key = ?4 AND version <= ?5 - ?6",
        params![kind, project, env, key.as_str(), version, kept_versions()],
    )?;
    tx.commit()?;
    Ok(version)
}

/// See `liftoff::local`: the local store of the CLI. Clones share the connection.
#[derive(Clone)]
pub struct LocalSecretStore {
    conn: Arc<Mutex<Connection>>,
    key: Arc<MasterKey>,
}

impl core::fmt::Debug for LocalSecretStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalSecretStore").finish_non_exhaustive()
    }
}

/// The columns `(scope, project, env)` of `scope`.
fn scope_columns(scope: &ScopeRef) -> (&'static str, String, String) {
    match scope {
        ScopeRef::Tenant(project) => (
            ScopeKind::Tenant.as_str(),
            project.to_string(),
            String::new(),
        ),
        ScopeRef::Project(project) => (
            ScopeKind::Project.as_str(),
            project.to_string(),
            String::new(),
        ),
        ScopeRef::Environment(project, env) => (
            ScopeKind::Environment.as_str(),
            project.to_string(),
            env.to_string(),
        ),
        _ => ("unknown", String::new(), String::new()),
    }
}

/// A timestamp as stored: nanoseconds since the Unix epoch.
fn nanos(at: Timestamp) -> Result<i64, Error> {
    i64::try_from(at.unix_timestamp_nanos())
        .map_err(|_overflow| Error::internal_msg("a time beyond what the local store keeps"))
}

/// A stored timestamp.
fn at(raw: i64) -> rusqlite::Result<Timestamp> {
    Timestamp::from_unix_timestamp_nanos(i128::from(raw)).map_err(|error| corrupt(&error))
}

/// A stored value that does not parse.
fn corrupt(error: &dyn core::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("a corrupt row of the local store: {error}").into(),
    )
}

/// A failure of SQLite itself.
fn internal(error: rusqlite::Error) -> Error {
    Error::internal(error)
}

/// What a write of an installation's record answers in local mode.
fn needs_server<T>(what: &str) -> Result<T, Error> {
    Err(Error::unavailable(format!(
        "{what} needs a liftoff server, and the CLI runs without one: \
         log in to one with `lof login <url>` and connect this project with `lof server connect <url>`"
    )))
}

impl LocalSecretStore {
    /// Opens (or creates) `secrets.db` of `home` and its key: the config
    /// home `0700`, both files `0600`.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when a file cannot be made or read, the key is
    /// broken, or the database cannot be migrated.
    pub(crate) fn open(home: &Home) -> Result<Self, Error> {
        let key = home.master_key()?;
        let path = home.secrets();
        private_file(&path)?;
        let mut conn = Connection::open(&path).map_err(internal)?;
        conn.pragma_update(None, "busy_timeout", 5_000)
            .map_err(internal)?;
        migrations().to_latest(&mut conn).map_err(Error::internal)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            key: Arc::new(key),
        })
    }

    /// The local key, the `Secrets` port of local mode.
    pub(crate) fn key(&self) -> Arc<MasterKey> {
        Arc::clone(&self.key)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `work` on a blocking thread with the connection: the `Store`
    /// port's calls, which the core makes from async code.
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

    // ---- the secrets of local projects --------------------------------------

    /// Writes `value` as the next version of `key` in `scope` (version 1
    /// for a new key), sealed, and keeps the newest
    /// [`SecretsService::MAX_VERSIONS`] versions. The rules of a secret are the core's
    /// (`liftoff_core::service::check_ci_scope`): the caller checks them
    /// first; this store only keeps the rows.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when sealing or SQLite fails.
    pub(crate) fn set(
        &self,
        scope: &ScopeRef,
        key: &SecretKey,
        value: &SecretValue,
        ci: bool,
        now: Timestamp,
    ) -> Result<LocalSecret, Error> {
        let sealed = self.key.encrypt(value.expose())?;
        let (kind, project, env) = scope_columns(scope);
        let version = write_version(
            &mut self.conn(),
            (kind, &project, &env),
            key,
            sealed.as_str(),
            ci,
            nanos(now)?,
        )
        .map_err(internal)?;
        Ok(LocalSecret {
            key: key.clone(),
            scope: scope.clone(),
            version,
            ci,
            updated_at: now,
        })
    }

    /// The current value of `key` in `scope` and its version.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when SQLite fails or the value does not open
    /// under the local key.
    pub(crate) fn get(
        &self,
        scope: &ScopeRef,
        key: &SecretKey,
    ) -> Result<Option<(SecretValue, u32)>, Error> {
        let (kind, project, env) = scope_columns(scope);
        let sealed: Option<(String, u32)> = self
            .conn()
            .query_row(
                "SELECT value, version FROM secrets
                 WHERE scope = ?1 AND project = ?2 AND env = ?3 AND key = ?4",
                params![kind, project, env, key.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        sealed
            .map(|(sealed, version)| {
                self.key
                    .decrypt(&SealedEnv::new(sealed))
                    .map(|value| (SecretValue::new(value), version))
            })
            .transpose()
    }

    /// The secrets of `scope`, by key, without values.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when SQLite fails.
    pub(crate) fn list(&self, scope: &ScopeRef) -> Result<Vec<LocalSecret>, Error> {
        let (kind, project, env) = scope_columns(scope);
        self.select(
            "SELECT key, version, ci, updated_at FROM secrets
             WHERE scope = ?1 AND project = ?2 AND env = ?3 ORDER BY key",
            params![kind, project, env],
            |row| {
                Ok(LocalSecret {
                    key: SecretKey::new(&row.get::<_, String>(0)?)
                        .map_err(|error| corrupt(&error))?,
                    scope: scope.clone(),
                    version: row.get(1)?,
                    ci: row.get(2)?,
                    updated_at: at(row.get(3)?)?,
                })
            },
        )
    }

    /// The rows `sql` with `params` selects, each made by `map`.
    fn select<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>, Error> {
        let conn = self.conn();
        let mut statement = conn.prepare(sql).map_err(internal)?;
        let rows = statement
            .query_map(params, map)
            .and_then(Iterator::collect::<rusqlite::Result<Vec<T>>>)
            .map_err(internal);
        drop(statement);
        drop(conn);
        rows
    }

    /// Deletes `key` of `scope` with its history; whether there was one.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when SQLite fails.
    pub(crate) fn remove(&self, scope: &ScopeRef, key: &SecretKey) -> Result<bool, Error> {
        let (kind, project, env) = scope_columns(scope);
        let mut conn = self.conn();
        let removed = (|| {
            let tx = conn.transaction()?;
            let _versions = tx.execute(
                "DELETE FROM secret_versions
                 WHERE scope = ?1 AND project = ?2 AND env = ?3 AND key = ?4",
                params![kind, project, env, key.as_str()],
            )?;
            let deleted = tx.execute(
                "DELETE FROM secrets WHERE scope = ?1 AND project = ?2 AND env = ?3 AND key = ?4",
                params![kind, project, env, key.as_str()],
            )?;
            tx.commit()?;
            Ok(deleted > 0)
        })();
        removed.map_err(internal)
    }

    /// The kept versions of `key` in `scope`, newest first, without values.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a key `scope` does not hold;
    /// [`Error::Internal`] when SQLite fails.
    pub(crate) fn history(
        &self,
        scope: &ScopeRef,
        key: &SecretKey,
    ) -> Result<Vec<LocalRevision>, Error> {
        let (kind, project, env) = scope_columns(scope);
        let revisions = self.select(
            "SELECT version, updated_at FROM secret_versions
             WHERE scope = ?1 AND project = ?2 AND env = ?3 AND key = ?4
             ORDER BY version DESC",
            params![kind, project, env, key.as_str()],
            |row| {
                Ok(LocalRevision {
                    version: row.get(0)?,
                    updated_at: at(row.get(1)?)?,
                })
            },
        )?;
        if revisions.is_empty() {
            return Err(Error::not_found(format!("secret {key}")));
        }
        Ok(revisions)
    }

    /// Makes the value of version `to` of `key` in `scope` its new current
    /// version (SEC-05 as on a server), keeping its `ci`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a key `scope` does not hold or a version
    /// that is not kept; [`Error::Internal`] when SQLite fails.
    pub(crate) fn rollback(
        &self,
        scope: &ScopeRef,
        key: &SecretKey,
        to: u32,
        now: Timestamp,
    ) -> Result<LocalSecret, Error> {
        let (kind, project, env) = scope_columns(scope);
        let mut conn = self.conn();
        let current: Option<bool> = conn
            .query_row(
                "SELECT ci FROM secrets WHERE scope = ?1 AND project = ?2 AND env = ?3 AND key = ?4",
                params![kind, project, env, key.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)?;
        let ci = current.ok_or_else(|| Error::not_found(format!("secret {key}")))?;
        let sealed: String = conn
            .query_row(
                "SELECT value FROM secret_versions
                 WHERE scope = ?1 AND project = ?2 AND env = ?3 AND key = ?4 AND version = ?5",
                params![kind, project, env, key.as_str(), to],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)?
            .ok_or_else(|| Error::not_found(format!("version {to} of secret {key}")))?;
        let version = write_version(
            &mut conn,
            (kind, &project, &env),
            key,
            &sealed,
            ci,
            nanos(now)?,
        )
        .map_err(internal)?;
        drop(conn);
        Ok(LocalSecret {
            key: key.clone(),
            scope: scope.clone(),
            version,
            ci,
            updated_at: now,
        })
    }

    /// Every secret of the local project `project` in every scope, with
    /// its value: what `secret push` copies (tenant, project, then
    /// environment scopes, by key).
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when SQLite fails or a value does not open.
    pub(crate) fn of_project(
        &self,
        project: &ProjectName,
    ) -> Result<Vec<(LocalSecret, SecretValue)>, Error> {
        let rows = self.select(
            "SELECT scope, env, key, value, version, ci, updated_at FROM secrets
             WHERE project = ?1
             ORDER BY CASE scope WHEN 'tenant' THEN 0 WHEN 'project' THEN 1 ELSE 2 END, env, key",
            params![project.as_str()],
            |row| {
                Ok(StoredSecret {
                    scope: row.get(0)?,
                    env: row.get(1)?,
                    key: row.get(2)?,
                    sealed: row.get(3)?,
                    version: row.get(4)?,
                    ci: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            },
        )?;
        rows.into_iter()
            .map(|row| {
                let scope = scope_ref(&row.scope, project, &row.env)?;
                let value = SecretValue::new(self.key.decrypt(&SealedEnv::new(row.sealed))?);
                Ok((
                    LocalSecret {
                        key: SecretKey::new(&row.key)?,
                        scope,
                        version: row.version,
                        ci: row.ci,
                        updated_at: at(row.updated_at).map_err(internal)?,
                    },
                    value,
                ))
            })
            .collect()
    }
}

/// A row of `secrets` as stored.
struct StoredSecret {
    scope: String,
    env: String,
    key: String,
    sealed: String,
    version: u32,
    ci: bool,
    updated_at: i64,
}

/// The scope of a stored row: its `scope`, of `project`, `env` for an
/// environment.
fn scope_ref(kind: &str, project: &ProjectName, env: &str) -> Result<ScopeRef, Error> {
    let kind = ScopeKind::parse(kind)?;
    if kind == ScopeKind::Tenant {
        Ok(ScopeRef::Tenant(project.clone()))
    } else if kind == ScopeKind::Project {
        Ok(ScopeRef::Project(project.clone()))
    } else {
        Ok(ScopeRef::Environment(project.clone(), EnvName::new(env)?))
    }
}

// ---- rows of the Store port ---------------------------------------------------

fn connection_row(row: &Row<'_>) -> rusqlite::Result<GitConnection> {
    let parsed = |error: liftoff_core::ValidationError| corrupt(&error);
    Ok(GitConnection {
        id: GitConnectionId::parse(&row.get::<_, String>("id")?).map_err(parsed)?,
        user_id: UserId::parse(&row.get::<_, String>("user_id")?).map_err(parsed)?,
        provider: GitProvider::parse(&row.get::<_, String>("provider")?).map_err(parsed)?,
        host: Host::new(&row.get::<_, String>("host")?).map_err(parsed)?,
        account_login: Login::new(&row.get::<_, String>("account_login")?).map_err(parsed)?,
        credential: SealedEnv::new(row.get("credential")?),
        kind: CredentialKind::parse(&row.get::<_, String>("kind")?).map_err(parsed)?,
        expires_at: row
            .get::<_, Option<i64>>("expires_at")?
            .map(at)
            .transpose()?,
        created_at: at(row.get("created_at")?)?,
    })
}

/// `modules.config`: by field, `{"plain": <value>}` or `{"sealed":
/// "<ciphertext>"}` (REG-05), as the daemon's store keeps it.
fn config_text(config: &ModuleConfigRecord) -> String {
    let fields: serde_json::Map<String, serde_json::Value> = config
        .iter()
        .map(|(field, value)| {
            let encoded = match value {
                ConfigValue::Plain(json) => serde_json::json!({ "plain": json }),
                ConfigValue::Sealed(sealed) => serde_json::json!({ "sealed": sealed.as_str() }),
            };
            (field.clone(), encoded)
        })
        .collect();
    serde_json::Value::Object(fields).to_string()
}

fn parse_config(raw: &str) -> rusqlite::Result<ModuleConfigRecord> {
    let fields: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(raw).map_err(|error| corrupt(&error))?;
    fields
        .into_iter()
        .map(|(field, encoded)| {
            let value = match (encoded.get("plain"), encoded.get("sealed")) {
                (Some(json), None) => ConfigValue::Plain(json.clone()),
                (None, Some(serde_json::Value::String(sealed))) => {
                    ConfigValue::Sealed(SealedEnv::new(sealed.clone()))
                }
                _ => return Err(corrupt(&"not a module configuration")),
            };
            Ok((field, value))
        })
        .collect()
}

fn module_row(row: &Row<'_>) -> rusqlite::Result<InstalledModule> {
    let parsed = |error: liftoff_core::ValidationError| corrupt(&error);
    Ok(InstalledModule {
        name: ModuleName::new(&row.get::<_, String>("name")?).map_err(parsed)?,
        version: ModuleVersion::new(&row.get::<_, String>("version")?).map_err(parsed)?,
        sha256: row.get("sha256")?,
        enabled: row.get("enabled")?,
        config: parse_config(&row.get::<_, String>("config")?)?,
        error: row.get("error")?,
        installed_at: at(row.get("installed_at")?)?,
    })
}

const MODULE_COLUMNS: &str = "name, version, sha256, enabled, config, error, installed_at";

/// Writes `module` with `sql` (an insert or an update by name); the rows
/// it hit.
fn write_module(conn: &Connection, module: &InstalledModule, sql: &str) -> Result<usize, Error> {
    conn.execute(
        sql,
        params![
            module.name.as_str(),
            module.version.to_string(),
            module.sha256,
            module.enabled,
            config_text(&module.config),
            module.error,
            nanos(module.installed_at)?
        ],
    )
    .map_err(|error| match &error {
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Error::conflict(format!("module {} is installed already", module.name))
        }
        _ => internal(error),
    })
}

impl Store for LocalSecretStore {
    // ---- artifacts -------------------------------------

    fn insert_artifact(
        &self,
        _artifact: &Artifact,
        _first: &ArtifactVersion,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("artifacts"))
    }

    fn add_artifact_version(
        &self,
        _artifact: &Artifact,
        _version: &ArtifactVersion,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("artifacts"))
    }

    fn update_artifact(
        &self,
        _artifact: &Artifact,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("artifacts"))
    }

    fn artifact(
        &self,
        _id: ArtifactId,
    ) -> impl Future<Output = Result<Option<Artifact>, Error>> + Send {
        ready(needs_server("artifacts"))
    }

    fn artifacts(
        &self,
        _owner: Option<UserId>,
    ) -> impl Future<Output = Result<Vec<Artifact>, Error>> + Send {
        ready(needs_server("artifacts"))
    }

    fn artifact_versions(
        &self,
        _id: ArtifactId,
    ) -> impl Future<Output = Result<Vec<ArtifactVersion>, Error>> + Send {
        ready(needs_server("artifacts"))
    }

    fn artifact_version(
        &self,
        _id: ArtifactId,
        _number: VersionNumber,
    ) -> impl Future<Output = Result<Option<ArtifactVersion>, Error>> + Send {
        ready(needs_server("artifacts"))
    }

    fn delete_artifact_version(
        &self,
        _id: ArtifactId,
        _number: VersionNumber,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("artifacts"))
    }

    fn prune_artifact_versions(
        &self,
        _id: ArtifactId,
        _keep: usize,
    ) -> impl Future<Output = Result<Vec<VersionNumber>, Error>> + Send {
        ready(needs_server("artifacts"))
    }

    fn delete_artifact(&self, _id: ArtifactId) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("artifacts"))
    }

    fn expired_artifacts(
        &self,
        _now: Timestamp,
    ) -> impl Future<Output = Result<Vec<Artifact>, Error>> + Send {
        ready(needs_server("artifacts"))
    }

    // ---- users: the local user only ---------------------------------------------

    fn insert_user(&self, _user: &User) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a user"))
    }

    fn update_user(&self, _user: &User) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a user"))
    }

    fn user(&self, id: UserId) -> impl Future<Output = Result<Option<User>, Error>> + Send {
        let local = local_user();
        ready(Ok((local.id == id).then_some(local)))
    }

    fn user_by_login(
        &self,
        login: &Login,
    ) -> impl Future<Output = Result<Option<User>, Error>> + Send {
        let local = local_user();
        ready(Ok((&local.login == login).then_some(local)))
    }

    fn user_by_subject(
        &self,
        _source: &UserSource,
        _subject: &str,
    ) -> impl Future<Output = Result<Option<User>, Error>> + Send {
        ready(Ok(None))
    }

    fn users(&self) -> impl Future<Output = Result<Vec<User>, Error>> + Send {
        ready(Ok(vec![local_user()]))
    }

    // ---- sessions, gate, setup token: an installation's -------------------------

    fn insert_session(&self, _session: &Session) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a session"))
    }

    fn update_session(&self, _session: &Session) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a session"))
    }

    fn session_by_hash(
        &self,
        _hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<Session>, Error>> + Send {
        ready(Ok(None))
    }

    fn sessions_of_user(
        &self,
        _user: UserId,
    ) -> impl Future<Output = Result<Vec<Session>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn delete_session(&self, _id: SessionId) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("session")))
    }

    fn delete_sessions_of_user(
        &self,
        _user: UserId,
    ) -> impl Future<Output = Result<usize, Error>> + Send {
        ready(Ok(0))
    }

    fn insert_gate_code(&self, _code: &GateCode) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a gate code"))
    }

    fn take_gate_code(
        &self,
        _hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<GateCode>, Error>> + Send {
        ready(Ok(None))
    }

    fn insert_gate_session(
        &self,
        _session: &GateSession,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a gate session"))
    }

    fn gate_session(
        &self,
        _hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<GateSession>, Error>> + Send {
        ready(Ok(None))
    }

    fn prune_gate_records(
        &self,
        _now: Timestamp,
    ) -> impl Future<Output = Result<(usize, usize), Error>> + Send {
        ready(Ok((0, 0)))
    }

    fn insert_one_time_link(
        &self,
        _link: &OneTimeLink,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a one-time link"))
    }

    fn one_time_link(
        &self,
        _hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<OneTimeLink>, Error>> + Send {
        ready(Ok(None))
    }

    fn use_one_time_link(
        &self,
        _hash: &TokenHash,
        _at: Timestamp,
    ) -> impl Future<Output = Result<Option<OneTimeLink>, Error>> + Send {
        ready(Ok(None))
    }

    fn prune_one_time_links(
        &self,
        _now: Timestamp,
    ) -> impl Future<Output = Result<usize, Error>> + Send {
        ready(Ok(0))
    }

    fn put_setup_token(
        &self,
        _token: &SetupToken,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a setup token"))
    }

    fn setup_token(
        &self,
        _hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<SetupToken>, Error>> + Send {
        ready(Ok(None))
    }

    fn exchange_setup_token(
        &self,
        _hash: &TokenHash,
        _session: &Session,
    ) -> impl Future<Output = Result<Option<SetupToken>, Error>> + Send {
        ready(Ok(None))
    }

    // ---- git connections: kept ----------------------------------------------------

    async fn put_git_connection(&self, connection: &GitConnection) -> Result<(), Error> {
        if connection.user_id != local_user().id {
            return Err(Error::not_found("user"));
        }
        let connection = connection.clone();
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO git_connections
                    (user_id, host, id, provider, account_login, credential, kind, expires_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (user_id, host) DO UPDATE SET
                    id = excluded.id, provider = excluded.provider,
                    account_login = excluded.account_login, credential = excluded.credential,
                    kind = excluded.kind, expires_at = excluded.expires_at,
                    created_at = excluded.created_at",
                params![
                    connection.user_id.to_string(),
                    connection.host.as_str(),
                    connection.id.to_string(),
                    connection.provider.as_str(),
                    connection.account_login.as_str(),
                    connection.credential.as_str(),
                    connection.kind.as_str(),
                    connection.expires_at.map(nanos).transpose()?,
                    nanos(connection.created_at)?
                ],
            )
            .map(drop)
            .map_err(internal)
        })
        .await
    }

    async fn git_connection(
        &self,
        user: UserId,
        host: &Host,
    ) -> Result<Option<GitConnection>, Error> {
        let host = host.clone();
        self.run(move |conn| {
            conn.query_row(
                "SELECT * FROM git_connections WHERE user_id = ?1 AND host = ?2",
                params![user.to_string(), host.as_str()],
                connection_row,
            )
            .optional()
            .map_err(internal)
        })
        .await
    }

    async fn git_connections_of_user(&self, user: UserId) -> Result<Vec<GitConnection>, Error> {
        self.run(move |conn| {
            let mut statement = conn
                .prepare("SELECT * FROM git_connections WHERE user_id = ?1 ORDER BY host")
                .map_err(internal)?;
            let rows = statement
                .query_map(params![user.to_string()], connection_row)
                .map_err(internal)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
        })
        .await
    }

    async fn delete_git_connection(&self, user: UserId, host: &Host) -> Result<(), Error> {
        let host = host.clone();
        self.run(move |conn| {
            let deleted = conn
                .execute(
                    "DELETE FROM git_connections WHERE user_id = ?1 AND host = ?2",
                    params![user.to_string(), host.as_str()],
                )
                .map_err(internal)?;
            if deleted == 0 {
                return Err(Error::not_found("git connection"));
            }
            Ok(())
        })
        .await
    }

    // ---- projects, environments, domains: none locally ----------------------------

    fn insert_project(&self, _project: &Project) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("creating a project"))
    }

    fn update_project(&self, _project: &Project) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("project")))
    }

    fn project(
        &self,
        _id: ProjectId,
    ) -> impl Future<Output = Result<Option<Project>, Error>> + Send {
        ready(Ok(None))
    }

    fn project_by_name(
        &self,
        _tenant: Option<TenantId>,
        _name: &ProjectName,
    ) -> impl Future<Output = Result<Option<Project>, Error>> + Send {
        ready(Ok(None))
    }

    fn project_by_repo(
        &self,
        _git_host: &Host,
        _repo: &RepoRef,
    ) -> impl Future<Output = Result<Option<Project>, Error>> + Send {
        ready(Ok(None))
    }

    fn projects(&self) -> impl Future<Output = Result<Vec<Project>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn delete_project(&self, _id: ProjectId) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("project")))
    }

    fn insert_environment(
        &self,
        _environment: &Environment,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("an environment"))
    }

    fn update_environment(
        &self,
        _environment: &Environment,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("environment")))
    }

    fn set_environment_protected(
        &self,
        _id: EnvironmentId,
        _protected: bool,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("environment")))
    }

    fn delete_environment(
        &self,
        _id: EnvironmentId,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("environment")))
    }

    fn environment(
        &self,
        _id: EnvironmentId,
    ) -> impl Future<Output = Result<Option<Environment>, Error>> + Send {
        ready(Ok(None))
    }

    fn environment_by_name(
        &self,
        _project: ProjectId,
        _name: &EnvName,
    ) -> impl Future<Output = Result<Option<Environment>, Error>> + Send {
        ready(Ok(None))
    }

    fn environments_of_project(
        &self,
        _project: ProjectId,
    ) -> impl Future<Output = Result<Vec<Environment>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn environments(&self) -> impl Future<Output = Result<Vec<Environment>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn environments_on_branch(
        &self,
        _project: ProjectId,
        _branch: &Branch,
    ) -> impl Future<Output = Result<Vec<Environment>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn insert_domain(&self, _domain: &Domain) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a domain"))
    }

    fn update_domain(&self, _domain: &Domain) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("domain")))
    }

    fn delete_domain(&self, _id: DomainId) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("domain")))
    }

    fn domain(&self, _id: DomainId) -> impl Future<Output = Result<Option<Domain>, Error>> + Send {
        ready(Ok(None))
    }

    fn domain_by_host(
        &self,
        _host: &Host,
    ) -> impl Future<Output = Result<Option<Domain>, Error>> + Send {
        ready(Ok(None))
    }

    fn domains_of_project(
        &self,
        _project: ProjectId,
    ) -> impl Future<Output = Result<Vec<Domain>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn domains(&self) -> impl Future<Output = Result<Vec<Domain>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn insert_binding(&self, _binding: &Binding) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a binding"))
    }

    fn delete_binding(&self, _id: BindingId) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("binding")))
    }

    fn binding_by_domain(
        &self,
        _domain: DomainId,
    ) -> impl Future<Output = Result<Option<Binding>, Error>> + Send {
        ready(Ok(None))
    }

    fn bindings_of_environment(
        &self,
        _environment: EnvironmentId,
    ) -> impl Future<Output = Result<Vec<Binding>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn bindings(&self) -> impl Future<Output = Result<Vec<Binding>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn free_host_port(
        &self,
        _range: &RangeInclusive<HostPort>,
    ) -> impl Future<Output = Result<Option<HostPort>, Error>> + Send {
        ready(needs_server("a host port"))
    }

    // ---- deploys and deploy tokens: an installation's -----------------------------

    fn insert_deploy(&self, _deploy: &Deploy) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a deploy"))
    }

    fn update_deploy(&self, _deploy: &Deploy) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("deploy")))
    }

    fn deploy(&self, _id: DeployId) -> impl Future<Output = Result<Option<Deploy>, Error>> + Send {
        ready(Ok(None))
    }

    fn deploy_by_number(
        &self,
        _environment: EnvironmentId,
        _number: DeployNumber,
    ) -> impl Future<Output = Result<Option<Deploy>, Error>> + Send {
        ready(Ok(None))
    }

    fn deploys_of_environment(
        &self,
        _environment: EnvironmentId,
    ) -> impl Future<Output = Result<Vec<Deploy>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn next_deploy_number(
        &self,
        _environment: EnvironmentId,
    ) -> impl Future<Output = Result<DeployNumber, Error>> + Send {
        ready(needs_server("a deploy"))
    }

    fn last_succeeded_deploy(
        &self,
        _environment: EnvironmentId,
    ) -> impl Future<Output = Result<Option<Deploy>, Error>> + Send {
        ready(Ok(None))
    }

    fn deploys_with_status(
        &self,
        _statuses: &[DeployStatus],
    ) -> impl Future<Output = Result<Vec<Deploy>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn prune_deploys(
        &self,
        _environment: EnvironmentId,
        _keep: usize,
    ) -> impl Future<Output = Result<usize, Error>> + Send {
        ready(Ok(0))
    }

    fn insert_deploy_token(
        &self,
        _token: &DeployToken,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a deploy token"))
    }

    fn deploy_token(
        &self,
        _id: DeployTokenId,
    ) -> impl Future<Output = Result<Option<DeployToken>, Error>> + Send {
        ready(Ok(None))
    }

    fn live_deploy_token(
        &self,
        _project: ProjectId,
    ) -> impl Future<Output = Result<Option<DeployToken>, Error>> + Send {
        ready(Ok(None))
    }

    fn live_deploy_token_by_hash(
        &self,
        _hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<DeployToken>, Error>> + Send {
        ready(Ok(None))
    }

    fn revoke_deploy_token(
        &self,
        _id: DeployTokenId,
        _at: Timestamp,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("deploy token")))
    }

    fn replace_deploy_token(
        &self,
        _old: DeployTokenId,
        _new: &DeployToken,
        _at: Timestamp,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a deploy token"))
    }

    // ---- secrets of an installation's scopes: none (see `set`) -------------------

    fn put_secret(&self, _secret: &Secret) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a secret of a project on a server"))
    }

    fn secret(
        &self,
        _scope: &SecretScope,
        _key: &SecretKey,
    ) -> impl Future<Output = Result<Option<Secret>, Error>> + Send {
        ready(Ok(None))
    }

    fn secrets_of_scope(
        &self,
        _scope: &SecretScope,
    ) -> impl Future<Output = Result<Vec<Secret>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn delete_secret(&self, _id: SecretId) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found("secret")))
    }

    fn secret_versions(
        &self,
        _id: SecretId,
    ) -> impl Future<Output = Result<Vec<SecretVersion>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    fn prune_secret_versions(
        &self,
        _id: SecretId,
        _keep: usize,
    ) -> impl Future<Output = Result<usize, Error>> + Send {
        ready(Ok(0))
    }

    fn put_secret_sync(
        &self,
        _sync: &SecretSyncRecord,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("syncing CI secrets"))
    }

    fn secret_sync(
        &self,
        _secret: SecretId,
        _target: SyncTarget,
    ) -> impl Future<Output = Result<Option<SecretSyncRecord>, Error>> + Send {
        ready(Ok(None))
    }

    // ---- key-value of functions -------------------------------------------------------
    // The local store keeps no keys of functions: they live in the
    // server's `kv` table; writing one needs a server, reading
    // finds nothing, like the gate records above.

    fn kv_get(
        &self,
        _environment: EnvironmentId,
        _key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send {
        ready(Ok(None))
    }

    fn kv_set(
        &self,
        _environment: EnvironmentId,
        _key: &str,
        _value: &[u8],
        _at: Timestamp,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a function's key-value store"))
    }

    fn kv_delete(
        &self,
        _environment: EnvironmentId,
        _key: &str,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("a function's key-value store"))
    }

    fn kv_usage(
        &self,
        _environment: EnvironmentId,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        ready(Ok(0))
    }

    // ---- audit: an installation's ---------------------------------------------------

    fn insert_audit(&self, _entry: &Audit) -> impl Future<Output = Result<(), Error>> + Send {
        ready(needs_server("the audit log"))
    }

    fn audit_log(&self) -> impl Future<Output = Result<Vec<Audit>, Error>> + Send {
        ready(Ok(Vec::new()))
    }

    // ---- modules: kept ----------------------------------------------------------------

    async fn insert_module(&self, module: &InstalledModule) -> Result<(), Error> {
        let module = module.clone();
        self.run(move |conn| {
            write_module(
                conn,
                &module,
                &format!(
                    "INSERT INTO modules ({MODULE_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                ),
            )
            .map(drop)
        })
        .await
    }

    async fn update_module(&self, module: &InstalledModule) -> Result<(), Error> {
        let module = module.clone();
        self.run(move |conn| {
            let updated = write_module(
                conn,
                &module,
                "UPDATE modules SET version = ?2, sha256 = ?3, enabled = ?4, config = ?5,
                    error = ?6, installed_at = ?7 WHERE name = ?1",
            )?;
            if updated == 0 {
                return Err(Error::not_found("module"));
            }
            Ok(())
        })
        .await
    }

    async fn module(&self, name: &ModuleName) -> Result<Option<InstalledModule>, Error> {
        let name = name.clone();
        self.run(move |conn| {
            conn.query_row(
                &format!("SELECT {MODULE_COLUMNS} FROM modules WHERE name = ?1"),
                params![name.as_str()],
                module_row,
            )
            .optional()
            .map_err(internal)
        })
        .await
    }

    async fn modules(&self) -> Result<Vec<InstalledModule>, Error> {
        self.run(|conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {MODULE_COLUMNS} FROM modules ORDER BY installed_at, rowid"
                ))
                .map_err(internal)?;
            let rows = statement.query_map([], module_row).map_err(internal)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
        })
        .await
    }

    async fn delete_module(&self, name: &ModuleName) -> Result<(), Error> {
        let name = name.clone();
        self.run(move |conn| {
            let deleted = conn
                .execute(
                    "DELETE FROM modules WHERE name = ?1",
                    params![name.as_str()],
                )
                .map_err(internal)?;
            if deleted == 0 {
                return Err(Error::not_found("module"));
            }
            Ok(())
        })
        .await
    }

    // ---- template cache: kept -----------------------------------------------------------

    async fn put_cached_template(&self, entry: &CachedTemplate) -> Result<(), Error> {
        if entry.user_id != local_user().id {
            return Err(Error::not_found("user"));
        }
        let entry = entry.clone();
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO template_cache (user_id, host, repo, manifest, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (user_id, host, repo) DO UPDATE SET
                    manifest = excluded.manifest, fetched_at = excluded.fetched_at",
                params![
                    entry.user_id.to_string(),
                    entry.host.as_str(),
                    entry.repo.to_string(),
                    entry.manifest,
                    nanos(entry.fetched_at)?
                ],
            )
            .map(drop)
            .map_err(internal)
        })
        .await
    }

    async fn cached_template(
        &self,
        user: UserId,
        host: &Host,
        repo: &RepoRef,
    ) -> Result<Option<CachedTemplate>, Error> {
        let (host, repo) = (host.clone(), repo.clone());
        self.run(move |conn| {
            conn.query_row(
                "SELECT manifest, fetched_at FROM template_cache
                 WHERE user_id = ?1 AND host = ?2 AND repo = ?3",
                params![user.to_string(), host.as_str(), repo.to_string()],
                |row| {
                    Ok(CachedTemplate {
                        user_id: user,
                        host: host.clone(),
                        repo: repo.clone(),
                        manifest: row.get(0)?,
                        fetched_at: at(row.get(1)?)?,
                    })
                },
            )
            .optional()
            .map_err(internal)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use liftoff_core::fakes::Records;

    use super::*;

    /// A store in a config home of its own, which lives as long as it.
    fn store() -> (tempfile::TempDir, LocalSecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::of(&dir.path().join("liftoff").join("config.toml"));
        let store = LocalSecretStore::open(&home).unwrap();
        (dir, store)
    }

    fn host(raw: &str) -> Host {
        Host::new(raw).unwrap()
    }

    fn key(raw: &str) -> SecretKey {
        SecretKey::new(raw).unwrap()
    }

    fn demo() -> ProjectName {
        ProjectName::new("demo").unwrap()
    }

    /// TC: LCL-01. The forge connections of the local user, one per host
    /// (the `Store` contract's rule for them).
    mod git_connections {
        use super::*;

        // TC: LCL-01
        #[tokio::test]
        async fn replaces_connection_when_same_user_and_host() {
            let (_dir, store) = store();
            let records = Records::default();
            let owner = local_user();
            let first = records
                .git_connection(&owner, "github.com", SealedEnv::new("old".to_owned()))
                .unwrap();
            let second = records
                .git_connection(&owner, "github.com", SealedEnv::new("new".to_owned()))
                .unwrap();
            store.put_git_connection(&first).await.unwrap();
            store.put_git_connection(&second).await.unwrap();
            assert_eq!(
                store.git_connections_of_user(owner.id).await.unwrap(),
                vec![second]
            );
        }

        // TC: LCL-01 (sorted by host)
        #[tokio::test]
        async fn lists_by_host_when_user_has_several() {
            let (_dir, store) = store();
            let records = Records::default();
            let owner = local_user();
            for raw in ["gitlab.com", "github.com"] {
                let connection = records
                    .git_connection(&owner, raw, SealedEnv::new("t".to_owned()))
                    .unwrap();
                store.put_git_connection(&connection).await.unwrap();
            }
            let hosts: Vec<String> = store
                .git_connections_of_user(owner.id)
                .await
                .unwrap()
                .iter()
                .map(|connection| connection.host.to_string())
                .collect();
            assert_eq!(hosts, vec!["github.com", "gitlab.com"]);
        }

        // TC: LCL-01 (the local user is the only one)
        #[tokio::test]
        async fn not_found_when_connection_is_of_another_user() {
            let (_dir, store) = store();
            let records = Records::default();
            let bob = records.user("bob", UserRole::User).unwrap();
            let connection = records
                .git_connection(&bob, "github.com", SealedEnv::new("t".to_owned()))
                .unwrap();
            let refused = store.put_git_connection(&connection).await;
            assert!(matches!(refused, Err(Error::NotFound(_))), "{refused:?}");
        }

        // TC: LCL-01
        #[tokio::test]
        async fn not_found_when_disconnecting_unknown_host() {
            let (_dir, store) = store();
            let refused = store
                .delete_git_connection(local_user().id, &host("github.com"))
                .await;
            assert!(matches!(refused, Err(Error::NotFound(_))), "{refused:?}");
        }
    }

    /// TC: LCL-01. The installed modules of local mode (`lof module
    /// install`): the module rules of the core's `store_contract`, which
    /// need no seeded users, run over this store as over the daemon's.
    /// (The contract's connection and template rules seed users of an
    /// installation, which this store refuses: the local-user versions of
    /// them are above and below.)
    mod modules {
        use super::*;

        macro_rules! contract_rules {
            ($($rule:ident),+ $(,)?) => {
                $(
                    #[tokio::test]
                    async fn $rule() -> Result<(), Error> {
                        let (_dir, store) = store();
                        liftoff_core::fakes::store_contract::$rule(&store).await
                    }
                )+
            };
        }

        contract_rules!(
            round_trips_module_with_sealed_config_when_stored,
            conflicts_when_module_name_taken,
            lists_modules_in_install_order_when_listed,
            replaces_module_when_updated,
            forgets_module_when_deleted,
            not_found_when_module_missing_on_update_or_delete,
        );
    }

    /// TC: LCL-03. The template cache of local mode, one entry per user,
    /// host and repository.
    mod template_cache {
        use super::*;

        // TC: LCL-03 (the contract's `replaces_cached_template_when_put_again`
        // for the local user)
        #[tokio::test]
        async fn replaces_local_users_cached_template_when_put_again() {
            let (_dir, store) = store();
            let entry = |manifest: &str| CachedTemplate {
                user_id: local_user().id,
                host: host("github.com"),
                repo: RepoRef::new("acme/app").unwrap(),
                manifest: Some(manifest.to_owned()),
                fetched_at: Timestamp::from_unix_timestamp(10).unwrap(),
            };
            store.put_cached_template(&entry("old")).await.unwrap();
            store.put_cached_template(&entry("new")).await.unwrap();
            let cached = store
                .cached_template(
                    local_user().id,
                    &host("github.com"),
                    &RepoRef::new("acme/app").unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(cached, Some(entry("new")));
        }
    }

    /// TC: LCL-02. The secrets of local projects.
    mod secrets {
        use super::*;

        // TC: LCL-02 (each `set` is one more version)
        #[test]
        fn counts_versions_when_set_again() {
            let (_dir, store) = store();
            let scope = ScopeRef::Project(demo());
            let at = Timestamp::UNIX_EPOCH;
            let _first = store
                .set(
                    &scope,
                    &key("API_KEY"),
                    &SecretValue::new("a".to_owned()),
                    false,
                    at,
                )
                .unwrap();
            let second = store
                .set(
                    &scope,
                    &key("API_KEY"),
                    &SecretValue::new("b".to_owned()),
                    false,
                    at,
                )
                .unwrap();
            let current = store.get(&scope, &key("API_KEY")).unwrap();
            assert_eq!(
                (
                    second.version,
                    current.map(|(value, version)| (value.expose().to_owned(), version))
                ),
                (2, Some(("b".to_owned(), 2)))
            );
        }

        // TC: LCL-05 (what `secret push` copies: every scope of the
        // project, tenant first, with its value)
        #[test]
        fn lists_every_scope_of_project_when_pushed() {
            let (_dir, store) = store();
            let at = Timestamp::UNIX_EPOCH;
            let prod = ScopeRef::Environment(demo(), EnvName::new("prod").unwrap());
            for (scope, name) in [
                (prod, "DATABASE_URL"),
                (ScopeRef::Project(demo()), "SESSION_SECRET"),
                (ScopeRef::Tenant(demo()), "SHARED"),
            ] {
                let _set = store
                    .set(
                        &scope,
                        &key(name),
                        &SecretValue::new(name.to_owned()),
                        false,
                        at,
                    )
                    .unwrap();
            }
            let other = ScopeRef::Project(ProjectName::new("other").unwrap());
            let _other = store
                .set(
                    &other,
                    &key("NOT_MINE"),
                    &SecretValue::new("x".to_owned()),
                    false,
                    at,
                )
                .unwrap();
            let pushed: Vec<(String, String)> = store
                .of_project(&demo())
                .unwrap()
                .into_iter()
                .map(|(secret, value)| (secret.key.to_string(), value.expose().to_owned()))
                .collect();
            assert_eq!(
                pushed,
                vec![
                    ("SHARED".to_owned(), "SHARED".to_owned()),
                    ("SESSION_SECRET".to_owned(), "SESSION_SECRET".to_owned()),
                    ("DATABASE_URL".to_owned(), "DATABASE_URL".to_owned()),
                ]
            );
        }
    }

    /// TC: LCL-03. What an installation keeps is not kept locally.
    mod installation_records {
        use super::*;

        // TC: LCL-03 (the plan stops before `CreateProject`; a write that
        // reached the store anyway says what to do)
        #[tokio::test]
        async fn unavailable_with_server_connect_hint_when_project_is_inserted() {
            let (_dir, store) = store();
            let records = Records::default();
            let project = records
                .project("demo", "github.com", "owner/demo", &local_user())
                .unwrap();
            let refused = store.insert_project(&project).await;
            assert!(
                matches!(&refused, Err(Error::Unavailable(why)) if why.contains("lof server connect")),
                "{refused:?}"
            );
        }

        // TC: LCL-03 (a name or a domain is never taken locally)
        #[tokio::test]
        async fn finds_nothing_when_project_or_domain_is_looked_up() {
            let (_dir, store) = store();
            let project = store.project_by_name(None, &demo()).await.unwrap();
            let domain = store
                .domain_by_host(&host("demo.liftoff.localhost"))
                .await
                .unwrap();
            assert_eq!((project, domain), (None, None));
        }
    }
}
