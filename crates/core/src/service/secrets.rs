//! `SecretsService` (test cases SEC-02..SEC-09).
//!
//! The secrets manager:
//!
//! - `set` writes version N+1 of a key in a scope and keeps the newest
//!   [`SecretsService::MAX_VERSIONS`] versions (SEC-02, SEC-03); the `ci`
//!   flag is stored with it;
//! - only the owner of the scope (single-user mode: the admin) may call any
//!   operation (SEC-10): `resolve` hands out every value, so it is guarded
//!   like `get`, and `list`/`history` reveal a scope's key names, writers
//!   and times, so a foreign scope stays off limits; a refused call appends
//!   `secret.<operation>.denied` and touches nothing else;
//! - `get` shows the current value (SEC-04), except of the CI-only keys
//!   `LIFTOFF_TOKEN`, `REGISTRY_USER` and `REGISTRY_TOKEN`, which are
//!   write-only: whoever may read a project's scope (a tenant's other
//!   members too) never reads them back;
//! - `list` and `history` carry no values (SEC-04);
//! - `rollback` makes an old value the new current version (SEC-05);
//! - `sync` lives in [`SecretSync`](super::SecretSync), which records every attempt in `secret_syncs`;
//! - a scope is a tenant, a project or an environment; the tenant scope
//!   is named through a project ([`ScopeRef::Tenant`]: the tenant that
//!   project is in), and a project outside every tenant (no
//!   `tenants` module) answers [`Error::ModuleRequired`] `tenants` to
//!   whoever may ask about the installation (the admin without modules),
//!   [`Error::Forbidden`] to anyone else;
//! - `resolve` merges tenant → project → environment (the lower scope
//!   wins) and
//!   appends the system variables `LIFTOFF_PROJECT`, `LIFTOFF_ENV`,
//!   `LIFTOFF_DOMAIN` (the first bound host alphabetically, empty without
//!   a binding) and, for a compose environment given a tag, `IMAGE_TAG`
//!   (SEC-06); the deploy token `LIFTOFF_TOKEN` and the registry login
//!   `REGISTRY_USER`/`REGISTRY_TOKEN` are for CI only and never part of
//!   it;
//! - only a project secret may be `ci = true` (SYNC-04);
//! - values are sealed by the [`Secrets`](crate::ports::Secrets) port
//!   before they reach the store (SEC-07);
//! - every operation appends an [`Audit`] entry
//!   without the value (SEC-08): action `secret.<operation>`, resource
//!   `project:<project>[/<KEY>]`, `environment:<project>/<env>[/<KEY>]` or
//!   `tenant-of:<project>[/<KEY>]` (the tenant scope as it was named).
//!   Writes append it before touching the secret, so an error from a write
//!   means nothing was stored (an audit row may then name an attempt that
//!   failed, never a change that went unrecorded).

use std::collections::BTreeMap;
use std::sync::Arc;

use super::support::{authorize, authorized, environment_by_name, project_by_name};
use crate::domain::{
    Audit, AuditId, Env, EnvKind, EnvName, Environment, ImageTag, Project, ProjectName, ScopeKind,
    ScopeRef, Secret, SecretId, SecretKey, SecretScope, SecretValue, Timestamp, UpdatedBy, User,
    VersionNumber,
};
use crate::error::Error;
use crate::ports::{Action, Clock as _, Ports, Resource, Rng as _, Secrets as _, Store as _};
use crate::snippet::{REGISTRY_TOKEN_KEY, REGISTRY_USER_KEY};

#[cfg(test)]
mod tests;

/// A secret as `ls` shows it: everything but the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEntry {
    /// The env variable.
    pub key: SecretKey,
    /// The kind of scope it lives in.
    pub scope: ScopeKind,
    /// The current version.
    pub version: VersionNumber,
    /// Whether it goes to the forge's CI secrets.
    pub ci: bool,
    /// Who wrote the current version.
    pub updated_by: UpdatedBy,
    /// When the current version was written.
    pub updated_at: Timestamp,
}

/// One version as `history` shows it, without the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRevision {
    /// Its number.
    pub version: VersionNumber,
    /// Who wrote it.
    pub updated_by: UpdatedBy,
    /// When.
    pub updated_at: Timestamp,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct SecretsService<P> {
    ports: Arc<P>,
}

