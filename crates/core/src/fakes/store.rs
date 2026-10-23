//! [`InMemoryStore`]: the [`Store`] port in memory, held to the same
//! contract as the SQLite store (`fakes::store_contract`).

use core::future::{Ready, ready};
use core::ops::RangeInclusive;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use super::log::{Call, CallLog, Faults, lock, scriptable};
use crate::domain::{
    Artifact, ArtifactId, ArtifactVersion, Audit, AuditId, Binding, BindingId, Branch,
    CachedTemplate, Deploy, DeployId, DeployNumber, DeployStatus, DeployToken, DeployTokenId,
    Domain, DomainId, EnvName, Environment, EnvironmentId, GateCode, GateSession, GitConnection,
    Host, HostPort, InstalledModule, Login, ModuleName, OneTimeLink, Project, ProjectId,
    ProjectName, RepoRef, Secret, SecretId, SecretKey, SecretScope, SecretSyncRecord,
    SecretVersion, Session, SessionId, SetupToken, SyncTarget, TenantId, Timestamp, TokenHash,
    User, UserId, UserSource, VersionNumber, Visibility,
};
use crate::error::Error;
use crate::ports::{ModuleStore, Store};

#[derive(Debug, Default)]
struct Tables {
    users: BTreeMap<UserId, User>,
    sessions: BTreeMap<SessionId, Session>,
    gate_codes: BTreeMap<TokenHash, GateCode>,
    gate_sessions: BTreeMap<TokenHash, GateSession>,
    one_time_links: BTreeMap<TokenHash, OneTimeLink>,
    git_connections: BTreeMap<(UserId, Host), GitConnection>,
    /// The template cache, by `(user, host, repo)`.
    cached_templates: BTreeMap<(UserId, Host, RepoRef), CachedTemplate>,
    projects: BTreeMap<ProjectId, Project>,
    environments: BTreeMap<EnvironmentId, Environment>,
    domains: BTreeMap<DomainId, Domain>,
    bindings: BTreeMap<BindingId, Binding>,
    deploys: BTreeMap<DeployId, Deploy>,
    tokens: BTreeMap<DeployTokenId, DeployToken>,
    secrets: BTreeMap<SecretId, Secret>,
    secret_versions: BTreeMap<(SecretId, VersionNumber), SecretVersion>,
    /// What the last sync of a secret to a target did, by `(secret, target)`.
    secret_syncs: BTreeMap<(SecretId, SyncTarget), SecretSyncRecord>,
    /// The key-value stores of functions, by `(environment, key)`.
    kv: BTreeMap<(EnvironmentId, String), Vec<u8>>,
    audit: BTreeMap<AuditId, Audit>,
    /// The live single-use login token (the `settings` row).
    setup_token: Option<SetupToken>,
    /// Host port of the last inserted binding (the allocation cursor).
    last_host_port: Option<HostPort>,
    /// The key spaces of modules (`store-get`/`store-set`).
    module_keys: BTreeMap<(ModuleName, String), Vec<u8>>,
    /// Installed modules, in insertion order.
    modules: Vec<InstalledModule>,
    /// Artifacts.
    artifacts: BTreeMap<ArtifactId, Artifact>,
    /// Their versions, with their files, by `(artifact, number)`.
    artifact_versions: BTreeMap<(ArtifactId, VersionNumber), ArtifactVersion>,
}

fn conflict(what: &str) -> Error {
    Error::conflict(format!("{what} already exists"))
}

fn ensure(condition: bool, error: impl FnOnce() -> Error) -> Result<(), Error> {
    if condition { Ok(()) } else { Err(error()) }
}

impl Tables {
    fn need_user(&self, id: UserId) -> Result<(), Error> {
        ensure(self.users.contains_key(&id), || Error::not_found("user"))
    }

    fn need_project(&self, id: ProjectId) -> Result<(), Error> {
        ensure(self.projects.contains_key(&id), || {
            Error::not_found("project")
        })
    }

    fn need_environment(&self, id: EnvironmentId) -> Result<(), Error> {
        ensure(self.environments.contains_key(&id), || {
            Error::not_found("environment")
        })
    }

    fn check_user(&self, user: &User) -> Result<(), Error> {
        let clash = self.users.values().any(|other| {
            other.id != user.id
                && (other.login == user.login
                    || user.subject.is_some()
                        && other.subject == user.subject
                        && other.source == user.source)
        });
        ensure(!clash, || conflict(&format!("user {}", user.login)))
    }

    fn check_session(&self, session: &Session) -> Result<(), Error> {
        self.need_user(session.user_id)?;
        let clash = self
            .sessions
            .values()
            .any(|other| other.id != session.id && other.token_hash == session.token_hash);
        ensure(!clash, || conflict("session"))
    }

    /// Deletes what was handed over from the sessions that are gone.
    fn cascade_sessions(&mut self) {
        let sessions = &self.sessions;
        self.gate_codes
            .retain(|_, code| sessions.contains_key(&code.session_id));
        self.gate_sessions
            .retain(|_, gate| sessions.contains_key(&gate.session_id));
    }

    fn check_project(&self, project: &Project) -> Result<(), Error> {
        self.need_user(project.created_by)?;
        let clash = self.projects.values().any(|other| {
            other.id != project.id
                && ((other.git_host == project.git_host && other.repo == project.repo)
                    || (other.tenant_id == project.tenant_id && other.name == project.name))
        });
        ensure(!clash, || conflict(&format!("project {}", project.name)))
    }

    fn check_environment(&self, environment: &Environment) -> Result<(), Error> {
        self.need_project(environment.project_id)?;
        if let Some(pin) = &environment.pin {
            self.need_user(pin.pinned_by)?;
        }
        if let Some(owner) = environment.owner_user_id {
            self.need_user(owner)?;
        }
        let clash = self.environments.values().any(|other| {
            other.id != environment.id
                && other.project_id == environment.project_id
                && other.name == environment.name
        });
        ensure(!clash, || {
            conflict(&format!("environment {}", environment.name))
        })
    }

    fn check_domain(&self, domain: &Domain) -> Result<(), Error> {
        self.need_project(domain.project_id)?;
        let clash = self
            .domains
            .values()
            .any(|other| other.id != domain.id && other.host == domain.host);
        ensure(!clash, || conflict(&format!("domain {}", domain.host)))
    }

