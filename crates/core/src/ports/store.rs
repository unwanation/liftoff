use core::ops::RangeInclusive;

use crate::domain::{
    Artifact, ArtifactId, ArtifactVersion, Audit, Binding, BindingId, Branch, CachedTemplate,
    Deploy, DeployId, DeployNumber, DeployStatus, DeployToken, DeployTokenId, Domain, DomainId,
    EnvName, Environment, EnvironmentId, GateCode, GateSession, GitConnection, Host, HostPort,
    Login, OneTimeLink, Project, ProjectId, ProjectName, RepoRef, Secret, SecretId, SecretKey,
    SecretScope, SecretSyncRecord, SecretVersion, Session, SessionId, SetupToken, SyncTarget,
    TenantId, Timestamp, TokenHash, User, UserId, UserSource, VersionNumber,
};
use crate::domain::{InstalledModule, ModuleName};
use crate::error::Error;

/// Persistence of every core record.
///
/// # Contract
///
/// Both implementations (`InMemoryStore` and the SQLite store) obey the same
/// rules, which `fakes::store_contract` checks rule by rule:
///
/// - **Uniqueness → [`Error::Conflict`]**: `users.login`,
///   `(users.source, users.subject)` of users with a subject,
///   `sessions.token_hash`, `gate_codes.code_hash`,
///   `gate_sessions.token_hash`, `(projects.git_host, projects.repo)`,
///   `(projects.tenant_id, projects.name)` (a `None` tenant counts as one
///   value: two projects without a tenant cannot share a name),
///   `(environments.project_id, environments.name)`, `domains.host`,
///   `bindings.domain_id`, `bindings.host_port`,
///   `(deploys.environment_id, deploys.number)`, one live
///   (`revoked_at IS NULL`) deploy token per project,
///   `(secrets.scope, secrets.scope_id, secrets.key)` and
///   `(secret_versions.secret_id, secret_versions.version)`. Updates
///   re-check uniqueness.
/// - **Setup token**: at most one is live; [`Store::put_setup_token`]
///   replaces it and [`Store::exchange_setup_token`] trades it for a
///   session in one transaction.
/// - **Missing parent → [`Error::NotFound`]**: inserting a row whose user,
///   project, environment, domain or (for a gate code or session) browser
///   session does not exist; a secret whose scope (project or
///   environment) does not exist.
/// - **Missing row → [`Error::NotFound`]**: `update_*`, `delete_*` and
///   `revoke_*` of an id that does not exist.
/// - **Restrict → [`Error::Conflict`]**: deleting a domain that still has a
///   binding (DOM-06); deleting a project that still has an environment or
///   a domain.
/// - **Cascade**: deleting a session ([`Store::delete_session`],
///   [`Store::delete_sessions_of_user`]) deletes the gate codes and gate
///   sessions handed over from it (TEN-08); deleting every session of a
///   user deletes their one-time links too (an unredeemed link is a
///   session to come, OTL-02). Deleting an environment deletes its bindings, its
///   deploys and the secrets of its scope with their versions; domains and
///   the project's secrets stay (ENV-07, SEC-09). Deleting a secret
///   deletes its versions. Deleting a project deletes its deploy tokens
///   and the secrets of its scope with their versions.
/// - **Ordering**: lists are sorted by their natural key (users by login,
///   projects and environments by name, domains by host, bindings by host
///   port, secrets by key, git connections by host); sessions of a user by
///   id; deploys of an environment newest first (highest number first);
///   secret versions newest first; [`Store::deploys_with_status`] and
///   [`Store::audit_log`] oldest first (by id: ids are ULIDs, so creation
///   order).
/// - **Secret syncs**: one row per `(secret_id, target)`
///   ([`Store::put_secret_sync`] replaces it); deleting a secret (also
///   through its environment or project) deletes its syncs (SYNC-03).
/// - **One-time links**: keyed by
///   `token_hash` (unique → [`Error::Conflict`]; a missing user →
///   [`Error::NotFound`]); [`Store::use_one_time_link`] marks one used
///   once: two uses of one link cannot both succeed (OTL-02);
///   [`Store::prune_one_time_links`] deletes the expired ones, used or
///   not (OTL-01).
/// - **Key-value of functions** (`kv`): one value per `(environment_id, key)`, [`Store::kv_set`]
///   replaces it; environments never see each other's keys (FN-03);
///   deleting an environment deletes its keys; a missing environment →
///   [`Error::NotFound`].
/// - **Template cache**: one entry per `(user_id, host, repo)`
///   ([`Store::put_cached_template`] replaces it); entries of different
///   users or hosts never answer for each other.
/// - **Artifacts**: `artifacts.id` unique
///   (→ [`Error::Conflict`]), `(artifact_id, number)` of a version unique
///   (→ [`Error::Conflict`]); a missing owner or `created_by` user, or
///   a version of a missing artifact → [`Error::NotFound`]. An artifact
///   comes with its version 1 ([`Store::insert_artifact`]); a later
///   version comes with the artifact row it makes current
///   ([`Store::add_artifact_version`]), in one transaction each. A
///   version keeps its files (`artifact_files`: name, hash, size) in
///   the order given. [`Store::delete_artifact`] deletes its versions and
///   their files; [`Store::delete_artifact_version`] of the current
///   version → [`Error::Conflict`]; [`Store::prune_artifact_versions`]
///   keeps the newest `keep` and never the current one.
///   [`Store::artifacts`] lists newest first (by `created_at`, then id),
///   versions oldest first (by number); [`Store::expired_artifacts`]
///   is every `private` artifact whose `expires_at` is not after `now`,
///   never a `shared` one.
/// - **Host ports are not reused at once**: see [`Store::free_host_port`].
/// - **Case**: logins and repositories are compared as the lower-cased
///   values [`Login`] and [`RepoRef`] hold.
/// - Lookups return `Ok(None)` when nothing matches; `Err` is reserved for
///   failures of the store itself.
pub trait Store: Send + Sync {
    // ---- users -------------------------------------------------------------