impl<P> Clone for SecretsService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
        }
    }
}

impl<P: Ports> SecretsService<P> {
    /// How many versions of a secret are kept.
    pub const MAX_VERSIONS: usize = 20;

    /// A service over `ports`.
    #[must_use]
    pub const fn new(ports: Arc<P>) -> Self {
        Self { ports }
    }

    /// Writes `value` as the next version of `key` in `scope` (version 1
    /// for a new key) with the `ci` flag, and prunes the history to
    /// [`Self::MAX_VERSIONS`]. Only a project secret may be `ci`: the
    /// repository's CI secrets are the project's (`lof secret sync`, SYNC-04).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] on `ci` for `ci` outside the project scope,
    /// before anything is asked;
    /// [`Error::Forbidden`] when `actor` does not own the scope;
    /// [`Error::NotFound`] for an unknown project or environment; whatever
    /// the secrets port or the store report.
    pub async fn set(
        &self,
        actor: &User,
        scope: &ScopeRef,
        key: SecretKey,
        value: SecretValue,
        ci: bool,
    ) -> Result<SecretEntry, Error> {
        check_ci_scope(scope, ci)?;
        let resolved = self
            .authorized_scope(actor, Action::SecretSet, scope, Some(&key))
            .await?;
        self.set_authorized(actor, (scope, resolved), key, value, ci)
            .await
    }

    /// [`SecretsService::set`] for an operation that already asked
    /// [`Authorizer::check`](super::Authorizer::check) about the scope
    /// `scope` names, which resolved to `resolved`: `deploy.rotate_token` stores the deploy
    /// token this way, so it asks the hook once (MOD-04).
    ///
    /// # Errors
    ///
    /// Whatever the secrets port or the store report.
    pub(super) async fn set_authorized(
        &self,
        actor: &User,
        (scope, resolved): (&ScopeRef, SecretScope),
        key: SecretKey,
        value: SecretValue,
        ci: bool,
    ) -> Result<SecretEntry, Error> {
        let store = self.ports.store();
        let sealed = self.ports.secrets().encrypt(value.expose())?;
        let now = self.ports.clock().now();
        let secret = match store.secret(&resolved, &key).await? {
            Some(current) => Secret {
                version: next_version(current.version)?,
                value: sealed,
                ci,
                updated_by: UpdatedBy::User(actor.id),
                updated_at: now,
                ..current
            },
            None => Secret {
                id: SecretId::new(self.ports.rng().ulid()),
                scope: resolved,
                key,
                value: sealed,
                version: VersionNumber::FIRST,
                ci,
                updated_by: UpdatedBy::User(actor.id),
                updated_at: now,
                created_at: now,
            },
        };
        self.audit(actor, "set", scope, Some(&secret.key)).await?;
        self.write(&secret).await?;
        Ok(entry(&secret))
    }

    /// The current value of `key` in `scope`, for the owner of the scope.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] when `actor` does not own the scope (single
    /// user mode: is not an admin) or `key` is for CI only (the deploy token, the registry login: write-only);
    /// [`Error::NotFound`] for an unknown scope or key.
    pub async fn get(
        &self,
        actor: &User,
        scope: &ScopeRef,
        key: &SecretKey,
    ) -> Result<SecretValue, Error> {
        Ok(self.get_versioned(actor, scope, key).await?.0)
    }

    /// [`SecretsService::get`] with the version the value is.
    ///
    /// # Errors
    ///
    /// As [`SecretsService::get`].
    pub async fn get_versioned(
        &self,
        actor: &User,
        scope: &ScopeRef,
        key: &SecretKey,
    ) -> Result<(SecretValue, VersionNumber), Error> {
        let resolved = self
            .authorized_scope(actor, Action::SecretGet, scope, Some(key))
            .await?;
        check_readable(key)?;
        let secret = self.existing(&resolved, scope, key).await?;
        let value = SecretValue::new(self.ports.secrets().decrypt(&secret.value)?);
        self.audit(actor, "get", scope, Some(key)).await?;
        Ok((value, secret.version))
    }

