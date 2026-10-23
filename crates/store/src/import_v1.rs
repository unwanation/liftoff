//! Import of a liftoff v1 database into the core (without modules).
//!
//! The rules are on [`import_v1`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use liftoff_core::Error;
use liftoff_core::compose::parse_env;
use liftoff_core::domain::{
    Binding, BindingId, Branch, ComposeProject, ContainerPort, Cpus, Deploy, DeployId,
    DeployNumber, DeploySource, DeployStatus, DeployToken, DeployTokenId, Domain, DomainId,
    DomainKind, EnvKind, EnvName, EnvState, Environment, EnvironmentId, Host, HostPort, ImageTag,
    Limits, Login, MemoryMb, OidcIssuer, Pin, Project, ProjectId, ProjectName, RepoRef, SealedEnv,
    Secret, SecretId, SecretKey, SecretScope, ServiceName, TenantId, Timestamp, TokenHash, Trigger,
    UpdatedBy, User, UserId, UserRole, UserSource, VersionNumber, Visibility,
};
use liftoff_core::error::{Problem, ValidationError};
use liftoff_core::ports::{Clock, Rng, Secrets};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension as _, Row, Transaction, TransactionBehavior,
};
use time::format_description::well_known::Rfc3339;

use crate::SqliteStore;
use crate::rows::version_record;
use crate::sql::{
    Record, ToRecord as _, internal, number, number_opt, parsed, parsed_opt, timestamp,
    timestamp_opt, token_hash,
};

/// What an import needs besides the two databases.
#[derive(Debug, Clone)]
pub struct V1Import<K, R, C> {
    /// The OIDC issuer the v1 users logged in with (v1 `zitadel.issuer`,
    /// e.g. `https://auth.example.com`): their `source` becomes
    /// `oidc:<issuer>`.
    pub issuer: OidcIssuer,
    /// The forge the v1 repositories live on (v1 `forgejo.url`'s host, e.g.
    /// `git.example.com`): every project's `git_host`.
    pub git_host: Host,
    /// Opens the v1 env files and seals the secrets made from them (the
    /// master key both versions share).
    pub secrets: K,
    /// Ids of the records v1 did not have (projects, secrets).
    pub rng: R,
    /// The time of the secrets' version 1.
    pub clock: C,
}

/// How many records an import wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Users.
    pub users: usize,
    /// Projects (one per repository).
    pub projects: usize,
    /// Environments (one per target).
    pub environments: usize,
    /// Domains.
    pub domains: usize,
    /// Bindings.
    pub bindings: usize,
    /// Deploy tokens, live and revoked.
    pub deploy_tokens: usize,
    /// Deploys (at most 20 per environment).
    pub deploys: usize,
    /// Environment secrets.
    pub secrets: usize,
    /// Tenants put aside in `import_v1_tenants`.
    pub tenants: usize,
    /// The environments whose compose project and slice differ from their
    /// v1 target's, by id: they keep running under the v1 name until they
    /// are brought up again.
    pub renamed: Vec<Renamed>,
}

/// An imported environment that runs under another compose project than
/// its v1 target did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renamed {
    /// The environment (the v1 target's id).
    pub environment: EnvironmentId,
    /// The v1 compose project, `lf_<tenant>_<target>`.
    pub v1: String,
    /// The v2 compose project, `lf_<project>_<env>`.
    pub v2: ComposeProject,
}

/// A domain of a v1 tenant as `import_v1_tenant_domains` keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedDomain {
    /// The imported domain.
    pub id: DomainId,
    /// Its v1 Cloudflare DNS record, for the `dns.cloudflare` module.
    pub cf_record_id: Option<String>,
}

/// A member's role in a v1 tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantRole {
    /// `owner`.
    Owner,
    /// `member`.
    Member,
}

/// One membership of a v1 tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedMember {
    /// The (imported) user.
    pub user_id: UserId,
    /// Their role.
    pub role: TenantRole,
}

/// A v1 tenant as `import_v1_tenants` keeps it for the `tenants` module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedTenant {
    /// The v1 tenant id (what `projects.tenant_id` gets once the module
    /// adopts it).
    pub id: TenantId,
    /// The v1 slug.
    pub slug: String,
    /// The display name.
    pub name: String,
    /// Who created it.
    pub created_by: UserId,
    /// The v1 `max_targets` quota.
    pub max_targets: u32,
    /// The v1 per-target limits (`target_cpus_hundredths`,
    /// `target_memory_mb`).
    pub limits: Limits,
    /// When it was created.
    pub created_at: Timestamp,
    /// Its members, by user id.
    pub members: Vec<ImportedMember>,
    /// The imported projects of its targets, by id.
    pub projects: Vec<ProjectId>,
    /// The imported domains it owned, by id.
    pub domains: Vec<ImportedDomain>,
}

/// The v1 schema version (`PRAGMA user_version`) this import reads: the
/// three migrations of liftoffd 0.2.0.
const V1_SCHEMA_VERSION: i64 = 3;

/// How many deploys of each environment are kept.
const KEPT_DEPLOYS: usize = 20;