    fn check_deploy(&self, deploy: &Deploy) -> Result<(), Error> {
        self.need_environment(deploy.environment_id)?;
        let clash = self.deploys.values().any(|other| {
            other.id != deploy.id
                && other.environment_id == deploy.environment_id
                && other.number == deploy.number
        });
        ensure(!clash, || conflict(&format!("deploy #{}", deploy.number)))
    }

    fn insert_binding(&mut self, binding: &Binding) -> Result<(), Error> {
        self.need_environment(binding.environment_id)?;
        ensure(self.domains.contains_key(&binding.domain_id), || {
            Error::not_found("domain")
        })?;
        let clash = self.bindings.values().any(|other| {
            other.id == binding.id
                || other.domain_id == binding.domain_id
                || other.host_port == binding.host_port
        });
        ensure(!clash, || conflict("binding"))?;
        let _previous: Option<Binding> = self.bindings.insert(binding.id, binding.clone());
        self.last_host_port = Some(binding.host_port);
        Ok(())
    }

    fn free_host_port(&self, range: &RangeInclusive<HostPort>) -> Option<HostPort> {
        let used: BTreeSet<HostPort> = self.bindings.values().map(|b| b.host_port).collect();
        let cursor = self.last_host_port.filter(|last| range.contains(last));
        let (after, up_to): (Vec<HostPort>, Vec<HostPort>) =
            HostPort::each_in(range).partition(|port| cursor.is_none_or(|last| *port > last));
        after
            .into_iter()
            .chain(up_to)
            .find(|port| !used.contains(port))
    }

    fn delete_environment(&mut self, id: EnvironmentId) -> Result<(), Error> {
        let _removed: Environment = self
            .environments
            .remove(&id)
            .ok_or_else(|| Error::not_found("environment"))?;
        self.bindings.retain(|_, b| b.environment_id != id);
        self.deploys.retain(|_, d| d.environment_id != id);
        self.kv
            .retain(|(environment, _key), _value| *environment != id);
        let scope = SecretScope::Environment(id);
        let doomed: BTreeSet<SecretId> = self
            .secrets
            .values()
            .filter(|s| s.scope == scope)
            .map(|s| s.id)
            .collect();
        self.secrets.retain(|sid, _| !doomed.contains(sid));
        self.secret_versions
            .retain(|(sid, _), _| !doomed.contains(sid));
        self.secret_syncs
            .retain(|(sid, _), _| !doomed.contains(sid));
        Ok(())
    }

    fn delete_project(&mut self, id: ProjectId) -> Result<(), Error> {
        self.need_project(id)?;
        let in_use = self.environments.values().any(|e| e.project_id == id)
            || self.domains.values().any(|d| d.project_id == id);
        ensure(!in_use, || {
            Error::conflict("project has environments or domains")
        })?;
        let _removed: Option<Project> = self.projects.remove(&id);
        self.tokens.retain(|_, t| t.project_id != id);
        let scope = SecretScope::Project(id);
        let doomed: BTreeSet<SecretId> = self
            .secrets
            .values()
            .filter(|s| s.scope == scope)
            .map(|s| s.id)
            .collect();
        self.secrets.retain(|sid, _| !doomed.contains(sid));
        self.secret_versions
            .retain(|(sid, _), _| !doomed.contains(sid));
        self.secret_syncs
            .retain(|(sid, _), _| !doomed.contains(sid));
        Ok(())
    }

    fn delete_domain(&mut self, id: DomainId) -> Result<(), Error> {
        ensure(self.domains.contains_key(&id), || {
            Error::not_found("domain")
        })?;
        let bound = self.bindings.values().any(|b| b.domain_id == id);
        ensure(!bound, || {
            Error::conflict("domain is bound to an environment")
        })?;
        let _removed: Option<Domain> = self.domains.remove(&id);
        Ok(())
    }

    fn deploys_of_environment(&self, environment: EnvironmentId) -> Vec<Deploy> {
        let mut deploys: Vec<Deploy> = self
            .deploys
            .values()
            .filter(|d| d.environment_id == environment)
            .cloned()
            .collect();
        deploys.sort_by_key(|d| core::cmp::Reverse(d.number));
        deploys
    }

    fn next_deploy_number(&self, environment: EnvironmentId) -> Result<DeployNumber, Error> {
        self.deploys
            .values()
            .filter(|d| d.environment_id == environment)
            .map(|d| d.number)
            .max()
            .map_or(Ok(DeployNumber::FIRST), |max| {
                max.next()
                    .ok_or_else(|| Error::internal_msg("deploy number overflow"))
            })
    }

    fn prune_deploys(&mut self, environment: EnvironmentId, keep: usize) -> usize {
        let current = self
            .environments
            .get(&environment)
            .and_then(|e| e.current_deploy_id);
        let doomed: Vec<DeployId> = self
            .deploys_of_environment(environment)
            .into_iter()
            .skip(keep)
            .map(|d| d.id)
            .filter(|id| Some(*id) != current)
            .collect();
        for id in &doomed {
            let _removed: Option<Deploy> = self.deploys.remove(id);
        }
        doomed.len()
    }

    fn insert_deploy_token(&mut self, token: &DeployToken) -> Result<(), Error> {
        self.need_project(token.project_id)?;
        self.need_user(token.created_by)?;
        let clash = self.tokens.values().any(|other| {
            other.id == token.id
                || (token.is_live() && other.is_live() && other.project_id == token.project_id)
        });
        ensure(!clash, || conflict("live deploy token of the project"))?;
        let _previous: Option<DeployToken> = self.tokens.insert(token.id, token.clone());
        Ok(())
    }

    fn revoke_deploy_token(&mut self, id: DeployTokenId, at: Timestamp) -> Result<(), Error> {
        let token = self
            .tokens
            .get_mut(&id)
            .ok_or_else(|| Error::not_found("deploy token"))?;
        token.revoked_at = Some(at);
        Ok(())
    }

    fn replace_deploy_token(
        &mut self,
        old: DeployTokenId,
        new: &DeployToken,
        at: Timestamp,
    ) -> Result<(), Error> {
        let before = self
            .tokens
            .get(&old)
            .ok_or_else(|| Error::not_found("deploy token"))?
            .revoked_at;
        self.revoke_deploy_token(old, at)?;
        if let Err(err) = self.insert_deploy_token(new) {
            if let Some(token) = self.tokens.get_mut(&old) {
                token.revoked_at = before;
            }
            return Err(err);
        }
        Ok(())
    }