    /// The secrets of `scope`, by key, without values.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] when `actor` does not own the scope;
    /// [`Error::NotFound`] for an unknown scope.
    pub async fn list(&self, actor: &User, scope: &ScopeRef) -> Result<Vec<SecretEntry>, Error> {
        let resolved = self
            .authorized_scope(actor, Action::SecretList, scope, None)
            .await?;
        let entries = self
            .ports
            .store()
            .secrets_of_scope(&resolved)
            .await?
            .iter()
            .map(entry)
            .collect();
        self.audit(actor, "list", scope, None).await?;
        Ok(entries)
    }

    /// Deletes `key` of `scope` with its history.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] when `actor` does not own the scope;
    /// [`Error::NotFound`] for an unknown scope or key.
    pub async fn remove(
        &self,
        actor: &User,
        scope: &ScopeRef,
        key: &SecretKey,
    ) -> Result<(), Error> {
        let resolved = self
            .authorized_scope(actor, Action::SecretRemove, scope, Some(key))
            .await?;
        let secret = self.existing(&resolved, scope, key).await?;
        self.audit(actor, "remove", scope, Some(key)).await?;
        self.ports.store().delete_secret(secret.id).await
    }

    /// The kept versions of `key` in `scope`, newest first, without values.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] when `actor` does not own the scope;
    /// [`Error::NotFound`] for an unknown scope or key.
    pub async fn history(
        &self,
        actor: &User,
        scope: &ScopeRef,
        key: &SecretKey,
    ) -> Result<Vec<SecretRevision>, Error> {
        let resolved = self
            .authorized_scope(actor, Action::SecretHistory, scope, Some(key))
            .await?;
        let secret = self.existing(&resolved, scope, key).await?;
        let revisions = self
            .ports
            .store()
            .secret_versions(secret.id)
            .await?
            .into_iter()
            .map(|version| SecretRevision {
                version: version.version,
                updated_by: version.updated_by,
                updated_at: version.updated_at,
            })
            .collect();
        self.audit(actor, "history", scope, Some(key)).await?;
        Ok(revisions)
    }

    /// Makes the value of version `to` the new current version of `key`.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] when `actor` does not own the scope;
    /// [`Error::NotFound`] for an unknown scope or key, or a version that
    /// is not kept.
    pub async fn rollback(
        &self,
        actor: &User,
        scope: &ScopeRef,
        key: &SecretKey,
        to: VersionNumber,
    ) -> Result<SecretEntry, Error> {
        let resolved = self
            .authorized_scope(actor, Action::SecretRollback, scope, Some(key))
            .await?;
        let current = self.existing(&resolved, scope, key).await?;
        let old = self
            .ports
            .store()
            .secret_versions(current.id)
            .await?
            .into_iter()
            .find(|version| version.version == to)
            .ok_or_else(|| {
                Error::not_found(format!("version {to} of {}", resource(scope, Some(key))))
            })?;
        let secret = Secret {
            version: next_version(current.version)?,
            value: old.value,
            updated_by: UpdatedBy::User(actor.id),
            updated_at: self.ports.clock().now(),
            ..current
        };
        self.audit(actor, "rollback", scope, Some(key)).await?;
        self.write(&secret).await?;
        Ok(entry(&secret))
    }

    /// The env of `env` of `project`: the secrets of its tenant (when it is
    /// in one) overridden by the project's, those by the environment's, by
    /// key, then `LIFTOFF_PROJECT`, `LIFTOFF_ENV`,
    /// `LIFTOFF_DOMAIN` and, for a compose environment when `tag` is
    /// given, `IMAGE_TAG`. A secret named like a system variable is
    /// dropped: the system value wins; the deploy token
    /// ([`DEPLOY_TOKEN_KEY`]) and the registry login are dropped too. Only the owner of the environment
    /// may resolve it: a deploy path without a user needs its own entry
    /// point, not this one.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] when `actor` does not own the environment;
    /// [`Error::NotFound`] for an unknown project or environment; whatever
    /// the secrets port or the store report.
    pub async fn resolve(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
        tag: Option<&ImageTag>,
    ) -> Result<Env, Error> {
        let scope = ScopeRef::Environment(project.clone(), env.clone());
        let found = environment_by_name(self.ports.as_ref(), project, env).await;
        let (project_row, environment) = self
            .audit_refusal(
                actor,
                Action::SecretResolve,
                &scope,
                None,
                authorized(
                    self.ports.as_ref(),
                    actor,
                    Action::SecretResolve,
                    found,
                    |(_, environment)| Resource::Secret(SecretScope::Environment(environment.id)),
                )
                .await,
            )
            .await?;
        let resolved = self.assemble(&project_row, &environment, tag).await?;
        self.audit(actor, "resolve", &scope, None).await?;
        Ok(resolved)
    }