/// Imports the v1 database at `source` into `store`.
///
/// It reads the v1 schema (`users`, `tenants`, `memberships`,
/// `targets`, `domains`, `bindings`, `deploy_tokens`, `deploys`) from a
/// file opened read-only and writes the v2 records into an empty
/// [`SqliteStore`]:
///
/// - users keep their id, login, e-mail and role; `source =
///   oidc:<issuer>`, `subject` = the v1 `sub`;
/// - one project per distinct `targets.repo` on [`V1Import::git_host`].
///   The project of each tenant's home repository (that of its oldest
///   target) is named after the tenant's slug, so its environments keep
///   the v1 compose project and slice `lf_<tenant>_<target>` and go on
///   running across the switch-over. Any other repository's
///   project is named after the repository (`acme/web` → `web`); when
///   that name is taken (by a slug, or a project imported before it in
///   lower-case order of the repositories) it gets the tenant's slug as
///   suffix (`beta-acme`). Its environments are listed in
///   [`ImportReport::renamed`]. `tenant_id` stays empty without the
///   `tenants` module;
/// - every target becomes a `compose` environment of its repository's
///   project, keeping its id, name, pin, limits, state and current deploy;
///   a target behind the v1 SSO gate becomes `private` and `protected`
///   (behind the v2 gate), the others `shared`. Two tenants' targets of
///   one repository may share a name in v1; the one of the tenant the project is named after keeps it, any
///   other gets the tenant's slug as suffix (`qa-beta`) and is listed in
///   [`ImportReport::renamed`]. An unpinned target joins its tenant's home project; in a
///   tenant that pins nothing, the project of the repository of its
///   newest deploy that names one (none → refused, naming every such
///   target);
/// - domains and bindings keep their ids, hosts, kinds, verification,
///   services and ports, and the port cursor goes on; a domain belongs to
///   the project of the target it is bound to, an unbound one to its
///   tenant's home project (a tenant without targets → refused);
/// - deploy tokens keep their id, hash and revocation, on the project of
///   their repository, so a v1 token keeps authenticating (IMP-04); a
///   token of a repository without targets is skipped;
/// - the newest 20 deploys of each environment, and its current one,
///   keep their id, number and fields (the v1 `env` and `repo` columns
///   are dropped);
/// - the env of the newest succeeded deploy of each environment becomes
///   its environment secrets, version 1, `updated_by = import` (IMP-02);
///   an env key v1 accepted but that is not a [`SecretKey`] (`node_env`,
///   `_FOO`) refuses the import, naming every such key of every
///   environment;
/// - tenants and memberships, and which projects and domains belonged to
///   which tenant, go to `import_v1_tenants` for the `tenants` module
///   ([`SqliteStore::import_v1_tenants`], IMP-05), with each
///   domain's v1 Cloudflare record for the `dns.cloudflare` module;
/// - v1 sessions and SSO-gate records are not imported: users log in
///   again.
///
/// A database that already holds a user is refused (IMP-06): the import
/// runs once, on a fresh database, and writes everything or nothing.
///
/// # Errors
///
/// - [`Error::Conflict`] when `store` already holds a user (a second
///   `--import`, or a database `setup` already wrote); nothing changes;
/// - [`Error::Internal`] when `source` is not a readable v1 database, an
///   env does not open with `import.secrets`, or a v1 value does not fit
///   the v2 domain; nothing changes;
/// - whatever the store reports.
pub async fn import_v1<K, R, C>(
    source: impl AsRef<Path>,
    store: &SqliteStore,
    import: V1Import<K, R, C>,
) -> Result<ImportReport, Error>
where
    K: Secrets + 'static,
    R: Rng + 'static,
    C: Clock + 'static,
{
    let source = source.as_ref().to_owned();
    store
        .run(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            let has_user: bool = tx
                .query_row("SELECT EXISTS (SELECT 1 FROM users)", [], |row| row.get(0))
                .map_err(internal)?;
            if has_user {
                return Err(Error::conflict(
                    "the database already holds users: a v1 import runs once, on a new database",
                ));
            }
            let mut v1 = open_v1(&source)?;
            let snapshot = v1.transaction().map_err(|error| v1_error("open", &error))?;
            let plan = Plan::read(&snapshot, &import)?;
            drop(snapshot);
            let report = plan.write(&tx)?;
            tx.commit().map_err(internal)?;
            tracing::info!(
                source = %source.display(),
                users = report.users,
                projects = report.projects,
                environments = report.environments,
                deploys = report.deploys,
                secrets = report.secrets,
                tenants = report.tenants,
                "imported the v1 database"
            );
            Ok(report)
        })
        .await
}

impl SqliteStore {
    /// The v1 tenants an import put aside, by slug (IMP-05); empty when
    /// the database was not imported.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when SQLite fails.
    pub async fn import_v1_tenants(&self) -> Result<Vec<ImportedTenant>, Error> {
        self.run(|conn| read_tenants(conn).map_err(internal)).await
    }
}

// ---- reading v1 ------------------------------------------------------------------

/// The v1 file, read-only: the import never writes to it (IMP-06).
fn open_v1(source: &Path) -> Result<Connection, Error> {
    let conn = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| v1_error(&format!("open {}", source.display()), &error))?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| v1_error("read the schema version", &error))?;
    let has_targets: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'targets')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| v1_error("read the schema", &error))?;
    if version != V1_SCHEMA_VERSION || !has_targets {
        return Err(Error::internal_msg(format!(
            "{} is not a liftoff v1 database (schema version {version}, expected {V1_SCHEMA_VERSION})",
            source.display()
        )));
    }
    Ok(conn)
}

/// A failure while reading or converting the v1 rows of `what`.
fn v1_error(what: &str, error: &rusqlite::Error) -> Error {
    tracing::error!(%error, what, "v1 import failure");
    Error::internal_msg(format!("v1 import: {what}: {error}"))
}

/// A v1 value that has no v2 form: the import is refused.
fn misfit(what: impl core::fmt::Display) -> Error {
    tracing::error!(%what, "v1 import refused");
    Error::internal_msg(format!("v1 import: {what}"))
}