    fn need_scope(&self, scope: SecretScope) -> Result<(), Error> {
        match scope {
            // The tenants live in the `tenants` module's tables: the core
            // knows a tenant by the projects in it.
            SecretScope::Tenant(id) => ensure(
                self.projects.values().any(|p| p.tenant_id == Some(id)),
                || Error::not_found("tenant"),
            ),
            SecretScope::Project(id) => self.need_project(id),
            SecretScope::Environment(id) => self.need_environment(id),
        }
    }

    fn put_secret(&mut self, secret: &Secret) -> Result<(), Error> {
        self.need_scope(secret.scope)?;
        let clash = self.secrets.values().any(|other| {
            other.id != secret.id && other.scope == secret.scope && other.key == secret.key
        });
        ensure(!clash, || conflict(&format!("secret {}", secret.key)))?;
        let version_key = (secret.id, secret.version);
        ensure(!self.secret_versions.contains_key(&version_key), || {
            conflict(&format!(
                "version {} of secret {}",
                secret.version, secret.key
            ))
        })?;
        let _previous: Option<Secret> = self.secrets.insert(secret.id, secret.clone());
        let _none: Option<SecretVersion> = self.secret_versions.insert(
            version_key,
            SecretVersion {
                secret_id: secret.id,
                version: secret.version,
                value: secret.value.clone(),
                updated_by: secret.updated_by,
                updated_at: secret.updated_at,
            },
        );
        Ok(())
    }

    fn secret_versions(&self, id: SecretId) -> Vec<SecretVersion> {
        // Keys sort by (secret, version): reverse for newest first.
        let mut versions: Vec<SecretVersion> = self
            .secret_versions
            .range((id, VersionNumber::FIRST)..)
            .take_while(|((sid, _), _)| *sid == id)
            .map(|(_, v)| v.clone())
            .collect();
        versions.reverse();
        versions
    }

    fn delete_secret(&mut self, id: SecretId) -> Result<(), Error> {
        let _removed: Secret = self
            .secrets
            .remove(&id)
            .ok_or_else(|| Error::not_found("secret"))?;
        self.secret_versions.retain(|(sid, _), _| *sid != id);
        self.secret_syncs.retain(|(sid, _), _| *sid != id);
        Ok(())
    }

    /// The versions of artifact `id`, oldest first.
    fn artifact_versions_of(&self, id: ArtifactId) -> Vec<ArtifactVersion> {
        self.artifact_versions
            .range((id, VersionNumber::FIRST)..)
            .take_while(|((artifact, _), _)| *artifact == id)
            .map(|(_, version)| version.clone())
            .collect()
    }

    fn prune_secret_versions(&mut self, id: SecretId, keep: usize) -> usize {
        let doomed: Vec<(SecretId, VersionNumber)> = self
            .secret_versions(id)
            .into_iter()
            .skip(keep)
            .map(|v| (v.secret_id, v.version))
            .collect();
        for key in &doomed {
            let _removed: Option<SecretVersion> = self.secret_versions.remove(key);
        }
        doomed.len()
    }
}

/// Replaces the row with the same id, or fails with [`Error::NotFound`].
fn replace<K: Ord, V: Clone>(
    rows: &mut BTreeMap<K, V>,
    key: &K,
    row: &V,
    what: &str,
) -> Result<(), Error> {
    let slot = rows.get_mut(key).ok_or_else(|| Error::not_found(what))?;
    slot.clone_from(row);
    Ok(())
}

/// Inserts a row whose id is new, or fails with [`Error::Conflict`].
fn insert_new<K: Ord, V: Clone>(
    rows: &mut BTreeMap<K, V>,
    key: K,
    row: &V,
    what: &str,
) -> Result<(), Error> {
    ensure(!rows.contains_key(&key), || conflict(what))?;
    let _previous: Option<V> = rows.insert(key, row.clone());
    Ok(())
}

fn sorted_by<T, K: Ord>(rows: impl Iterator<Item = T>, key: impl Fn(&T) -> K) -> Vec<T> {
    let mut rows: Vec<T> = rows.collect();
    rows.sort_by_key(|row| key(row));
    rows
}

/// Every record in memory. Clones share the data.
///
/// Implements the uniqueness, foreign-key, restrict and cascade rules of
/// the SQL schema (see the [`Store`] docs), so one contract suite runs
/// against both. Mutating methods append `Call::Store("<method>")` to the
/// call log; reads are not logged. `fail_next(op, error)` scripts a
/// failure of any method by name (reads included).
#[derive(Debug, Clone, Default)]
pub struct InMemoryStore {
    tables: Arc<Mutex<Tables>>,
    log: CallLog,
    faults: Faults,
}

scriptable!(InMemoryStore);

impl InMemoryStore {
    /// An empty store with its own call log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store writing to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            ..Self::default()
        }
    }

    fn read<T>(
        &self,
        op: &'static str,
        query: impl FnOnce(&Tables) -> T,
    ) -> Ready<Result<T, Error>> {
        ready(self.faults.check(op).map(|()| query(&lock(&self.tables))))
    }

    fn write<T>(
        &self,
        op: &'static str,
        change: impl FnOnce(&mut Tables) -> Result<T, Error>,
    ) -> Ready<Result<T, Error>> {
        self.log.push(Call::Store(op));
        ready(
            self.faults
                .check(op)
                .and_then(|()| change(&mut lock(&self.tables))),
        )
    }
}

impl Store for InMemoryStore {
    // ---- artifacts -------------------------------------