    /// The env of `environment` as [`SecretsService::resolve`] builds it,
    /// for the deploy path, which has no user to check (DEP-03).
    ///
    /// # Errors
    ///
    /// Whatever the secrets port or the store report.
    pub(super) async fn assemble(
        &self,
        project_row: &Project,
        environment: &Environment,
        tag: Option<&ImageTag>,
    ) -> Result<Env, Error> {
        let store = self.ports.store();
        let mut layers = Vec::new();
        let scopes = project_row
            .tenant_id
            .map(SecretScope::Tenant)
            .into_iter()
            .chain([
                SecretScope::Project(project_row.id),
                SecretScope::Environment(environment.id),
            ]);
        for scope in scopes {
            for secret in store.secrets_of_scope(&scope).await? {
                let value = SecretValue::new(self.ports.secrets().decrypt(&secret.value)?);
                layers.push((secret.key, value));
            }
        }
        let mut merged = merge_layers(layers)?;
        let mut hosts = Vec::new();
        for binding in store.bindings_of_environment(environment.id).await? {
            if let Some(domain) = store.domain(binding.domain_id).await? {
                hosts.push(domain.host);
            }
        }
        let domain = hosts
            .into_iter()
            .min()
            .map(|host| host.to_string())
            .unwrap_or_default();
        let mut system = vec![
            (system_key("LIFTOFF_PROJECT")?, project_row.name.to_string()),
            (system_key("LIFTOFF_ENV")?, environment.name.to_string()),
            (system_key("LIFTOFF_DOMAIN")?, domain),
        ];
        match environment.kind {
            EnvKind::Compose => {
                if let Some(tag) = tag {
                    system.push((system_key("IMAGE_TAG")?, tag.to_string()));
                }
            }
            // `IMAGE_TAG` is compose's only.
            EnvKind::Function | EnvKind::Tunnel => {}
        }
        for (key, _) in &system {
            merged.remove(key);
        }
        let vars = merged
            .into_iter()
            .chain(
                system
                    .into_iter()
                    .map(|(key, value)| (key, SecretValue::new(value))),
            )
            .collect();
        Ok(Env::new(vars))
    }

    /// Writes `value` as the next version of `key` in the scope of
    /// `environment` on behalf of CI (`updated_by = ci`): the deprecated
    /// `env` field of `POST /deploys` (DEP-04). The `ci` flag of
    /// an existing key is kept; a new key is not synced to CI.
    ///
    /// # Errors
    ///
    /// Whatever the secrets port or the store report.
    pub(super) async fn set_by_ci(
        &self,
        project_row: &Project,
        environment: &Environment,
        key: SecretKey,
        value: &SecretValue,
    ) -> Result<(), Error> {
        let store = self.ports.store();
        let scope = SecretScope::Environment(environment.id);
        let sealed = self.ports.secrets().encrypt(value.expose())?;
        let now = self.ports.clock().now();
        let secret = match store.secret(&scope, &key).await? {
            Some(current) => Secret {
                version: next_version(current.version)?,
                value: sealed,
                updated_by: UpdatedBy::Ci,
                updated_at: now,
                ..current
            },
            None => Secret {
                id: SecretId::new(self.ports.rng().ulid()),
                scope,
                key,
                value: sealed,
                version: VersionNumber::FIRST,
                ci: false,
                updated_by: UpdatedBy::Ci,
                updated_at: now,
                created_at: now,
            },
        };
        let named = ScopeRef::Environment(project_row.name.clone(), environment.name.clone());
        self.record(None, "set", &named, Some(&secret.key)).await?;
        self.write(&secret).await
    }
    /// The stored scope `scope` names, once `actor` may do `action` on
    /// its secrets (single-user mode: is an admin); a refusal appends
    /// `secret.<operation>.denied` and touches nothing else. The tenant of
    /// a project outside every tenant is asked about as the installation
    /// ([`Resource::Admin`]), like an unknown name, before its
    /// [`Error::ModuleRequired`].
    async fn authorized_scope(
        &self,
        actor: &User,
        action: Action,
        scope: &ScopeRef,
        key: Option<&SecretKey>,
    ) -> Result<SecretScope, Error> {
        let ports = self.ports.as_ref();
        let checked = match self.scope(scope).await {
            Err(Error::ModuleRequired(module)) => authorize(ports, actor, action, &Resource::Admin)
                .await
                .and(Err(Error::ModuleRequired(module))),
            found => {
                authorized(ports, actor, action, found, |resolved| {
                    Resource::Secret(*resolved)
                })
                .await
            }
        };
        self.audit_refusal(actor, action, scope, key, checked).await
    }