/// All rows of `sql` through `map`; `what` names them in errors.
fn rows<T>(
    conn: &Connection,
    what: &str,
    sql: &str,
    args: impl rusqlite::Params,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, Error> {
    let read = || -> rusqlite::Result<Vec<T>> {
        let mut statement = conn.prepare(sql)?;
        let found = statement.query_map(args, map)?;
        found.collect()
    };
    read().map_err(|error| v1_error(what, &error))
}

fn limits(row: &Row<'_>, cpus: &str, memory: &str) -> rusqlite::Result<Limits> {
    Ok(Limits {
        cpus: number(row, cpus, Cpus::from_hundredths)?,
        memory_mb: number(row, memory, MemoryMb::new)?,
    })
}

fn tenant_role(raw: &str) -> Result<TenantRole, ValidationError> {
    match raw {
        "owner" => Ok(TenantRole::Owner),
        "member" => Ok(TenantRole::Member),
        _ => Err(ValidationError::new(
            "role",
            Problem::Pattern {
                expected: "owner or member",
            },
        )),
    }
}

const fn tenant_role_text(role: TenantRole) -> &'static str {
    match role {
        TenantRole::Owner => "owner",
        TenantRole::Member => "member",
    }
}

/// A v1 target: the environment it becomes once its project is known.
struct Target {
    id: EnvironmentId,
    name: EnvName,
    pin: Option<Pin>,
    limits: Limits,
    state: EnvState,
    current_deploy_id: Option<DeployId>,
    visibility: Visibility,
    /// Behind the v1 SSO gate: behind the gate of v2 (the `tenants` module decides who passes, the admin alone without it).
    protected: bool,
    created_at: Timestamp,
    tenant: TenantId,
}

impl Target {
    fn environment(&self, project_id: ProjectId) -> Environment {
        Environment {
            id: self.id,
            project_id,
            name: self.name.clone(),
            kind: EnvKind::Compose,
            pin: self.pin.clone(),
            owner_user_id: None,
            limits: self.limits,
            state: self.state,
            current_deploy_id: self.current_deploy_id,
            visibility: self.visibility,
            protected: self.protected,
            created_at: self.created_at,
        }
    }
}

/// A v1 domain: the v2 record once its project is known.
struct V1Domain {
    id: DomainId,
    host: Host,
    kind: DomainKind,
    verified_at: Option<Timestamp>,
    proxied: bool,
    created_at: Timestamp,
    tenant: TenantId,
    cf_record_id: Option<String>,
}

impl V1Domain {
    fn domain(self, project_id: ProjectId) -> Domain {
        Domain {
            id: self.id,
            // Empty without the `tenants` module.
            tenant_id: None,
            project_id,
            host: self.host,
            kind: self.kind,
            verified_at: self.verified_at,
            record_lost_at: None,
            proxied: self.proxied,
            created_at: self.created_at,
        }
    }
}

/// A v1 deploy token, still on its repository.
struct V1Token {
    id: DeployTokenId,
    token_hash: TokenHash,
    created_by: UserId,
    created_at: Timestamp,
    revoked_at: Option<Timestamp>,
    repo: RepoRef,
}

impl V1Token {
    fn token(self, project_id: ProjectId) -> DeployToken {
        DeployToken {
            id: self.id,
            project_id,
            token_hash: self.token_hash,
            created_by: self.created_by,
            created_at: self.created_at,
            revoked_at: self.revoked_at,
        }
    }
}

/// The v2 records an import writes, all read before anything is written.
struct Plan {
    users: Vec<User>,
    tenants: Vec<ImportedTenant>,
    projects: Vec<Project>,
    environments: Vec<Environment>,
    domains: Vec<Domain>,
    bindings: Vec<Binding>,
    port_cursor: Option<u16>,
    deploys: Vec<Deploy>,
    tokens: Vec<DeployToken>,
    secrets: Vec<Secret>,
    renamed: Vec<Renamed>,
}

