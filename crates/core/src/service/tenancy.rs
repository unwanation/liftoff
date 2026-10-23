//! [`TenancyService`]: what the core does for the `tenants` module alone.
//!
//! The module keeps tenants, memberships and quotas in its own tables;
//! the core keeps `projects.tenant_id`, `domains.tenant_id` and
//! `environments.protected`. The module decides who may do what (its
//! `authorize` and `gate` hooks), so these operations ask no `authorize`:
//! asked from within the module's own call, `authorize` would skip the
//! module (no re-entry, `docs/ARCHITECTURE.md`) and leave every
//! non-admin refused. That is why only the module named `tenants` may
//! call them (`core-call`, [`CoreBridge`](crate::module::CoreBridge)).

use std::collections::HashSet;
use std::sync::Arc;

use super::settings::Settings;
use super::support::apply_proxy;
use crate::domain::{
    Domain, DomainId, EnvName, Environment, EnvironmentId, Host, Login, Project, ProjectId,
    ProjectName, TenantId, User,
};
use crate::error::Error;
use crate::ports::{Ports, Store as _};

/// A record whose tenant the module asks about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantOf {
    /// A project.
    Project(ProjectId),
    /// An environment: its project's tenant.
    Environment(EnvironmentId),
    /// A domain: its own tenant, else its project's.
    Domain(DomainId),
    /// The domain of this host, as [`TenantOf::Domain`].
    Host(Host),
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct TenancyService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for TenancyService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> TenancyService<P> {
    /// The service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// Every user, by login (the owner of `default` on install, TEN-01).
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn users(&self) -> Result<Vec<User>, Error> {
        self.ports.store().users().await
    }

    /// The user `login` (`lof tenant members add`, TEN-03).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] naming the login; whatever the store reports.
    pub async fn user(&self, login: &Login) -> Result<User, Error> {
        self.ports
            .store()
            .user_by_login(login)
            .await?
            .ok_or_else(|| Error::not_found(format!("user {login}")))
    }

    /// The tenant of `of`; `None` for a record outside every tenant.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a record that does not exist; whatever the
    /// store reports.
    pub async fn tenant_of(&self, of: &TenantOf) -> Result<Option<TenantId>, Error> {
        let store = self.ports.store();
        match of {
            TenantOf::Project(id) => Ok(self.project(*id).await?.tenant_id),
            TenantOf::Environment(id) => {
                let environment = store
                    .environment(*id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("environment {id}")))?;
                Ok(self.project(environment.project_id).await?.tenant_id)
            }
            TenantOf::Domain(id) => {
                let domain = store
                    .domain(*id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("domain {id}")))?;
                self.domain_tenant(&domain).await
            }
            TenantOf::Host(host) => {
                let domain = store
                    .domain_by_host(host)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("domain {host}")))?;
                self.domain_tenant(&domain).await
            }
        }
    }

    /// The tenant of `domain`: its own, else its project's.
    async fn domain_tenant(&self, domain: &Domain) -> Result<Option<TenantId>, Error> {
        match domain.tenant_id {
            Some(tenant) => Ok(Some(tenant)),
            None => Ok(self.project(domain.project_id).await?.tenant_id),
        }
    }

    /// How many projects `tenant` holds (`lof tenant rm`, TEN-10).
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn project_count(&self, tenant: TenantId) -> Result<usize, Error> {
        Ok(self
            .ports
            .store()
            .projects()
            .await?
            .iter()
            .filter(|project| project.tenant_id == Some(tenant))
            .count())
    }

    /// The environment `env` of the project `project` (`lof protect`).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown project or environment;
    /// whatever the store reports.
    pub async fn environment(
        &self,
        project: &ProjectName,
        env: &EnvName,
    ) -> Result<(Project, Environment), Error> {
        let store = self.ports.store();
        let project = store
            .projects()
            .await?
            .into_iter()
            .find(|candidate| &candidate.name == project)
            .ok_or_else(|| Error::not_found(format!("project {project}")))?;
        let environment = store
            .environment_by_name(project.id, env)
            .await?
            .ok_or_else(|| Error::not_found(format!("environment {env} of {}", project.name)))?;
        Ok((project, environment))
    }

    /// Puts `projects` and `domains` in `tenant` (the v1 tenants adopted
    /// on install, TEN-01); those that no longer exist are skipped. How
    /// many of each it changed.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn assign(
        &self,
        tenant: TenantId,
        projects: &[ProjectId],
        domains: &[DomainId],
    ) -> Result<(usize, usize), Error> {
        let projects: HashSet<ProjectId> = projects.iter().copied().collect();
        let domains: HashSet<DomainId> = domains.iter().copied().collect();
        self.assign_where(
            tenant,
            |project| projects.contains(&project.id),
            |domain| domains.contains(domain),
        )
        .await
    }

    /// Puts every project and domain outside a tenant in `tenant` (the tenant `default` on install, TEN-01). How many of each
    /// it changed.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn adopt(&self, tenant: TenantId) -> Result<(usize, usize), Error> {
        self.assign_where(tenant, |project| project.tenant_id.is_none(), |_| true)
            .await
    }

    /// Sets `tenant` on the projects `project` picks and on the domains
    /// without a tenant that `domain` picks.
    async fn assign_where(
        &self,
        tenant: TenantId,
        project: impl Fn(&Project) -> bool + Send + Sync,
        domain: impl Fn(&DomainId) -> bool + Send + Sync,
    ) -> Result<(usize, usize), Error> {
        let store = self.ports.store();
        let mut projects = 0_usize;
        for mut picked in store.projects().await?.into_iter().filter(|p| project(p)) {
            if picked.tenant_id != Some(tenant) {
                picked.tenant_id = Some(tenant);
                store.update_project(&picked).await?;
                projects = projects.saturating_add(1);
            }
        }
        let mut domains = 0_usize;
        for mut picked in store
            .domains()
            .await?
            .into_iter()
            .filter(|d| d.tenant_id.is_none() && domain(&d.id))
        {
            picked.tenant_id = Some(tenant);
            store.update_domain(&picked).await?;
            domains = domains.saturating_add(1);
        }
        Ok((projects, domains))
    }

    /// `lof protect` and `lof unprotect` (TEN-06, TEN-09): sets
    /// `environments.protected` of `env` of `project` and pushes the
    /// proxy configuration, in which the environment's domains then ask
    /// the gate (or no longer do). Setting the flag it already has
    /// changes nothing and asks the proxy nothing.
    ///
    /// The flag is stored first, alone
    /// ([`Store::set_environment_protected`](crate::ports::Store::set_environment_protected):
    /// no other writer of the environment changes it), then the whole
    /// configuration is read from the store and pushed, as every other
    /// change of routes does: an apply that runs later reads the flag
    /// too. When the proxy refuses, the flag is put back, so both stay
    /// as they were (v1 SSO-02).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown project or environment;
    /// [`Error::Upstream`] with the proxy's text when it refuses; whatever
    /// the store reports.
    pub async fn set_protected(
        &self,
        project: &ProjectName,
        env: &EnvName,
        protected: bool,
    ) -> Result<Environment, Error> {
        let (_project, mut environment) = self.environment(project, env).await?;
        if environment.protected == protected {
            return Ok(environment);
        }
        let store = self.ports.store();
        store
            .set_environment_protected(environment.id, protected)
            .await?;
        if let Err(refused) = apply_proxy(self.ports.as_ref(), &self.settings).await {
            if let Err(error) = store
                .set_environment_protected(environment.id, !protected)
                .await
            {
                tracing::error!(%error, project = %project, env = %env, "could not put the gate flag back after the proxy refused");
            }
            return Err(refused);
        }
        environment.protected = protected;
        tracing::info!(project = %project, env = %env, protected, "changed the gate of an environment");
        Ok(environment)
    }

    /// The project `id`.
    async fn project(&self, id: ProjectId) -> Result<Project, Error> {
        self.ports
            .store()
            .project(id)
            .await?
            .ok_or_else(|| Error::not_found(format!("project {id}")))
    }
}