    /// `checked` as it is, after appending `secret.<operation>.denied` when
    /// it is [`Error::Forbidden`] (SEC-08).
    pub(super) async fn audit_refusal<T>(
        &self,
        actor: &User,
        action: Action,
        scope: &ScopeRef,
        key: Option<&SecretKey>,
        checked: Result<T, Error>,
    ) -> Result<T, Error> {
        if matches!(checked, Err(Error::Forbidden(_))) {
            let operation = action
                .as_str()
                .strip_prefix("secret.")
                .unwrap_or(action.as_str());
            self.audit(actor, &format!("{operation}.denied"), scope, key)
                .await?;
        }
        checked
    }

    /// The stored scope `scope` names.
    async fn scope(&self, scope: &ScopeRef) -> Result<SecretScope, Error> {
        match scope {
            ScopeRef::Tenant(project) => project_by_name(self.ports.as_ref(), project)
                .await?
                .tenant_id
                .map(SecretScope::Tenant)
                .ok_or_else(|| Error::module_required("tenants")),
            ScopeRef::Project(project) => Ok(SecretScope::Project(
                project_by_name(self.ports.as_ref(), project).await?.id,
            )),
            ScopeRef::Environment(project, env) => Ok(SecretScope::Environment(
                environment_by_name(self.ports.as_ref(), project, env)
                    .await?
                    .1
                    .id,
            )),
        }
    }

    /// The stored secret `key` of `scope`, which resolved to `resolved`.
    async fn existing(
        &self,
        resolved: &SecretScope,
        scope: &ScopeRef,
        key: &SecretKey,
    ) -> Result<Secret, Error> {
        self.ports
            .store()
            .secret(resolved, key)
            .await?
            .ok_or_else(|| Error::not_found(format!("secret {}", resource(scope, Some(key)))))
    }

    /// Stores `secret` with its version and prunes the history (SEC-03).
    async fn write(&self, secret: &Secret) -> Result<(), Error> {
        let store = self.ports.store();
        store.put_secret(secret).await?;
        store
            .prune_secret_versions(secret.id, Self::MAX_VERSIONS)
            .await?;
        Ok(())
    }

    /// Appends the audit entry `secret.<operation>` on `scope[/key]` by
    /// `actor`.
    pub(super) async fn audit(
        &self,
        actor: &User,
        operation: &str,
        scope: &ScopeRef,
        key: Option<&SecretKey>,
    ) -> Result<(), Error> {
        self.record(Some(actor), operation, scope, key).await
    }

    /// Appends the audit entry `secret.<operation>` on `scope[/key]`;
    /// `actor` is `None` for CI.
    async fn record(
        &self,
        actor: Option<&User>,
        operation: &str,
        scope: &ScopeRef,
        key: Option<&SecretKey>,
    ) -> Result<(), Error> {
        let entry = Audit {
            id: AuditId::new(self.ports.rng().ulid()),
            actor_user_id: actor.map(|actor| actor.id),
            action: format!("secret.{operation}"),
            resource: resource(scope, key),
            at: self.ports.clock().now(),
        };
        self.ports.store().insert_audit(&entry).await
    }
}

/// `project:<project>[/<KEY>]`, `environment:<project>/<env>[/<KEY>]` or
/// `tenant-of:<project>[/<KEY>]`.
fn resource(scope: &ScopeRef, key: Option<&SecretKey>) -> String {
    let base = match scope {
        ScopeRef::Tenant(project) => format!("tenant-of:{project}"),
        ScopeRef::Project(project) => format!("project:{project}"),
        ScopeRef::Environment(project, env) => format!("environment:{project}/{env}"),
    };
    match key {
        Some(key) => format!("{base}/{key}"),
        None => base,
    }
}