impl Plan {
    fn read<K: Secrets, R: Rng, C: Clock>(
        v1: &Connection,
        import: &V1Import<K, R, C>,
    ) -> Result<Self, Error> {
        let users = read_users(v1, &import.issuer)?;
        let mut tenants = read_v1_tenants(v1)?;
        let targets = read_targets(v1)?;
        let (projects, home) = place_projects(&targets, &tenants, import)?;
        let project_of_repo: BTreeMap<&RepoRef, ProjectId> = projects
            .iter()
            .map(|project| (&project.repo, project.id))
            .collect();
        let project_of = |repo: &RepoRef| {
            project_of_repo
                .get(repo)
                .copied()
                .ok_or_else(|| misfit(format!("no project for {repo}")))
        };
        let mut environments = Vec::with_capacity(targets.len());
        for (target, repo) in &targets {
            environments.push(target.environment(project_of(repo)?));
        }
        settle_env_names(&targets, &mut environments, &projects, &tenants)?;
        let renamed = renamed(&targets, &environments, &projects, &tenants);
        for tenant in &mut tenants {
            let mut owned = Vec::new();
            for (target, repo) in &targets {
                if target.tenant == tenant.id {
                    owned.push(project_of(repo)?);
                }
            }
            owned.sort_unstable();
            owned.dedup();
            tenant.projects = owned;
        }

        let bindings = read_bindings(v1)?;
        let domains = place_domains(v1, &environments, &bindings, &home, &mut tenants)?;
        let port_cursor: Option<u16> = v1
            .query_row(
                "SELECT host_port FROM port_cursor WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| v1_error("port cursor", &error))?;

        let tokens = place_tokens(v1, &project_of_repo)?;
        let (deploys, secrets) = read_history(v1, &environments, import)?;

        Ok(Self {
            users,
            tenants,
            projects,
            environments,
            domains,
            bindings,
            port_cursor,
            deploys,
            tokens,
            secrets,
            renamed,
        })
    }

    fn write(&self, tx: &Transaction<'_>) -> Result<ImportReport, Error> {
        let insert = |record: Record| -> Result<(), Error> {
            record
                .insert(tx)
                .map_err(|error| v1_error(&format!("write {}", record.what), &error))
        };
        for user in &self.users {
            insert(user.record()?)?;
        }
        for project in &self.projects {
            insert(project.record()?)?;
        }
        for environment in &self.environments {
            insert(environment.record()?)?;
        }
        for domain in &self.domains {
            insert(domain.record()?)?;
        }
        for binding in &self.bindings {
            insert(binding.record()?)?;
        }
        if let Some(port) = self.port_cursor {
            let _cursor: usize = tx
                .execute(
                    "INSERT INTO port_cursor (id, host_port) VALUES (1, ?)",
                    [port],
                )
                .map_err(internal)?;
        }
        for deploy in &self.deploys {
            insert(deploy.record()?)?;
        }
        for token in &self.tokens {
            insert(token.record()?)?;
        }
        for secret in &self.secrets {
            insert(secret.record()?)?;
            insert(version_record(secret)?)?;
        }
        write_tenants(tx, &self.tenants).map_err(internal)?;
        Ok(ImportReport {
            users: self.users.len(),
            projects: self.projects.len(),
            environments: self.environments.len(),
            domains: self.domains.len(),
            bindings: self.bindings.len(),
            deploy_tokens: self.tokens.len(),
            deploys: self.deploys.len(),
            secrets: self.secrets.len(),
            tenants: self.tenants.len(),
            renamed: self.renamed.clone(),
        })
    }
}

/// The v1 users as OIDC users of `issuer` with their v1 `sub`.
fn read_users(v1: &Connection, issuer: &OidcIssuer) -> Result<Vec<User>, Error> {
    let source = UserSource::Oidc(issuer.clone());
    rows(
        v1,
        "users",
        "SELECT id, sub, login, email, role, created_at FROM users ORDER BY id",
        [],
        |row| {
            Ok(User {
                id: parsed(row, "id", UserId::parse)?,
                login: parsed(row, "login", Login::new)?,
                email: row.get("email")?,
                role: parsed(row, "role", UserRole::parse)?,
                source: source.clone(),
                subject: Some(row.get("sub")?),
                created_at: timestamp(row, "created_at")?,
            })
        },
    )
}

/// The v1 domains on their projects, each noted on its tenant with its v1
/// Cloudflare record.
fn place_domains(
    v1: &Connection,
    environments: &[Environment],
    bindings: &[Binding],
    home: &Homes,
    tenants: &mut [ImportedTenant],
) -> Result<Vec<Domain>, Error> {
    let mut domains = Vec::new();
    for v1_domain in read_domains(v1)? {
        let bound = bindings
            .iter()
            .find(|binding| binding.domain_id == v1_domain.id)
            .and_then(|binding| {
                environments
                    .iter()
                    .find(|environment| environment.id == binding.environment_id)
            })
            .map(|environment| environment.project_id);
        // An unbound domain goes to its tenant's home project.
        let project_id = bound
            .or_else(|| home.get(&v1_domain.tenant).copied())
            .ok_or_else(|| {
                misfit(format!(
                    "domain {} is bound to nothing and its tenant has no target to put it \
                     beside; bind or delete it in v1 first",
                    v1_domain.host
                ))
            })?;
        if let Some(tenant) = tenants
            .iter_mut()
            .find(|tenant| tenant.id == v1_domain.tenant)
        {
            tenant.domains.push(ImportedDomain {
                id: v1_domain.id,
                cf_record_id: v1_domain.cf_record_id.clone(),
            });
        }
        domains.push(v1_domain.domain(project_id));
    }
    for tenant in tenants {
        tenant.domains.sort_unstable_by_key(|domain| domain.id);
    }
    Ok(domains)
}

/// The v1 deploy tokens on the projects of their repositories.
fn place_tokens(
    v1: &Connection,
    project_of_repo: &BTreeMap<&RepoRef, ProjectId>,
) -> Result<Vec<DeployToken>, Error> {
    let mut tokens = Vec::new();
    for v1_token in read_tokens(v1)? {
        if let Some(&project_id) = project_of_repo.get(&v1_token.repo) {
            tokens.push(v1_token.token(project_id));
        } else {
            tracing::warn!(
                token = %v1_token.id,
                repo = %v1_token.repo,
                "skipped the deploy token of a repository without targets"
            );
        }
    }
    Ok(tokens)
}

/// The deploys each environment keeps (the newest [`KEPT_DEPLOYS`] and
/// the current one) and the secrets from the env of its newest succeeded
/// deploy.
fn read_history<K: Secrets, R: Rng, C: Clock>(
    v1: &Connection,
    environments: &[Environment],
    import: &V1Import<K, R, C>,
) -> Result<(Vec<Deploy>, Vec<Secret>), Error> {
    let mut deploys = Vec::new();
    let mut secrets = Vec::new();
    let mut misfits = Vec::new();
    let now = import.clock.now();
    for environment in environments {
        let history = read_deploys(v1, environment.id)?;
        if let Some((deploy, env)) = history
            .iter()
            .find(|(deploy, _env)| deploy.status == DeployStatus::Succeeded)
        {
            for secret in env_secrets(environment.id, deploy, env, import, now)? {
                match secret {
                    Ok(secret) => secrets.push(secret),
                    Err(misfit) => misfits.push(misfit),
                }
            }
        }
        deploys.extend(
            history
                .into_iter()
                .enumerate()
                .filter(|(index, (deploy, _env))| {
                    *index < KEPT_DEPLOYS || environment.current_deploy_id == Some(deploy.id)
                })
                .map(|(_index, (deploy, _env))| deploy),
        );
    }
    if !misfits.is_empty() {
        return Err(misfit(format!(
            "env keys that are not secret keys (^[A-Z][A-Z0-9_]{{0,63}}$): {}; rename them \
             in v1 and deploy again first",
            misfits.join("; ")
        )));
    }
    Ok((deploys, secrets))
}

/// The v1 tenants and their members; projects and domains come later.
fn read_v1_tenants(v1: &Connection) -> Result<Vec<ImportedTenant>, Error> {
    let mut tenants = rows(
        v1,
        "tenants",
        "SELECT * FROM tenants ORDER BY slug",
        [],
        |row| {
            Ok(ImportedTenant {
                id: parsed(row, "id", TenantId::parse)?,
                slug: row.get("slug")?,
                name: row.get("name")?,
                created_by: parsed(row, "created_by", UserId::parse)?,
                max_targets: row.get("max_targets")?,
                limits: limits(row, "target_cpus_hundredths", "target_memory_mb")?,
                created_at: timestamp(row, "created_at")?,
                members: Vec::new(),
                projects: Vec::new(),
                domains: Vec::new(),
            })
        },
    )?;
    for tenant in &mut tenants {
        tenant.members = rows(
            v1,
            "memberships",
            "SELECT user_id, role FROM memberships WHERE tenant_id = ? ORDER BY user_id",
            [tenant.id.to_string()],
            |row| {
                Ok(ImportedMember {
                    user_id: parsed(row, "user_id", UserId::parse)?,
                    role: parsed(row, "role", tenant_role)?,
                })
            },
        )?;
    }
    Ok(tenants)
}

/// The v1 targets as compose environments (without their project yet),
/// oldest first, each with the repository whose project it joins.
///
/// An unpinned target joins its tenant's home repository (that of the
/// tenant's oldest pinned target), so its compose name stays; in a tenant
/// that pins nothing it takes the repository of its newest deploy that
/// names one. A target left without a repository refuses the import,
/// naming every such target.
fn read_targets(v1: &Connection) -> Result<Vec<(Target, RepoRef)>, Error> {
    let read = rows(
        v1,
        "targets",
        "SELECT * FROM targets ORDER BY created_at, id",
        [],
        |row| {
            let pin = match (
                parsed_opt(row, "branch", Branch::new)?,
                parsed_opt(row, "pinned_by", UserId::parse)?,
            ) {
                (Some(branch), Some(pinned_by)) => Some(Pin { branch, pinned_by }),
                _ => None,
            };
            let sso: bool = row.get("sso")?;
            let target = Target {
                id: parsed(row, "id", EnvironmentId::parse)?,
                name: parsed(row, "name", EnvName::new)?,
                pin,
                limits: limits(row, "cpus_hundredths", "memory_mb")?,
                state: parsed(row, "state", EnvState::parse)?,
                current_deploy_id: parsed_opt(row, "current_deploy_id", DeployId::parse)?,
                // A target behind the v1 SSO gate stays behind login.
                visibility: if sso {
                    Visibility::Private
                } else {
                    Visibility::Shared
                },
                protected: sso,
                created_at: timestamp(row, "created_at")?,
                tenant: parsed(row, "tenant_id", TenantId::parse)?,
            };
            Ok((target, parsed_opt(row, "repo", RepoRef::new)?))
        },
    )?;
    let pinning: BTreeSet<TenantId> = read
        .iter()
        .filter(|(_target, repo)| repo.is_some())
        .map(|(target, _repo)| target.tenant)
        .collect();
    let mut found = Vec::with_capacity(read.len());
    for (target, repo) in read {
        let repo = match repo {
            None if !pinning.contains(&target.tenant) => deployed_repo(v1, target.id)?,
            repo => repo,
        };
        found.push((target, repo));
    }
    let mut home: BTreeMap<TenantId, RepoRef> = BTreeMap::new();
    for (target, repo) in &found {
        if let Some(repo) = repo {
            let _oldest = home.entry(target.tenant).or_insert_with(|| repo.clone());
        }
    }
    let mut placed = Vec::with_capacity(found.len());
    let mut homeless = Vec::new();
    for (target, repo) in found {
        match repo.or_else(|| home.get(&target.tenant).cloned()) {
            Some(repo) => placed.push((target, repo)),
            None => homeless.push(format!("{} ({})", target.name, target.id)),
        }
    }
    if !homeless.is_empty() {
        return Err(misfit(format!(
            "no repository is known for targets {}: their tenant pins none and no deploy names \
             one; pin or delete them in v1 first",
            homeless.join(", ")
        )));
    }
    Ok(placed)
}

/// The repository of the newest deploy of `target` that names one.
fn deployed_repo(v1: &Connection, target: EnvironmentId) -> Result<Option<RepoRef>, Error> {
    let found = rows(
        v1,
        "deploys",
        "SELECT repo FROM deploys WHERE target_id = ? AND repo IS NOT NULL \
         ORDER BY number DESC LIMIT 1",
        [target.to_string()],
        |row| parsed(row, "repo", RepoRef::new),
    )?;
    Ok(found.into_iter().next())
}

/// The project each tenant's unbound domains go to: that of the
/// repository of its oldest target.
type Homes = BTreeMap<TenantId, ProjectId>;

/// One project per repository, in lower-case order of the repositories,
/// and each tenant's home project.
///
/// The project of a tenant's home repository (that of its oldest target)
/// is named after the tenant's slug, so its environments keep the v1
/// compose project and slice `lf_<tenant>_<target>`; any
/// other repository's project is named by [`project_name`]. The tenant,
/// creation time and creator (who pinned it, else the tenant's creator)
/// come from the repository's oldest target.
fn place_projects<K: Secrets, R: Rng, C: Clock>(
    targets: &[(Target, RepoRef)],
    tenants: &[ImportedTenant],
    import: &V1Import<K, R, C>,
) -> Result<(Vec<Project>, Homes), Error> {
    let mut first_target: BTreeMap<&RepoRef, &Target> = BTreeMap::new();
    let mut home_repo: BTreeMap<TenantId, &RepoRef> = BTreeMap::new();
    for (target, repo) in targets {
        let _oldest = first_target.entry(repo).or_insert(target);
        let _home = home_repo.entry(target.tenant).or_insert(repo);
    }
    let tenant_of = |target: &Target| {
        tenants
            .iter()
            .find(|tenant| tenant.id == target.tenant)
            .ok_or_else(|| misfit(format!("tenant of target {}", target.id)))
    };
    // The slugs first, so no repository name takes a tenant's.
    let mut slug_names: BTreeMap<&RepoRef, ProjectName> = BTreeMap::new();
    for tenant in tenants {
        let (Some(&repo), Ok(name)) = (home_repo.get(&tenant.id), ProjectName::new(&tenant.slug))
        else {
            continue;
        };
        if !slug_names.contains_key(repo) && !slug_names.values().any(|taken| *taken == name) {
            let _new = slug_names.insert(repo, name);
        }
    }
    let mut taken: BTreeSet<ProjectName> = slug_names.values().cloned().collect();
    let mut projects = Vec::new();
    for (repo, target) in first_target {
        let tenant = tenant_of(target)?;
        let name = if let Some(name) = slug_names.get(repo) {
            name.clone()
        } else {
            let name = project_name(repo, &tenant.slug, &taken)?;
            let _new = taken.insert(name.clone());
            name
        };
        projects.push(Project {
            id: ProjectId::new(import.rng.ulid()),
            tenant_id: None,
            name,
            git_host: import.git_host.clone(),
            repo: repo.clone(),
            manifest: None,
            template: None,
            created_by: target
                .pin
                .as_ref()
                .map_or(tenant.created_by, |pin| pin.pinned_by),
            created_at: target.created_at,
        });
    }
    let homes = home_repo
        .into_iter()
        .filter_map(|(tenant, repo)| {
            projects
                .iter()
                .find(|project| project.repo == *repo)
                .map(|project| (tenant, project.id))
        })
        .collect();
    Ok((projects, homes))
}

/// Makes every environment name unique within its project.
///
/// v1 target names are unique per tenant only, so the targets of two
/// tenants pinning one repository can share a name. The target of the
/// tenant the project is named after keeps its name (and its v1 compose
/// project); any other clashing one, oldest first, gets its tenant's slug
/// as suffix (`qa` → `qa-beta`), and so shows up in
/// [`ImportReport::renamed`].
fn settle_env_names(
    targets: &[(Target, RepoRef)],
    environments: &mut [Environment],
    projects: &[Project],
    tenants: &[ImportedTenant],
) -> Result<(), Error> {
    let slug_of = |tenant: TenantId| {
        tenants
            .iter()
            .find(|found| found.id == tenant)
            .map_or("", |found| found.slug.as_str())
    };
    let names_project = |target: &Target, environment: &Environment| {
        projects
            .iter()
            .find(|project| project.id == environment.project_id)
            .is_some_and(|project| project.name.as_str() == slug_of(target.tenant))
    };
    let mut pairs: Vec<(&Target, &mut Environment)> = targets
        .iter()
        .map(|(target, _repo)| target)
        .zip(environments.iter_mut())
        .collect();
    // Stable: oldest first within each group.
    pairs.sort_by_key(|(target, environment)| !names_project(target, environment));
    let mut taken: BTreeSet<(ProjectId, EnvName)> = BTreeSet::new();
    for (target, environment) in pairs {
        let project_id = environment.project_id;
        if taken.insert((project_id, environment.name.clone())) {
            continue;
        }
        let name = free_name(
            environment.name.as_str(),
            slug_of(target.tenant),
            EnvName::new,
            |candidate| !taken.contains(&(project_id, candidate.clone())),
        )
        .ok_or_else(|| {
            misfit(format!(
                "no free environment name for target {} ({})",
                target.name, target.id
            ))
        })?;
        let _new = taken.insert((project_id, name.clone()));
        environment.name = name;
    }
    Ok(())
}

/// The environments whose compose project differs from their v1 target's
/// `lf_<tenant>_<target>`, each also logged: they keep running under the
/// v1 name until they are brought up again.
fn renamed(
    targets: &[(Target, RepoRef)],
    environments: &[Environment],
    projects: &[Project],
    tenants: &[ImportedTenant],
) -> Vec<Renamed> {
    let mut renamed = Vec::new();
    for ((target, _repo), environment) in targets.iter().zip(environments) {
        let slug = tenants
            .iter()
            .find(|tenant| tenant.id == target.tenant)
            .map(|tenant| tenant.slug.as_str());
        let project = projects
            .iter()
            .find(|project| project.id == environment.project_id);
        let (Some(slug), Some(project)) = (slug, project) else {
            continue;
        };
        let v1 = format!("lf_{slug}_{}", target.name);
        let v2 = ComposeProject::new(&project.name, &environment.name);
        if v2.as_str() != v1 {
            tracing::warn!(
                environment = %environment.id,
                v1 = %v1,
                v2 = %v2,
                "the imported environment runs under another compose project: bring it up \
                 again once v1 is stopped"
            );
            renamed.push(Renamed {
                environment: environment.id,
                v1,
                v2,
            });
        }
    }
    renamed
}

/// The name of the project of `repo`: the repository's name made a valid
/// project name, then with `-<tenant slug>`, then with a counter, until
/// it is free.
fn project_name(
    repo: &RepoRef,
    slug: &str,
    taken: &BTreeSet<ProjectName>,
) -> Result<ProjectName, Error> {
    free_name(&label(repo.name()), slug, ProjectName::new, |candidate| {
        !taken.contains(candidate)
    })
    .ok_or_else(|| misfit(format!("no free project name for {repo}")))
}

/// The first of `base`, `base-<slug>`, `base-<slug>-2`, … (each within
/// [`LABEL_MAX`] characters) that `new` accepts and `is_free`; `None`
/// when none up to 99 is.
fn free_name<T, E>(
    base: &str,
    slug: &str,
    new: impl Fn(&str) -> Result<T, E>,
    is_free: impl Fn(&T) -> bool,
) -> Option<T> {
    let suffixed = |suffix: String| {
        let room = LABEL_MAX.saturating_sub(suffix.len());
        let head = base.get(..room.min(base.len())).unwrap_or(base);
        label(&format!("{}{suffix}", head.trim_end_matches('-')))
    };
    core::iter::once(base.to_owned())
        .chain(core::iter::once(suffixed(format!("-{}", label(slug)))))
        .chain((2..100_u32).map(|n| suffixed(format!("-{}-{n}", label(slug)))))
        .filter_map(|candidate| new(&candidate).ok())
        .find(|candidate| is_free(candidate))
}

/// The longest project name (`^[a-z0-9][a-z0-9-]{0, 31}$`).
const LABEL_MAX: usize = 32;

/// `raw` lower-cased, every other character a `-`, no leading `-`, at
/// most [`LABEL_MAX`] characters; `project` when nothing is left.
fn label(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed: String = mapped
        .trim_start_matches('-')
        .chars()
        .take(LABEL_MAX)
        .collect();
    if trimmed.is_empty() {
        "project".to_owned()
    } else {
        trimmed
    }
}

/// The v1 domains, by id.
fn read_domains(v1: &Connection) -> Result<Vec<V1Domain>, Error> {
    rows(
        v1,
        "domains",
        "SELECT * FROM domains ORDER BY id",
        [],
        |row| {
            Ok(V1Domain {
                id: parsed(row, "id", DomainId::parse)?,
                host: parsed(row, "host", Host::new)?,
                kind: parsed(row, "kind", DomainKind::parse)?,
                verified_at: timestamp_opt(row, "verified_at")?,
                proxied: row.get("proxied")?,
                created_at: timestamp(row, "created_at")?,
                tenant: parsed(row, "tenant_id", TenantId::parse)?,
                cf_record_id: row.get("cf_record_id")?,
            })
        },
    )
}

fn read_bindings(v1: &Connection) -> Result<Vec<Binding>, Error> {
    rows(
        v1,
        "bindings",
        "SELECT * FROM bindings ORDER BY host_port",
        [],
        |row| {
            Ok(Binding {
                id: parsed(row, "id", BindingId::parse)?,
                environment_id: parsed(row, "target_id", EnvironmentId::parse)?,
                domain_id: parsed(row, "domain_id", DomainId::parse)?,
                service: parsed(row, "service", ServiceName::new)?,
                container_port: number(row, "container_port", ContainerPort::new)?,
                host_port: number(row, "host_port", HostPort::new)?,
            })
        },
    )
}

fn read_tokens(v1: &Connection) -> Result<Vec<V1Token>, Error> {
    rows(
        v1,
        "deploy tokens",
        "SELECT * FROM deploy_tokens ORDER BY id",
        [],
        |row| {
            Ok(V1Token {
                id: parsed(row, "id", DeployTokenId::parse)?,
                token_hash: token_hash(row, "token_hash")?,
                created_by: parsed(row, "created_by", UserId::parse)?,
                created_at: timestamp(row, "created_at")?,
                revoked_at: timestamp_opt(row, "revoked_at")?,
                repo: parsed(row, "repo", RepoRef::new)?,
            })
        },
    )
}

/// Every deploy of `target`, newest first, with its sealed v1 env.
fn read_deploys(v1: &Connection, target: EnvironmentId) -> Result<Vec<(Deploy, SealedEnv)>, Error> {
    rows(
        v1,
        "deploys",
        "SELECT * FROM deploys WHERE target_id = ? ORDER BY number DESC",
        [target.to_string()],
        |row| {
            let trigger_kind: String = row.get("trigger_kind")?;
            let triggered_by = if trigger_kind == "user" {
                Trigger::User(parsed(row, "trigger_id", UserId::parse)?)
            } else {
                Trigger::Token(parsed(row, "trigger_id", DeployTokenId::parse)?)
            };
            let deploy = Deploy {
                id: parsed(row, "id", DeployId::parse)?,
                environment_id: target,
                number: number(row, "number", DeployNumber::new)?,
                source: parsed(row, "source", DeploySource::parse)?,
                git_ref: parsed_opt(row, "git_ref", Branch::new)?,
                tag: parsed(row, "tag", ImageTag::new)?,
                compose: row.get("compose")?,
                // v1 deployed compose only.
                component_hash: None,
                status: parsed(row, "status", DeployStatus::parse)?,
                log: row.get("log")?,
                error: row.get("error")?,
                rolled_back_to: number_opt(row, "rolled_back_to", DeployNumber::new)?,
                triggered_by,
                started_at: timestamp_opt(row, "started_at")?,
                finished_at: timestamp_opt(row, "finished_at")?,
            };
            Ok((deploy, SealedEnv::new(row.get("env")?)))
        },
    )
}

/// The env of `deploy` as version 1 of the environment's secrets, by
/// `import`, sealed again (IMP-02); a key that is not a [`SecretKey`] is
/// an `Err` naming it, so the caller can name every one at once.
fn env_secrets<K: Secrets, R: Rng, C: Clock>(
    environment: EnvironmentId,
    deploy: &Deploy,
    env: &SealedEnv,
    import: &V1Import<K, R, C>,
    now: Timestamp,
) -> Result<Vec<Result<Secret, String>>, Error> {
    let plain = import.secrets.decrypt(env).map_err(|error| {
        misfit(format!(
            "env of deploy {} of environment {environment} does not open with the master \
             key: {error}",
            deploy.id
        ))
    })?;
    let vars =
        parse_env(&plain).map_err(|why| misfit(format!("env of deploy {}: {why}", deploy.id)))?;
    vars.into_iter()
        .map(|(key, value)| {
            let Ok(key) = SecretKey::new(&key) else {
                return Ok(Err(format!(
                    "{key} (deploy {} of environment {environment})",
                    deploy.id
                )));
            };
            Ok(Ok(Secret {
                id: SecretId::new(import.rng.ulid()),
                scope: SecretScope::Environment(environment),
                key,
                value: import.secrets.encrypt(&value)?,
                version: VersionNumber::FIRST,
                ci: false,
                updated_by: UpdatedBy::Import,
                updated_at: now,
                created_at: now,
            }))
        })
        .collect()
}

// ---- import_v1_tenants -----------------------------------------------------------

fn write_tenants(tx: &Transaction<'_>, tenants: &[ImportedTenant]) -> rusqlite::Result<()> {
    for tenant in tenants {
        let created_at = tenant
            .created_at
            .format(&Rfc3339)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let _row: usize = tx.execute(
            "INSERT INTO import_v1_tenants (id, slug, name, created_by, max_targets, \
             cpus_hundredths, memory_mb, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                tenant.id.to_string(),
                tenant.slug,
                tenant.name,
                tenant.created_by.to_string(),
                tenant.max_targets,
                tenant.limits.cpus.hundredths(),
                tenant.limits.memory_mb.get(),
                created_at,
            ],
        )?;
        for member in &tenant.members {
            let _row: usize = tx.execute(
                "INSERT INTO import_v1_tenant_members (tenant_id, user_id, role) VALUES (?, ?, ?)",
                rusqlite::params![
                    tenant.id.to_string(),
                    member.user_id.to_string(),
                    tenant_role_text(member.role)
                ],
            )?;
        }
        for project in &tenant.projects {
            let _row: usize = tx.execute(
                "INSERT INTO import_v1_tenant_projects (tenant_id, project_id) VALUES (?, ?)",
                [tenant.id.to_string(), project.to_string()],
            )?;
        }
        for domain in &tenant.domains {
            let _row: usize = tx.execute(
                "INSERT INTO import_v1_tenant_domains (tenant_id, domain_id, cf_record_id) \
                 VALUES (?, ?, ?)",
                rusqlite::params![
                    tenant.id.to_string(),
                    domain.id.to_string(),
                    domain.cf_record_id
                ],
            )?;
        }
    }
    Ok(())
}