    fn insert_artifact(
        &self,
        artifact: &Artifact,
        first: &ArtifactVersion,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_artifact", |t| {
            t.need_user(artifact.owner_user_id)?;
            t.need_user(first.created_by)?;
            ensure(!t.artifacts.contains_key(&artifact.id), || {
                conflict(&format!("artifact {}", artifact.id))
            })?;
            let _none: Option<Artifact> = t.artifacts.insert(artifact.id, artifact.clone());
            let _none: Option<ArtifactVersion> = t
                .artifact_versions
                .insert((first.artifact_id, first.number), first.clone());
            Ok(())
        })
    }

    fn add_artifact_version(
        &self,
        artifact: &Artifact,
        version: &ArtifactVersion,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("add_artifact_version", |t| {
            ensure(t.artifacts.contains_key(&artifact.id), || {
                Error::not_found("artifact")
            })?;
            t.need_user(version.created_by)?;
            let key = (version.artifact_id, version.number);
            ensure(!t.artifact_versions.contains_key(&key), || {
                conflict(&format!(
                    "version {} of artifact {}",
                    version.number, artifact.id
                ))
            })?;
            let _none: Option<ArtifactVersion> = t.artifact_versions.insert(key, version.clone());
            replace(&mut t.artifacts, &artifact.id, artifact, "artifact")
        })
    }

    fn update_artifact(
        &self,
        artifact: &Artifact,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("update_artifact", |t| {
            replace(&mut t.artifacts, &artifact.id, artifact, "artifact")
        })
    }

    fn artifact(
        &self,
        id: ArtifactId,
    ) -> impl Future<Output = Result<Option<Artifact>, Error>> + Send {
        self.read("artifact", |t| t.artifacts.get(&id).cloned())
    }

    fn artifacts(
        &self,
        owner: Option<UserId>,
    ) -> impl Future<Output = Result<Vec<Artifact>, Error>> + Send {
        self.read("artifacts", |t| {
            let mut listed: Vec<Artifact> = t
                .artifacts
                .values()
                .filter(|artifact| owner.is_none_or(|user| artifact.owner_user_id == user))
                .cloned()
                .collect();
            listed.sort_by_key(|artifact| core::cmp::Reverse((artifact.created_at, artifact.id)));
            listed
        })
    }

    fn artifact_versions(
        &self,
        id: ArtifactId,
    ) -> impl Future<Output = Result<Vec<ArtifactVersion>, Error>> + Send {
        self.read("artifact_versions", |t| t.artifact_versions_of(id))
    }

    fn artifact_version(
        &self,
        id: ArtifactId,
        number: VersionNumber,
    ) -> impl Future<Output = Result<Option<ArtifactVersion>, Error>> + Send {
        self.read("artifact_version", |t| {
            t.artifact_versions.get(&(id, number)).cloned()
        })
    }

    fn delete_artifact_version(
        &self,
        id: ArtifactId,
        number: VersionNumber,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("delete_artifact_version", |t| {
            let artifact = t
                .artifacts
                .get(&id)
                .ok_or_else(|| Error::not_found("artifact"))?;
            ensure(artifact.current_version != number, || {
                Error::conflict(format!(
                    "version {number} is the current version of artifact {id}"
                ))
            })?;
            t.artifact_versions
                .remove(&(id, number))
                .map(drop)
                .ok_or_else(|| Error::not_found("artifact version"))
        })
    }

    fn prune_artifact_versions(
        &self,
        id: ArtifactId,
        keep: usize,
    ) -> impl Future<Output = Result<Vec<VersionNumber>, Error>> + Send {
        self.write("prune_artifact_versions", |t| {
            let current = t
                .artifacts
                .get(&id)
                .ok_or_else(|| Error::not_found("artifact"))?
                .current_version;
            let numbers: Vec<VersionNumber> = t
                .artifact_versions_of(id)
                .iter()
                .map(|version| version.number)
                .collect();
            let doomed: Vec<VersionNumber> = numbers
                .iter()
                .rev()
                .skip(keep)
                .rev()
                .copied()
                .filter(|number| *number != current)
                .collect();
            for number in &doomed {
                let _removed: Option<ArtifactVersion> = t.artifact_versions.remove(&(id, *number));
            }
            Ok(doomed)
        })
    }

    fn delete_artifact(&self, id: ArtifactId) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("delete_artifact", |t| {
            let _gone: Artifact = t
                .artifacts
                .remove(&id)
                .ok_or_else(|| Error::not_found("artifact"))?;
            t.artifact_versions
                .retain(|(artifact, _), _| *artifact != id);
            Ok(())
        })
    }

    fn expired_artifacts(
        &self,
        now: Timestamp,
    ) -> impl Future<Output = Result<Vec<Artifact>, Error>> + Send {
        self.read("expired_artifacts", |t| {
            let mut expired: Vec<Artifact> = t
                .artifacts
                .values()
                .filter(|artifact| {
                    artifact.visibility == Visibility::Private
                        && artifact.expires_at.is_some_and(|at| at <= now)
                })
                .cloned()
                .collect();
            expired.sort_by_key(|artifact| (artifact.expires_at, artifact.id));
            expired
        })
    }

    // ---- users -------------------------------------------------------------

    fn insert_user(&self, user: &User) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_user", |t| {
            t.check_user(user)?;
            insert_new(&mut t.users, user.id, user, "user")
        })
    }

    fn update_user(&self, user: &User) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("update_user", |t| {
            t.check_user(user)?;
            replace(&mut t.users, &user.id, user, "user")
        })
    }

    fn user(&self, id: UserId) -> impl Future<Output = Result<Option<User>, Error>> + Send {
        self.read("user", |t| t.users.get(&id).cloned())
    }

    fn user_by_login(
        &self,
        login: &Login,
    ) -> impl Future<Output = Result<Option<User>, Error>> + Send {
        self.read("user_by_login", |t| {
            t.users.values().find(|u| &u.login == login).cloned()
        })
    }

    fn user_by_subject(
        &self,
        source: &UserSource,
        subject: &str,
    ) -> impl Future<Output = Result<Option<User>, Error>> + Send {
        self.read("user_by_subject", |t| {
            t.users
                .values()
                .find(|u| &u.source == source && u.subject.as_deref() == Some(subject))
                .cloned()
        })
    }

    fn users(&self) -> impl Future<Output = Result<Vec<User>, Error>> + Send {
        self.read("users", |t| {
            sorted_by(t.users.values().cloned(), |u| u.login.clone())
        })
    }

    // ---- sessions ------------------------------------------------------------

    fn insert_session(&self, session: &Session) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_session", |t| {
            t.check_session(session)?;
            insert_new(&mut t.sessions, session.id, session, "session")
        })
    }

    fn update_session(&self, session: &Session) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("update_session", |t| {
            t.check_session(session)?;
            replace(&mut t.sessions, &session.id, session, "session")
        })
    }

    fn session_by_hash(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<Session>, Error>> + Send {
        self.read("session_by_hash", |t| {
            t.sessions.values().find(|s| &s.token_hash == hash).cloned()
        })
    }

    fn sessions_of_user(
        &self,
        user: UserId,
    ) -> impl Future<Output = Result<Vec<Session>, Error>> + Send {
        self.read("sessions_of_user", |t| {
            t.sessions
                .values()
                .filter(|s| s.user_id == user)
                .cloned()
                .collect()
        })
    }

    fn delete_session(&self, id: SessionId) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("delete_session", |t| {
            t.sessions
                .remove(&id)
                .ok_or_else(|| Error::not_found("session"))?;
            t.cascade_sessions();
            Ok(())
        })
    }

    fn delete_sessions_of_user(
        &self,
        user: UserId,
    ) -> impl Future<Output = Result<usize, Error>> + Send {
        self.write("delete_sessions_of_user", |t| {
            let before = t.sessions.len();
            t.sessions.retain(|_, s| s.user_id != user);
            t.cascade_sessions();
            t.one_time_links.retain(|_, link| link.user_id != user);
            Ok(before.saturating_sub(t.sessions.len()))
        })
    }

    // ---- gate ----------------------------------------------------------------

    fn insert_gate_code(&self, code: &GateCode) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_gate_code", |t| {
            ensure(t.sessions.contains_key(&code.session_id), || {
                Error::not_found("session")
            })?;
            insert_new(&mut t.gate_codes, code.code_hash, code, "gate code")
        })
    }

    fn take_gate_code(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<GateCode>, Error>> + Send {
        self.write("take_gate_code", |t| Ok(t.gate_codes.remove(hash)))
    }

    fn insert_gate_session(
        &self,
        session: &GateSession,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_gate_session", |t| {
            ensure(t.sessions.contains_key(&session.session_id), || {
                Error::not_found("session")
            })?;
            insert_new(
                &mut t.gate_sessions,
                session.token_hash,
                session,
                "gate session",
            )
        })
    }

    fn gate_session(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<GateSession>, Error>> + Send {
        self.read("gate_session", |t| t.gate_sessions.get(hash).cloned())
    }

    fn prune_gate_records(
        &self,
        now: Timestamp,
    ) -> impl Future<Output = Result<(usize, usize), Error>> + Send {
        self.write("prune_gate_records", |t| {
            let (codes, sessions) = (t.gate_codes.len(), t.gate_sessions.len());
            t.gate_codes.retain(|_, code| code.expires_at > now);
            t.gate_sessions.retain(|_, gate| gate.expires_at > now);
            Ok((
                codes.saturating_sub(t.gate_codes.len()),
                sessions.saturating_sub(t.gate_sessions.len()),
            ))
        })
    }

    // ---- one-time links -------------------------------

    fn insert_one_time_link(
        &self,
        link: &OneTimeLink,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_one_time_link", |t| {
            t.need_user(link.user_id)?;
            insert_new(
                &mut t.one_time_links,
                link.token_hash,
                link,
                "one-time link",
            )
        })
    }

    fn one_time_link(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<OneTimeLink>, Error>> + Send {
        self.read("one_time_link", |t| t.one_time_links.get(hash).cloned())
    }

    fn use_one_time_link(
        &self,
        hash: &TokenHash,
        at: Timestamp,
    ) -> impl Future<Output = Result<Option<OneTimeLink>, Error>> + Send {
        self.write("use_one_time_link", |t| {
            Ok(t.one_time_links
                .get_mut(hash)
                .filter(|link| link.used_at.is_none())
                .map(|link| {
                    link.used_at = Some(at);
                    link.clone()
                }))
        })
    }

    fn prune_one_time_links(
        &self,
        now: Timestamp,
    ) -> impl Future<Output = Result<usize, Error>> + Send {
        self.write("prune_one_time_links", |t| {
            let before = t.one_time_links.len();
            t.one_time_links.retain(|_, link| link.expires_at > now);
            Ok(before.saturating_sub(t.one_time_links.len()))
        })
    }

    // ---- setup token (settings) ---------------------------------------------

    fn put_setup_token(
        &self,
        token: &SetupToken,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("put_setup_token", |t| {
            t.need_user(token.user_id)?;
            t.setup_token = Some(token.clone());
            Ok(())
        })
    }

    fn setup_token(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<SetupToken>, Error>> + Send {
        self.read("setup_token", |t| {
            t.setup_token
                .clone()
                .filter(|token| &token.token_hash == hash)
        })
    }

    fn exchange_setup_token(
        &self,
        hash: &TokenHash,
        session: &Session,
    ) -> impl Future<Output = Result<Option<SetupToken>, Error>> + Send {
        self.write("exchange_setup_token", |t| {
            let live = t
                .setup_token
                .as_ref()
                .is_some_and(|token| &token.token_hash == hash && token.user_id == session.user_id);
            if !live {
                return Ok(None);
            }
            t.check_session(session)?;
            insert_new(&mut t.sessions, session.id, session, "session")?;
            Ok(t.setup_token.take())
        })
    }

    // ---- git connections -----------------------------------------------------

    fn put_git_connection(
        &self,
        connection: &GitConnection,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("put_git_connection", |t| {
            t.need_user(connection.user_id)?;
            let key = (connection.user_id, connection.host.clone());
            let clash = t
                .git_connections
                .iter()
                .any(|(other_key, other)| *other_key != key && other.id == connection.id);
            ensure(!clash, || conflict("git connection"))?;
            let _previous: Option<GitConnection> =
                t.git_connections.insert(key, connection.clone());
            Ok(())
        })
    }

    fn git_connection(
        &self,
        user: UserId,
        host: &Host,
    ) -> impl Future<Output = Result<Option<GitConnection>, Error>> + Send {
        self.read("git_connection", |t| {
            t.git_connections.get(&(user, host.clone())).cloned()
        })
    }

    fn git_connections_of_user(
        &self,
        user: UserId,
    ) -> impl Future<Output = Result<Vec<GitConnection>, Error>> + Send {
        self.read("git_connections_of_user", |t| {
            // Keys sort by (user, host).
            t.git_connections
                .values()
                .filter(|c| c.user_id == user)
                .cloned()
                .collect()
        })
    }

    fn delete_git_connection(
        &self,
        user: UserId,
        host: &Host,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("delete_git_connection", |t| {
            t.git_connections
                .remove(&(user, host.clone()))
                .map(drop)
                .ok_or_else(|| Error::not_found("git connection"))
        })
    }

    // ---- projects ------------------------------------------------------------

    fn insert_project(&self, project: &Project) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_project", |t| {
            t.check_project(project)?;
            insert_new(&mut t.projects, project.id, project, "project")
        })
    }

    fn update_project(&self, project: &Project) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("update_project", |t| {
            t.check_project(project)?;
            replace(&mut t.projects, &project.id, project, "project")
        })
    }

    fn project(
        &self,
        id: ProjectId,
    ) -> impl Future<Output = Result<Option<Project>, Error>> + Send {
        self.read("project", |t| t.projects.get(&id).cloned())
    }

    fn project_by_name(
        &self,
        tenant: Option<TenantId>,
        name: &ProjectName,
    ) -> impl Future<Output = Result<Option<Project>, Error>> + Send {
        self.read("project_by_name", |t| {
            t.projects
                .values()
                .find(|p| p.tenant_id == tenant && &p.name == name)
                .cloned()
        })
    }

    fn project_by_repo(
        &self,
        git_host: &Host,
        repo: &RepoRef,
    ) -> impl Future<Output = Result<Option<Project>, Error>> + Send {
        self.read("project_by_repo", |t| {
            t.projects
                .values()
                .find(|p| &p.git_host == git_host && &p.repo == repo)
                .cloned()
        })
    }

    fn projects(&self) -> impl Future<Output = Result<Vec<Project>, Error>> + Send {
        self.read("projects", |t| {
            sorted_by(t.projects.values().cloned(), |p| (p.name.clone(), p.id))
        })
    }

    fn delete_project(&self, id: ProjectId) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("delete_project", |t| t.delete_project(id))
    }

    // ---- environments --------------------------------------------------------

    fn insert_environment(
        &self,
        environment: &Environment,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_environment", |t| {
            t.check_environment(environment)?;
            insert_new(
                &mut t.environments,
                environment.id,
                environment,
                "environment",
            )
        })
    }

    fn update_environment(
        &self,
        environment: &Environment,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("update_environment", |t| {
            t.check_environment(environment)?;
            let protected = t
                .environments
                .get(&environment.id)
                .ok_or_else(|| Error::not_found("environment"))?
                .protected;
            let mut kept = environment.clone();
            kept.protected = protected;
            replace(&mut t.environments, &environment.id, &kept, "environment")
        })
    }

    fn set_environment_protected(
        &self,
        id: EnvironmentId,
        protected: bool,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("set_environment_protected", |t| {
            t.environments
                .get_mut(&id)
                .ok_or_else(|| Error::not_found("environment"))?
                .protected = protected;
            Ok(())
        })
    }

    fn delete_environment(
        &self,
        id: EnvironmentId,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("delete_environment", |t| t.delete_environment(id))
    }

    fn environment(
        &self,
        id: EnvironmentId,
    ) -> impl Future<Output = Result<Option<Environment>, Error>> + Send {
        self.read("environment", |t| t.environments.get(&id).cloned())
    }

    fn environment_by_name(
        &self,
        project: ProjectId,
        name: &EnvName,
    ) -> impl Future<Output = Result<Option<Environment>, Error>> + Send {
        self.read("environment_by_name", |t| {
            t.environments
                .values()
                .find(|e| e.project_id == project && &e.name == name)
                .cloned()
        })
    }

    fn environments_of_project(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<Vec<Environment>, Error>> + Send {
        self.read("environments_of_project", |t| {
            sorted_by(
                t.environments
                    .values()
                    .filter(|e| e.project_id == project)
                    .cloned(),
                |e| e.name.clone(),
            )
        })
    }

    fn environments(&self) -> impl Future<Output = Result<Vec<Environment>, Error>> + Send {
        self.read("environments", |t| {
            t.environments.values().cloned().collect()
        })
    }

    fn environments_on_branch(
        &self,
        project: ProjectId,
        branch: &Branch,
    ) -> impl Future<Output = Result<Vec<Environment>, Error>> + Send {
        self.read("environments_on_branch", |t| {
            sorted_by(
                t.environments
                    .values()
                    .filter(|e| {
                        e.project_id == project
                            && e.pin.as_ref().is_some_and(|pin| &pin.branch == branch)
                    })
                    .cloned(),
                |e| e.name.clone(),
            )
        })
    }

    // ---- domains and bindings ------------------------------------------------

    fn insert_domain(&self, domain: &Domain) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_domain", |t| {
            t.check_domain(domain)?;
            insert_new(&mut t.domains, domain.id, domain, "domain")
        })
    }

    fn update_domain(&self, domain: &Domain) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("update_domain", |t| {
            t.check_domain(domain)?;
            replace(&mut t.domains, &domain.id, domain, "domain")
        })
    }

    fn delete_domain(&self, id: DomainId) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("delete_domain", |t| t.delete_domain(id))
    }

    fn domain(&self, id: DomainId) -> impl Future<Output = Result<Option<Domain>, Error>> + Send {
        self.read("domain", |t| t.domains.get(&id).cloned())
    }

    fn domain_by_host(
        &self,
        host: &Host,
    ) -> impl Future<Output = Result<Option<Domain>, Error>> + Send {
        self.read("domain_by_host", |t| {
            t.domains.values().find(|d| &d.host == host).cloned()
        })
    }

    fn domains_of_project(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<Vec<Domain>, Error>> + Send {
        self.read("domains_of_project", |t| {
            sorted_by(
                t.domains
                    .values()
                    .filter(|d| d.project_id == project)
                    .cloned(),
                |d| d.host.clone(),
            )
        })
    }

    fn domains(&self) -> impl Future<Output = Result<Vec<Domain>, Error>> + Send {
        self.read("domains", |t| {
            sorted_by(t.domains.values().cloned(), |d| d.host.clone())
        })
    }

    fn insert_binding(&self, binding: &Binding) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_binding", |t| t.insert_binding(binding))
    }

    fn delete_binding(&self, id: BindingId) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("delete_binding", |t| {
            t.bindings
                .remove(&id)
                .map(drop)
                .ok_or_else(|| Error::not_found("binding"))
        })
    }

    fn binding_by_domain(
        &self,
        domain: DomainId,
    ) -> impl Future<Output = Result<Option<Binding>, Error>> + Send {
        self.read("binding_by_domain", |t| {
            t.bindings.values().find(|b| b.domain_id == domain).cloned()
        })
    }

    fn bindings_of_environment(
        &self,
        environment: EnvironmentId,
    ) -> impl Future<Output = Result<Vec<Binding>, Error>> + Send {
        self.read("bindings_of_environment", |t| {
            sorted_by(
                t.bindings
                    .values()
                    .filter(|b| b.environment_id == environment)
                    .cloned(),
                |b| b.host_port,
            )
        })
    }

    fn bindings(&self) -> impl Future<Output = Result<Vec<Binding>, Error>> + Send {
        self.read("bindings", |t| {
            sorted_by(t.bindings.values().cloned(), |b| b.host_port)
        })
    }

    fn free_host_port(
        &self,
        range: &RangeInclusive<HostPort>,
    ) -> impl Future<Output = Result<Option<HostPort>, Error>> + Send {
        self.read("free_host_port", |t| t.free_host_port(range))
    }

    // ---- deploys -------------------------------------------------------------

    fn insert_deploy(&self, deploy: &Deploy) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_deploy", |t| {
            t.check_deploy(deploy)?;
            insert_new(&mut t.deploys, deploy.id, deploy, "deploy")
        })
    }

    fn update_deploy(&self, deploy: &Deploy) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("update_deploy", |t| {
            t.check_deploy(deploy)?;
            replace(&mut t.deploys, &deploy.id, deploy, "deploy")
        })
    }

    fn deploy(&self, id: DeployId) -> impl Future<Output = Result<Option<Deploy>, Error>> + Send {
        self.read("deploy", |t| t.deploys.get(&id).cloned())
    }

    fn deploy_by_number(
        &self,
        environment: EnvironmentId,
        number: DeployNumber,
    ) -> impl Future<Output = Result<Option<Deploy>, Error>> + Send {
        self.read("deploy_by_number", |t| {
            t.deploys
                .values()
                .find(|d| d.environment_id == environment && d.number == number)
                .cloned()
        })
    }

    fn deploys_of_environment(
        &self,
        environment: EnvironmentId,
    ) -> impl Future<Output = Result<Vec<Deploy>, Error>> + Send {
        self.read("deploys_of_environment", |t| {
            t.deploys_of_environment(environment)
        })
    }

    fn next_deploy_number(
        &self,
        environment: EnvironmentId,
    ) -> impl Future<Output = Result<DeployNumber, Error>> + Send {
        ready(
            self.faults
                .check("next_deploy_number")
                .and_then(|()| lock(&self.tables).next_deploy_number(environment)),
        )
    }

    fn last_succeeded_deploy(
        &self,
        environment: EnvironmentId,
    ) -> impl Future<Output = Result<Option<Deploy>, Error>> + Send {
        self.read("last_succeeded_deploy", |t| {
            t.deploys_of_environment(environment)
                .into_iter()
                .find(|d| d.status == DeployStatus::Succeeded)
        })
    }

    fn deploys_with_status(
        &self,
        statuses: &[DeployStatus],
    ) -> impl Future<Output = Result<Vec<Deploy>, Error>> + Send {
        self.read("deploys_with_status", |t| {
            t.deploys
                .values()
                .filter(|d| statuses.contains(&d.status))
                .cloned()
                .collect()
        })
    }

    fn prune_deploys(
        &self,
        environment: EnvironmentId,
        keep: usize,
    ) -> impl Future<Output = Result<usize, Error>> + Send {
        self.write("prune_deploys", |t| Ok(t.prune_deploys(environment, keep)))
    }

    // ---- deploy tokens -------------------------------------------------------

    fn insert_deploy_token(
        &self,
        token: &DeployToken,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_deploy_token", |t| t.insert_deploy_token(token))
    }

    fn deploy_token(
        &self,
        id: DeployTokenId,
    ) -> impl Future<Output = Result<Option<DeployToken>, Error>> + Send {
        self.read("deploy_token", |t| t.tokens.get(&id).cloned())
    }

    fn live_deploy_token(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<Option<DeployToken>, Error>> + Send {
        self.read("live_deploy_token", |t| {
            t.tokens
                .values()
                .find(|x| x.project_id == project && x.is_live())
                .cloned()
        })
    }

    fn live_deploy_token_by_hash(
        &self,
        hash: &TokenHash,
    ) -> impl Future<Output = Result<Option<DeployToken>, Error>> + Send {
        self.read("live_deploy_token_by_hash", |t| {
            t.tokens
                .values()
                .find(|x| &x.token_hash == hash && x.is_live())
                .cloned()
        })
    }

    fn revoke_deploy_token(
        &self,
        id: DeployTokenId,
        at: Timestamp,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("revoke_deploy_token", |t| t.revoke_deploy_token(id, at))
    }

    fn replace_deploy_token(
        &self,
        old: DeployTokenId,
        new: &DeployToken,
        at: Timestamp,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("replace_deploy_token", |t| {
            t.replace_deploy_token(old, new, at)
        })
    }

    // ---- secrets -------------------------------------------------------------

    fn put_secret(&self, secret: &Secret) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("put_secret", |t| t.put_secret(secret))
    }

    fn secret(
        &self,
        scope: &SecretScope,
        key: &SecretKey,
    ) -> impl Future<Output = Result<Option<Secret>, Error>> + Send {
        self.read("secret", |t| {
            t.secrets
                .values()
                .find(|s| &s.scope == scope && &s.key == key)
                .cloned()
        })
    }

    fn secrets_of_scope(
        &self,
        scope: &SecretScope,
    ) -> impl Future<Output = Result<Vec<Secret>, Error>> + Send {
        self.read("secrets_of_scope", |t| {
            sorted_by(
                t.secrets.values().filter(|s| &s.scope == scope).cloned(),
                |s| s.key.clone(),
            )
        })
    }

    fn delete_secret(&self, id: SecretId) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("delete_secret", |t| t.delete_secret(id))
    }

    fn secret_versions(
        &self,
        id: SecretId,
    ) -> impl Future<Output = Result<Vec<SecretVersion>, Error>> + Send {
        self.read("secret_versions", |t| t.secret_versions(id))
    }

    fn prune_secret_versions(
        &self,
        id: SecretId,
        keep: usize,
    ) -> impl Future<Output = Result<usize, Error>> + Send {
        self.write("prune_secret_versions", |t| {
            Ok(t.prune_secret_versions(id, keep))
        })
    }

    // ---- secret syncs ----------------------------------------------------------

    fn put_secret_sync(
        &self,
        sync: &SecretSyncRecord,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        let sync = sync.clone();
        self.write("put_secret_sync", move |t| {
            ensure(t.secrets.contains_key(&sync.secret_id), || {
                Error::not_found("secret")
            })?;
            let _previous: Option<SecretSyncRecord> =
                t.secret_syncs.insert((sync.secret_id, sync.target), sync);
            Ok(())
        })
    }

    fn secret_sync(
        &self,
        secret: SecretId,
        target: SyncTarget,
    ) -> impl Future<Output = Result<Option<SecretSyncRecord>, Error>> + Send {
        self.read("secret_sync", move |t| {
            t.secret_syncs.get(&(secret, target)).cloned()
        })
    }

    // ---- key-value of functions ------------------------

    fn kv_get(
        &self,
        environment: EnvironmentId,
        key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send {
        self.read("kv_get", |t| {
            t.kv.get(&(environment, key.to_owned())).cloned()
        })
    }

    /// The in-memory store keeps no `updated_at`: nothing reads it back.
    fn kv_set(
        &self,
        environment: EnvironmentId,
        key: &str,
        value: &[u8],
        _at: Timestamp,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("kv_set", |t| {
            t.need_environment(environment)?;
            let _previous: Option<Vec<u8>> =
                t.kv.insert((environment, key.to_owned()), value.to_vec());
            Ok(())
        })
    }

    fn kv_delete(
        &self,
        environment: EnvironmentId,
        key: &str,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("kv_delete", |t| {
            let _removed: Option<Vec<u8>> = t.kv.remove(&(environment, key.to_owned()));
            Ok(())
        })
    }

    fn kv_usage(
        &self,
        environment: EnvironmentId,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        self.read("kv_usage", |t| {
            t.kv.iter()
                .filter(|((owner, _key), _value)| *owner == environment)
                .map(|((_owner, key), value)| {
                    u64::try_from(key.len().saturating_add(value.len())).unwrap_or(u64::MAX)
                })
                .fold(0_u64, u64::saturating_add)
        })
    }

    // ---- audit ---------------------------------------------------------------

    fn insert_audit(&self, entry: &Audit) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_audit", |t| {
            insert_new(&mut t.audit, entry.id, entry, "audit entry")
        })
    }

    fn audit_log(&self) -> impl Future<Output = Result<Vec<Audit>, Error>> + Send {
        self.read("audit_log", |t| t.audit.values().cloned().collect())
    }

    // ---- modules -----------------------------------

    fn insert_module(
        &self,
        module: &InstalledModule,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("insert_module", |t| {
            if t.modules.iter().any(|known| known.name == module.name) {
                return Err(conflict(&format!("module {}", module.name)));
            }
            t.modules.push(module.clone());
            Ok(())
        })
    }

    fn update_module(
        &self,
        module: &InstalledModule,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("update_module", |t| {
            let known = t
                .modules
                .iter_mut()
                .find(|known| known.name == module.name)
                .ok_or_else(|| Error::not_found(format!("module {}", module.name)))?;
            *known = InstalledModule {
                installed_at: known.installed_at,
                ..module.clone()
            };
            Ok(())
        })
    }

    fn module(
        &self,
        name: &ModuleName,
    ) -> impl Future<Output = Result<Option<InstalledModule>, Error>> + Send {
        self.read("module", |t| {
            t.modules.iter().find(|known| &known.name == name).cloned()
        })
    }

    fn modules(&self) -> impl Future<Output = Result<Vec<InstalledModule>, Error>> + Send {
        self.read("modules", |t| t.modules.clone())
    }

    fn delete_module(&self, name: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("delete_module", |t| {
            let before = t.modules.len();
            t.modules.retain(|known| &known.name != name);
            if t.modules.len() == before {
                return Err(Error::not_found(format!("module {name}")));
            }
            Ok(())
        })
    }

    // ---- template cache ------------------------------------------------------

    fn put_cached_template(
        &self,
        entry: &CachedTemplate,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("put_cached_template", |t| {
            t.need_user(entry.user_id)?;
            let key = (entry.user_id, entry.host.clone(), entry.repo.clone());
            let _previous: Option<CachedTemplate> = t.cached_templates.insert(key, entry.clone());
            Ok(())
        })
    }

    fn cached_template(
        &self,
        user: UserId,
        host: &Host,
        repo: &RepoRef,
    ) -> impl Future<Output = Result<Option<CachedTemplate>, Error>> + Send {
        self.read("cached_template", |t| {
            t.cached_templates
                .get(&(user, host.clone(), repo.clone()))
                .cloned()
        })
    }
}