fn entry(secret: &Secret) -> SecretEntry {
    SecretEntry {
        key: secret.key.clone(),
        scope: secret.scope.kind(),
        version: secret.version,
        ci: secret.ci,
        updated_by: secret.updated_by,
        updated_at: secret.updated_at,
    }
}

fn next_version(version: VersionNumber) -> Result<VersionNumber, Error> {
    version
        .next()
        .ok_or_else(|| Error::internal_msg("secret version overflow"))
}

/// The project secret that holds the deploy token: `ci = true`,
/// sent to the forge's CI, never put into an environment's env.
pub const DEPLOY_TOKEN_KEY: &str = "LIFTOFF_TOKEN";

/// Only a project secret may be `ci = true` (SYNC-04).
///
/// The repository's CI secrets are the project's. [`SecretsService::set`]
/// checks it before anything is asked; local mode (the CLI's store) checks
/// it the same way.
///
/// # Errors
///
/// [`Error::Validation`] on `ci` for `ci` outside the project scope.
pub fn check_ci_scope(scope: &ScopeRef, ci: bool) -> Result<(), Error> {
    if ci && !matches!(scope, ScopeRef::Project(_)) {
        return Err(Error::invalid(
            "ci",
            "only project secrets go to CI: use --scope project with --ci",
        ));
    }
    Ok(())
}

/// The keys for CI only (the deploy token, the registry login) are
/// write-only: `get` never shows them.
///
/// [`SecretsService::get`] checks it; local mode (the CLI's store) checks
/// it the same way.
///
/// # Errors
///
/// [`Error::Forbidden`] for `LIFTOFF_TOKEN`, `REGISTRY_USER` and
/// `REGISTRY_TOKEN`.
pub fn check_readable(key: &SecretKey) -> Result<(), Error> {
    if CI_ONLY_KEYS.contains(&key.as_str()) {
        return Err(Error::forbidden(format!(
            "{key} is for CI only: set it again to change it, it is never shown"
        )));
    }
    Ok(())
}

/// The scopes the env of environment `env` of `project` is merged from,
/// widest first: tenant, project, environment (the lower scope wins). [`merge_layers`] takes their secrets in this order.
#[must_use]
pub fn env_scopes(project: &ProjectName, env: &EnvName) -> [ScopeRef; 3] {
    [
        ScopeRef::Tenant(project.clone()),
        ScopeRef::Project(project.clone()),
        ScopeRef::Environment(project.clone(), env.clone()),
    ]
}

/// The secrets of `layers` merged by key, a later value winning.
///
/// The layers come widest scope first ([`env_scopes`]); the keys for CI
/// only (`LIFTOFF_TOKEN`, `REGISTRY_USER`, `REGISTRY_TOKEN`) are left out:
/// the secrets of an env before [`SecretsService::resolve`] adds the
/// system variables.
///
/// # Errors
///
/// Only if a CI-only key name is no key (a bug).
pub fn merge_layers<I>(layers: I) -> Result<BTreeMap<SecretKey, SecretValue>, Error>
where
    I: IntoIterator<Item = (SecretKey, SecretValue)>,
{
    let mut merged: BTreeMap<SecretKey, SecretValue> = layers.into_iter().collect();
    for name in CI_ONLY_KEYS {
        let _removed = merged.remove(&system_key(name)?);
    }
    Ok(merged)
}

/// The project secrets that exist for the forge's CI only, never go into
/// an environment's env and are never shown by `get`: the deploy token
/// and the registry login a Forgejo compose workflow pushes with (a package-scoped credential `lof create` asks for).
pub(super) const CI_ONLY_KEYS: [&str; 3] =
    [DEPLOY_TOKEN_KEY, REGISTRY_USER_KEY, REGISTRY_TOKEN_KEY];

/// The variables liftoff sets itself; their values are not secret.
pub(super) const SYSTEM_VARS: [&str; 4] = [
    "LIFTOFF_PROJECT",
    "LIFTOFF_ENV",
    "LIFTOFF_DOMAIN",
    "IMAGE_TAG",
];

/// The key of a system variable; every name passed is a valid key.
fn system_key(name: &str) -> Result<SecretKey, Error> {
    SecretKey::new(name)
        .map_err(|error| Error::internal_msg(format!("system variable {name}: {error}")))
}