    /// Inserts a new user.
    fn insert_user(&self, user: &User) -> impl Future<Output = Result<(), Error>> + Send;
    /// Replaces a user by id.
    fn update_user(&self, user: &User) -> impl Future<Output = Result<(), Error>> + Send;
    /// A user by id.
    fn user(&self, id: UserId) -> impl Future<Output = Result<Option<User>, Error>> + Send;
    /// A user by login.
    fn user_by_login(
        &self,
        login: &Login,
    ) -> impl Future<Output = Result<Option<User>, Error>> + Send;
    /// The user of `source` whose OIDC subject is `subject`.
    fn user_by_subject(
        &self,
        source: &UserSource,
        subject: &str,
    ) -> impl Future<Output = Result<Option<User>, Error>> + Send;
    /// Every user, by login.
    fn users(&self) -> impl Future<Output = Result<Vec<User>, Error>> + Send;

    // ---- sessions ------------------------------------------------------------

    /// Inserts a session.
    fn insert_session(&self, session: &Session) -> impl Future<Output = Result<(), Error>> + Send;
    /// Replaces a session by id.
    fn update_session(&self, session: &Session) -> impl Future<Output = Result<(), Error>> + Send;
    /// A session by token hash (expired ones included; the caller checks).
    fn session_by_hash(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<Session>, Error>> + Send;
    /// The sessions of `user`, by id.
    fn sessions_of_user(
        &self,
        user: UserId,
    ) -> impl Future<Output = Result<Vec<Session>, Error>> + Send;
    /// Deletes one session.
    fn delete_session(&self, id: SessionId) -> impl Future<Output = Result<(), Error>> + Send;
    /// Deletes every session of `user` and their one-time links; returns
    /// how many sessions.
    fn delete_sessions_of_user(
        &self,
        user: UserId,
    ) -> impl Future<Output = Result<usize, Error>> + Send;

    // ---- gate ------------------------------------------

    /// Inserts a one-time code of the gate. Its session must exist.
    fn insert_gate_code(&self, code: &GateCode) -> impl Future<Output = Result<(), Error>> + Send;
    /// Takes the code of hash `hash` out of the store (expired ones
    /// included; the caller checks): two takes of one code cannot both
    /// find it.
    fn take_gate_code(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<GateCode>, Error>> + Send;
    /// Inserts a session on a protected host. Its browser session must
    /// exist.
    fn insert_gate_session(
        &self,
        session: &GateSession,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// The session on a protected host of token hash `hash` (expired ones
    /// included; the caller checks).
    fn gate_session(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<GateSession>, Error>> + Send;
    /// Deletes the gate codes and the sessions on protected hosts whose
    /// `expires_at` is not after `now` (v1 SSO-13); how many of each.
    fn prune_gate_records(
        &self,
        now: Timestamp,
    ) -> impl Future<Output = Result<(usize, usize), Error>> + Send;

    // ---- one-time links -------------------------------

    /// Inserts a one-time link. Its user must exist.
    fn insert_one_time_link(
        &self,
        link: &OneTimeLink,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// The link of token hash `hash` (expired and used ones included; the
    /// caller checks).
    fn one_time_link(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<OneTimeLink>, Error>> + Send;
    /// Marks the unused link of hash `hash` used at `at` and returns it
    /// as stored now; `Ok(None)` when there is no such link or it was
    /// used already, so two uses of one link cannot both succeed
    /// (OTL-02). Expired links included: the caller checks.
    fn use_one_time_link(
        &self,
        hash: &TokenHash,
        at: Timestamp,
    ) -> impl Future<Output = Result<Option<OneTimeLink>, Error>> + Send;
    /// Deletes the links whose `expires_at` is not after `now`, used or
    /// not; how many.
    fn prune_one_time_links(
        &self,
        now: Timestamp,
    ) -> impl Future<Output = Result<usize, Error>> + Send;

    // ---- setup token (settings) ---------------------------------------------

    /// Stores `token` as the one live single-use login token, replacing the
    /// previous one if any (LOGN-01, LOGN-02). Its user must
    /// exist.
    fn put_setup_token(&self, token: &SetupToken)
    -> impl Future<Output = Result<(), Error>> + Send;
    /// The live setup token when its hash is `hash`.
    fn setup_token(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<SetupToken>, Error>> + Send;
    /// Trades the live setup token for `session` in one transaction: when
    /// the live token's hash is `hash` and its user is `session.user_id`,
    /// removes the token, inserts `session` and returns the token; two
    /// exchanges of one token cannot both succeed. `Ok(None)` and nothing
    /// changes otherwise; when `session` is refused (as by
    /// [`Store::insert_session`]) nothing changes and the token stays live
    /// (LOGN-01).
    fn exchange_setup_token(
        &self,
        hash: &TokenHash,
        session: &Session,
    ) -> impl Future<Output = Result<Option<SetupToken>, Error>> + Send;

    // ---- git connections -----------------------------------------------------

    /// Stores `connection` as the connection of its user on its host,
    /// replacing the previous one of that `(user_id, host)` if any.
    fn put_git_connection(
        &self,
        connection: &GitConnection,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// The connection of `user` on `host`.
    fn git_connection(
        &self,
        user: UserId,
        host: &Host,
    ) -> impl Future<Output = Result<Option<GitConnection>, Error>> + Send;
    /// The connections of `user`, by host.
    fn git_connections_of_user(
        &self,
        user: UserId,
    ) -> impl Future<Output = Result<Vec<GitConnection>, Error>> + Send;
    /// Deletes the connection of `user` on `host`.
    fn delete_git_connection(
        &self,
        user: UserId,
        host: &Host,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    // ---- projects ------------------------------------------------------------

    /// Inserts a project.
    fn insert_project(&self, project: &Project) -> impl Future<Output = Result<(), Error>> + Send;
    /// Replaces a project by id.
    fn update_project(&self, project: &Project) -> impl Future<Output = Result<(), Error>> + Send;
    /// A project by id.
    fn project(&self, id: ProjectId)
    -> impl Future<Output = Result<Option<Project>, Error>> + Send;
    /// A project by tenant (`None` without the `tenants` module) and name.
    fn project_by_name(
        &self,
        tenant: Option<TenantId>,
        name: &ProjectName,
    ) -> impl Future<Output = Result<Option<Project>, Error>> + Send;
    /// The project of `repo` on `git_host` (ENV-03).
    fn project_by_repo(
        &self,
        git_host: &Host,
        repo: &RepoRef,
    ) -> impl Future<Output = Result<Option<Project>, Error>> + Send;
    /// Every project, by name.
    fn projects(&self) -> impl Future<Output = Result<Vec<Project>, Error>> + Send;
    /// Deletes a project without environments or domains, with its deploy
    /// tokens and its secrets.
    fn delete_project(&self, id: ProjectId) -> impl Future<Output = Result<(), Error>> + Send;

    // ---- environments --------------------------------------------------------

    /// Inserts an environment.
    fn insert_environment(
        &self,
        environment: &Environment,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// Replaces an environment by id, all but its `protected` flag,
    /// which only [`Store::set_environment_protected`] changes: a writer
    /// holding an older copy of the environment cannot take it from
    /// behind the gate.
    fn update_environment(
        &self,
        environment: &Environment,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// Sets `protected` of the environment `id` (`lof protect`, `lof unprotect`).
    fn set_environment_protected(
        &self,
        id: EnvironmentId,
        protected: bool,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// Deletes an environment with its bindings, deploys and secrets.
    fn delete_environment(
        &self,
        id: EnvironmentId,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// An environment by id.
    fn environment(
        &self,
        id: EnvironmentId,
    ) -> impl Future<Output = Result<Option<Environment>, Error>> + Send;
    /// An environment by project and name.
    fn environment_by_name(
        &self,
        project: ProjectId,
        name: &EnvName,
    ) -> impl Future<Output = Result<Option<Environment>, Error>> + Send;
    /// The environments of `project`, by name.
    fn environments_of_project(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<Vec<Environment>, Error>> + Send;
    /// Every environment, by id.
    fn environments(&self) -> impl Future<Output = Result<Vec<Environment>, Error>> + Send;
    /// The environments of `project` pinned to `branch`, by name (CI
    /// routing, DEP-02).
    fn environments_on_branch(
        &self,
        project: ProjectId,
        branch: &Branch,
    ) -> impl Future<Output = Result<Vec<Environment>, Error>> + Send;

    // ---- domains and bindings ------------------------------------------------

    /// Inserts a domain.
    fn insert_domain(&self, domain: &Domain) -> impl Future<Output = Result<(), Error>> + Send;
    /// Replaces a domain by id.
    fn update_domain(&self, domain: &Domain) -> impl Future<Output = Result<(), Error>> + Send;
    /// Deletes an unbound domain.
    fn delete_domain(&self, id: DomainId) -> impl Future<Output = Result<(), Error>> + Send;
    /// A domain by id.
    fn domain(&self, id: DomainId) -> impl Future<Output = Result<Option<Domain>, Error>> + Send;
    /// A domain by host.
    fn domain_by_host(
        &self,
        host: &Host,
    ) -> impl Future<Output = Result<Option<Domain>, Error>> + Send;
    /// The domains of `project`, by host.
    fn domains_of_project(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<Vec<Domain>, Error>> + Send;
    /// Every domain, by host.
    fn domains(&self) -> impl Future<Output = Result<Vec<Domain>, Error>> + Send;
    /// Inserts a binding.
    fn insert_binding(&self, binding: &Binding) -> impl Future<Output = Result<(), Error>> + Send;
    /// Deletes a binding.
    fn delete_binding(&self, id: BindingId) -> impl Future<Output = Result<(), Error>> + Send;
    /// The binding of a domain.
    fn binding_by_domain(
        &self,
        domain: DomainId,
    ) -> impl Future<Output = Result<Option<Binding>, Error>> + Send;
    /// The bindings of `environment`, by host port.
    fn bindings_of_environment(
        &self,
        environment: EnvironmentId,
    ) -> impl Future<Output = Result<Vec<Binding>, Error>> + Send;
    /// Every binding, by host port (the input of `ProxyConfig`).
    fn bindings(&self) -> impl Future<Output = Result<Vec<Binding>, Error>> + Send;
    /// The host port for the next binding, from `range`, or `None` when
    /// every port of `range` is bound.
    ///
    /// Allocation goes round the range instead of reusing the lowest free
    /// port, so a port that was just freed is not handed out again at once
    /// (BND-02, BND-03). The store persists a cursor, the host port of the
    /// last [`Store::insert_binding`], and returns the first port after it
    /// that no binding uses, wrapping to the start of `range`. With no
    /// cursor, or one outside `range`, the search starts at
    /// `range.start()`.
    fn free_host_port(
        &self,
        range: &RangeInclusive<HostPort>,
    ) -> impl Future<Output = Result<Option<HostPort>, Error>> + Send;

    // ---- deploys -------------------------------------------------------------

    /// Inserts a deploy.
    fn insert_deploy(&self, deploy: &Deploy) -> impl Future<Output = Result<(), Error>> + Send;
    /// Replaces a deploy by id.
    fn update_deploy(&self, deploy: &Deploy) -> impl Future<Output = Result<(), Error>> + Send;
    /// A deploy by id.
    fn deploy(&self, id: DeployId) -> impl Future<Output = Result<Option<Deploy>, Error>> + Send;
    /// A deploy by environment and number.
    fn deploy_by_number(
        &self,
        environment: EnvironmentId,
        number: DeployNumber,
    ) -> impl Future<Output = Result<Option<Deploy>, Error>> + Send;
    /// The deploys of `environment`, newest first.
    fn deploys_of_environment(
        &self,
        environment: EnvironmentId,
    ) -> impl Future<Output = Result<Vec<Deploy>, Error>> + Send;
    /// One more than the highest number of `environment`, or
    /// [`DeployNumber::FIRST`].
    fn next_deploy_number(
        &self,
        environment: EnvironmentId,
    ) -> impl Future<Output = Result<DeployNumber, Error>> + Send;
    /// The newest succeeded deploy of `environment`.
    fn last_succeeded_deploy(
        &self,
        environment: EnvironmentId,
    ) -> impl Future<Output = Result<Option<Deploy>, Error>> + Send;
    /// Every deploy in one of `statuses`, oldest first (reconciliation).
    fn deploys_with_status(
        &self,
        statuses: &[DeployStatus],
    ) -> impl Future<Output = Result<Vec<Deploy>, Error>> + Send;
    /// Deletes all but the newest `keep` deploys of `environment`, never
    /// its `current_deploy_id`; returns how many were deleted (DEP-09).
    fn prune_deploys(
        &self,
        environment: EnvironmentId,
        keep: usize,
    ) -> impl Future<Output = Result<usize, Error>> + Send;

    // ---- deploy tokens -------------------------------------------------------

    /// Inserts a deploy token.
    fn insert_deploy_token(
        &self,
        token: &DeployToken,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// A token by id, live or revoked.
    fn deploy_token(
        &self,
        id: DeployTokenId,
    ) -> impl Future<Output = Result<Option<DeployToken>, Error>> + Send;
    /// The live token of `project`.
    fn live_deploy_token(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<Option<DeployToken>, Error>> + Send;
    /// The live token with this hash.
    fn live_deploy_token_by_hash(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<DeployToken>, Error>> + Send;
    /// Sets `revoked_at` of a token.
    fn revoke_deploy_token(
        &self,
        id: DeployTokenId,
        at: Timestamp,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// Revokes `old` and inserts `new` as the live token of the same
    /// project in one step: either both happen or neither does.
    fn replace_deploy_token(
        &self,
        old: DeployTokenId,
        new: &DeployToken,
        at: Timestamp,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    // ---- secrets -------------------------------------------------------------

    /// Stores `secret` as the current value of its `(scope, key)` and
    /// appends its version (`id`, `version`, `value`, `updated_by`,
    /// `updated_at`) to the history, in one step: either both happen or
    /// neither does. A new id inserts; a known id replaces that row.
    fn put_secret(&self, secret: &Secret) -> impl Future<Output = Result<(), Error>> + Send;
    /// The secret `key` of `scope`.
    fn secret(
        &self,
        scope: &SecretScope,
        key: &SecretKey,
    ) -> impl Future<Output = Result<Option<Secret>, Error>> + Send;
    /// The secrets of `scope`, by key.
    fn secrets_of_scope(
        &self,
        scope: &SecretScope,
    ) -> impl Future<Output = Result<Vec<Secret>, Error>> + Send;
    /// Deletes a secret with its versions.
    fn delete_secret(&self, id: SecretId) -> impl Future<Output = Result<(), Error>> + Send;
    /// The versions of a secret, newest first.
    fn secret_versions(
        &self,
        id: SecretId,
    ) -> impl Future<Output = Result<Vec<SecretVersion>, Error>> + Send;
    /// Deletes all but the newest `keep` versions of a secret; returns how
    /// many were deleted (SEC-03).
    fn prune_secret_versions(
        &self,
        id: SecretId,
        keep: usize,
    ) -> impl Future<Output = Result<usize, Error>> + Send;

    // ---- secret syncs --------------------------------

    /// Stores what the last sync of `sync.secret_id` to `sync.target`
    /// did, replacing the row of the same `(secret_id, target)`;
    /// [`Error::NotFound`] when the secret does not exist.
    fn put_secret_sync(
        &self,
        sync: &SecretSyncRecord,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// The last sync of `secret` to `target`, if any.
    fn secret_sync(
        &self,
        secret: SecretId,
        target: SyncTarget,
    ) -> impl Future<Output = Result<Option<SecretSyncRecord>, Error>> + Send;

    // ---- key-value of functions (`kv`) ----------------

    /// The value of `key` in `environment`'s key-value store.
    fn kv_get(
        &self,
        environment: EnvironmentId,
        key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send;
    /// Stores `value` as `key` of `environment`, replacing the earlier
    /// value; [`Error::NotFound`] when the environment does not exist.
    fn kv_set(
        &self,
        environment: EnvironmentId,
        key: &str,
        value: &[u8],
        at: Timestamp,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// Deletes `key` of `environment`; a key that is not there is no
    /// error.
    fn kv_delete(
        &self,
        environment: EnvironmentId,
        key: &str,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// The bytes `environment`'s keys and values take together (the
    /// limit `functions.kv_max_mb`, FN-04).
    fn kv_usage(
        &self,
        environment: EnvironmentId,
    ) -> impl Future<Output = Result<u64, Error>> + Send;

    // ---- audit ---------------------------------------------------------------

    /// Appends an audit entry.
    fn insert_audit(&self, entry: &Audit) -> impl Future<Output = Result<(), Error>> + Send;
    /// Every audit entry, oldest first.
    fn audit_log(&self) -> impl Future<Output = Result<Vec<Audit>, Error>> + Send;

    // ---- modules -------------------------------------

    /// Inserts an installed module.
    fn insert_module(
        &self,
        module: &InstalledModule,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// Replaces an installed module by name (`enabled`, `config`, `error`,
    /// `version`).
    fn update_module(
        &self,
        module: &InstalledModule,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// An installed module by name.
    fn module(
        &self,
        name: &ModuleName,
    ) -> impl Future<Output = Result<Option<InstalledModule>, Error>> + Send;
    /// Every installed module, in installation order.
    fn modules(&self) -> impl Future<Output = Result<Vec<InstalledModule>, Error>> + Send;
    /// Deletes an installed module by name; its storage stays (see
    /// [`ModuleStore::module_purge`](crate::ports::ModuleStore::module_purge)).
    fn delete_module(&self, name: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send;

    // ---- artifacts -------------------------------------

    /// Inserts `artifact` with its first version `first` in one
    /// transaction.
    fn insert_artifact(
        &self,
        artifact: &Artifact,
        first: &ArtifactVersion,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// Inserts `version` and replaces the row of its artifact by
    /// `artifact` (its new `current_version`, title, TTL) in one
    /// transaction.
    fn add_artifact_version(
        &self,
        artifact: &Artifact,
        version: &ArtifactVersion,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// Replaces the row of an artifact (title, visibility,
    /// `current_version`, `expires_at`, `updated_at`).
    fn update_artifact(
        &self,
        artifact: &Artifact,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// An artifact by id (expired ones included: the caller checks).
    fn artifact(
        &self,
        id: ArtifactId,
    ) -> impl Future<Output = Result<Option<Artifact>, Error>> + Send;
    /// The artifacts of `owner`, or of every user for `None`, newest
    /// first.
    fn artifacts(
        &self,
        owner: Option<UserId>,
    ) -> impl Future<Output = Result<Vec<Artifact>, Error>> + Send;
    /// The kept versions of an artifact with their files, oldest first.
    fn artifact_versions(
        &self,
        id: ArtifactId,
    ) -> impl Future<Output = Result<Vec<ArtifactVersion>, Error>> + Send;
    /// One version of an artifact with its files.
    fn artifact_version(
        &self,
        id: ArtifactId,
        number: VersionNumber,
    ) -> impl Future<Output = Result<Option<ArtifactVersion>, Error>> + Send;
    /// Deletes a version that is not the artifact's current one, with
    /// its files.
    fn delete_artifact_version(
        &self,
        id: ArtifactId,
        number: VersionNumber,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// Deletes all but the newest `keep` versions of an artifact, never
    /// its current one; the numbers deleted, oldest first (ART-05).
    fn prune_artifact_versions(
        &self,
        id: ArtifactId,
        keep: usize,
    ) -> impl Future<Output = Result<Vec<VersionNumber>, Error>> + Send;
    /// Deletes an artifact with every version and its files.
    fn delete_artifact(&self, id: ArtifactId) -> impl Future<Output = Result<(), Error>> + Send;
    /// The `private` artifacts whose `expires_at` is not after `now`,
    /// oldest first (ART-08).
    fn expired_artifacts(
        &self,
        now: Timestamp,
    ) -> impl Future<Output = Result<Vec<Artifact>, Error>> + Send;

    // ---- template cache -----------------------------

    /// Stores what `entry.user_id`'s connection read from a template
    /// repository, replacing the entry of the same `(user_id, host,
    /// repo)`; [`Error::NotFound`] when the user does not exist.
    fn put_cached_template(
        &self,
        entry: &CachedTemplate,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// The cached read of `repo` on `host` by `user`, however old (the
    /// catalogue decides whether it is fresh).
    fn cached_template(
        &self,
        user: UserId,
        host: &Host,
        repo: &RepoRef,
    ) -> impl Future<Output = Result<Option<CachedTemplate>, Error>> + Send;
}