fn read_tenants(conn: &Connection) -> rusqlite::Result<Vec<ImportedTenant>> {
    let mut tenants: Vec<ImportedTenant> = sql_rows(
        conn,
        "SELECT * FROM import_v1_tenants ORDER BY slug",
        &[],
        |row| {
            Ok(ImportedTenant {
                id: parsed(row, "id", TenantId::parse)?,
                slug: row.get("slug")?,
                name: row.get("name")?,
                created_by: parsed(row, "created_by", UserId::parse)?,
                max_targets: row.get("max_targets")?,
                limits: limits(row, "cpus_hundredths", "memory_mb")?,
                created_at: timestamp(row, "created_at")?,
                members: Vec::new(),
                projects: Vec::new(),
                domains: Vec::new(),
            })
        },
    )?;
    for tenant in &mut tenants {
        let id = [tenant.id.to_string()];
        tenant.members = sql_rows(
            conn,
            "SELECT user_id, role FROM import_v1_tenant_members WHERE tenant_id = ? \
             ORDER BY user_id",
            &id,
            |row| {
                Ok(ImportedMember {
                    user_id: parsed(row, "user_id", UserId::parse)?,
                    role: parsed(row, "role", tenant_role)?,
                })
            },
        )?;
        tenant.projects = sql_rows(
            conn,
            "SELECT project_id FROM import_v1_tenant_projects WHERE tenant_id = ? \
             ORDER BY project_id",
            &id,
            |row| parsed(row, "project_id", ProjectId::parse),
        )?;
        tenant.domains = sql_rows(
            conn,
            "SELECT domain_id, cf_record_id FROM import_v1_tenant_domains \
             WHERE tenant_id = ? ORDER BY domain_id",
            &id,
            |row| {
                Ok(ImportedDomain {
                    id: parsed(row, "domain_id", DomainId::parse)?,
                    cf_record_id: row.get("cf_record_id")?,
                })
            },
        )?;
    }
    Ok(tenants)
}

