//! [`SecretSync`] (test cases SYNC-01…SYNC-04).
//!
//! `sync(actor, project)` sends every `ci = true` secret of the project's
//! own scope to the CI secrets of its repository
//! ([`Git::put_ci_secret`](crate::ports::Git::put_ci_secret)) as the
//! actor's own connection on the project's forge:
//!
//! - only `ci = true` goes (SYNC-01); environment secrets never go, a
//!   repository's CI secrets are not per environment;
//! - each attempt is recorded in `secret_syncs`
//!   ([`SecretSyncRecord`]): a forge
//!   error lands in its `error` and the other secrets are still sent
//!   (SYNC-02);
//! - a secret whose current version the forge already holds is not sent
//!   again (SYNC-03); a new version (`lof secret set --ci`) is (SYNC-04);
//! - `authorize` is asked once, [`Action::SecretSync`] on the project's
//!   secret scope; a refusal sends nothing.
//!
//! [`Action::SecretSync`]: crate::ports::Action::SecretSync

use std::sync::Arc;

use super::secrets::SecretsService;
use super::support::{actor_access, authorized, project_by_name};
use crate::domain::{
    ProjectName, ScopeRef, SecretKey, SecretScope, SecretSyncRecord, SecretValue, SyncTarget, User,
};
use crate::error::Error;
use crate::ports::{Action, Clock as _, Git as _, Ports, Resource, Secrets as _, Store as _};

#[cfg(test)]
mod tests;

/// What one [`SecretSync::sync`] did, each list by key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncReport {
    /// Sent to the forge now.
    pub synced: Vec<SecretKey>,
    /// Not sent: the forge already holds the current version.
    pub unchanged: Vec<SecretKey>,
    /// The forge refused or failed; recorded in `secret_syncs.error`.
    pub failed: Vec<SyncFailure>,
}

/// One secret the forge did not take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncFailure {
    /// The secret.
    pub key: SecretKey,
    /// Why, as recorded in `secret_syncs.error`.
    pub error: String,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct SecretSync<P> {
    ports: Arc<P>,
}

impl<P> Clone for SecretSync<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
        }
    }
}

impl<P: Ports> SecretSync<P> {
    /// A service over `ports`.
    #[must_use]
    pub const fn new(ports: Arc<P>) -> Self {
        Self { ports }
    }

    /// Sends the `ci = true` secrets of `project` that changed since their
    /// last successful sync to the CI secrets of its repository.
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] when `authorize` refuses or `actor` has no
    ///   connection on the project's forge host;
    /// - [`Error::NotFound`] for an unknown project;
    /// - whatever the secrets port or the store report. A forge failure
    ///   for one secret is not an `Err`: it is in
    ///   [`SyncReport::failed`].
    pub async fn sync(&self, actor: &User, project: &ProjectName) -> Result<SyncReport, Error> {
        let ports = self.ports.as_ref();
        let secrets = SecretsService::new(Arc::clone(&self.ports));
        let scope = ScopeRef::Project(project.clone());
        let found = project_by_name(ports, project).await;
        let checked = authorized(ports, actor, Action::SecretSync, found, |project| {
            Resource::Secret(SecretScope::Project(project.id))
        })
        .await;
        let project = secrets
            .audit_refusal(actor, Action::SecretSync, &scope, None, checked)
            .await?;
        let access = actor_access(ports, actor, &project).await?;
        secrets.audit(actor, "sync", &scope, None).await?;
        let store = ports.store();
        let mut report = SyncReport::default();
        for secret in store
            .secrets_of_scope(&SecretScope::Project(project.id))
            .await?
        {
            if !secret.ci {
                continue;
            }
            let previous = store.secret_sync(secret.id, SyncTarget::Ci).await?;
            if previous
                .as_ref()
                .is_some_and(|row| row.version == Some(secret.version) && row.error.is_none())
            {
                report.unchanged.push(secret.key);
                continue;
            }
            let value = SecretValue::new(ports.secrets().decrypt(&secret.value)?);
            let sent = ports
                .git()
                .put_ci_secret(&access, &project.repo, &secret.key, &value)
                .await;
            let record = match sent {
                Ok(()) => {
                    report.synced.push(secret.key.clone());
                    SecretSyncRecord {
                        secret_id: secret.id,
                        target: SyncTarget::Ci,
                        version: Some(secret.version),
                        synced_at: Some(ports.clock().now()),
                        error: None,
                    }
                }
                Err(error) => {
                    if let Error::Internal(source) = &error {
                        tracing::error!(
                            project = %project.name,
                            key = %secret.key,
                            error = %source,
                            "syncing a CI secret failed"
                        );
                    }
                    let reason = error.to_string();
                    report.failed.push(SyncFailure {
                        key: secret.key.clone(),
                        error: reason.clone(),
                    });
                    SecretSyncRecord {
                        secret_id: secret.id,
                        target: SyncTarget::Ci,
                        version: previous.as_ref().and_then(|row| row.version),
                        synced_at: previous.as_ref().and_then(|row| row.synced_at),
                        error: Some(reason),
                    }
                }
            };
            store.put_secret_sync(&record).await?;
        }
        Ok(report)
    }
}