/// The key space of `store-get`/`store-set` per module; `store-query`
/// needs SQL and is refused (only the SQLite store runs it).
impl ModuleStore for InMemoryStore {
    fn module_get(
        &self,
        module: &ModuleName,
        key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send {
        self.read("module_get", |t| {
            t.module_keys
                .get(&(module.clone(), key.to_owned()))
                .cloned()
        })
    }

    fn module_set(
        &self,
        module: &ModuleName,
        key: &str,
        value: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("module_set", |t| {
            t.module_keys
                .insert((module.clone(), key.to_owned()), value.to_vec());
            Ok(())
        })
    }

    fn module_query(
        &self,
        module: &ModuleName,
        _sql: &str,
        _params: &[String],
    ) -> impl Future<Output = Result<String, Error>> + Send {
        let refused = self.faults.check("module_query").and_then(|()| {
            Err(Error::forbidden(format!(
                "store-query of {module} needs the SQLite store"
            )))
        });
        ready(refused)
    }

    /// Drops the module's keys; the in-memory store keeps no module
    /// tables (`module_query` refuses every statement).
    fn module_purge(&self, module: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send {
        self.write("module_purge", |t| {
            t.module_keys
                .retain(|(owner, _key), _value| owner != module);
            Ok(())
        })
    }
}