fn sql_rows<T>(
    conn: &Connection,
    sql: &str,
    args: &[String],
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    let mut statement = conn.prepare_cached(sql)?;
    let found = statement.query_map(rusqlite::params_from_iter(args), map)?;
    found.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(raw: &str) -> RepoRef {
        RepoRef::new(raw).unwrap()
    }

    fn names(raw: &[&str]) -> BTreeSet<ProjectName> {
        raw.iter()
            .map(|name| ProjectName::new(name).unwrap())
            .collect()
    }

    mod project_name {
        use super::*;

        // TC: IMP-01 (name = repository name, made a valid project name)
        #[test]
        fn makes_a_label_when_repo_name_has_other_characters() {
            assert_eq!(
                project_name(&repo("acme/My.Shop_2"), "acme", &names(&[]))
                    .unwrap()
                    .as_str(),
                "my-shop-2"
            );
        }

        // TC: IMP-01 (conflict → tenant suffix, within 32 characters)
        #[test]
        fn keeps_the_suffix_whole_when_repo_name_is_long() {
            let long = "a".repeat(40);
            let taken = names(&[&"a".repeat(32)]);
            assert_eq!(
                project_name(&repo(&format!("beta/{long}")), "beta", &taken)
                    .unwrap()
                    .as_str(),
                format!("{}-beta", "a".repeat(27))
            );
        }

        // TC: IMP-01
        #[test]
        fn counts_when_the_suffixed_name_is_taken_too() {
            let taken = names(&["shop", "shop-beta"]);
            assert_eq!(
                project_name(&repo("beta/shop"), "beta", &taken)
                    .unwrap()
                    .as_str(),
                "shop-beta-2"
            );
        }
    }
}
