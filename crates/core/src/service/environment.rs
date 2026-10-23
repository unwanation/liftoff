//! `EnvironmentService` (test cases ENV-04..ENV-08).
//!
//! The v1 target rules in environment terms:
//!
//! - `add` pins the environment to a branch; the actor's own forge
//!   connection must be `admin` of the repository, and the actor becomes
//!   `pinned_by` (ENV-04, ENV-05);
//! - `stop` is `docker compose down` keeping volumes, `state = stopped`
//!   and the proxy back to the 503 page (ENV-06); `start` brings it up
//!   again without pull;
//! - `remove` needs an explicit confirmation, then `docker compose down
//!   -v`, `systemctl stop` of the slice, bindings and deploys deleted;
//!   domains stay, unbound (ENV-07);
//! - nothing of this runs while a deploy of the environment is queued or
//!   running (ENV-08).

use std::sync::Arc;

use super::deploy::DeployService;
use super::settings::Settings;
use super::support::{
    apply_proxy, authorized_environment, authorized_project, compose_only, docker_ok,
    ensure_no_deploy_in_flight, ensure_repo_admin,
};
use futures_core::Stream;

use crate::domain::{
    Branch, ComposeProject, Cpus, Deploy, EnvKind, EnvName, EnvState, Environment, EnvironmentId,
    Limits, MemoryMb, Pin, ProjectName, ServiceName, SliceName, User,
};
use crate::error::Error;
use crate::ports::{
    Action, Clock as _, Docker as _, FunctionHost as _, HookResult, Hooks as _, Ports, ProjectFile,
    ProjectFiles as _, Rng as _, Store as _, Systemd as _,
};

/// What `PATCH /projects/{p}/environments/{e}` changes; `None` keeps the
/// field. Validated by the adapter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentChange {
    /// A new branch to pin to; the actor becomes `pinned_by` and must be
    /// `admin` of the repository, as for [`EnvironmentService::add`].
    pub branch: Option<Branch>,
    /// A new CPU allowance of the slice (`lof admin env set --cpus`);
    /// admins only.
    pub cpus: Option<Cpus>,
    /// A new memory allowance of the slice (`lof admin env set --memory`);
    /// admins only.
    pub memory_mb: Option<MemoryMb>,
}

/// The lines of [`EnvironmentService::logs`], as they come.
pub type LogLines = core::pin::Pin<Box<dyn Stream<Item = Result<String, Error>> + Send>>;

/// What `lof logs` asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogsRequest {
    /// One compose service; every service when `None`.
    pub service: Option<ServiceName>,
    /// The last lines only.
    pub tail: Option<u32>,
    /// Keep streaming.
    pub follow: bool,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct EnvironmentService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for EnvironmentService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> EnvironmentService<P> {
    /// A service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// The environments of `project`, by name.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown project; whatever the store
    /// reports.
    pub async fn list(
        &self,
        actor: &User,
        project: &ProjectName,
    ) -> Result<Vec<Environment>, Error> {
        let project =
            authorized_project(self.ports.as_ref(), actor, Action::EnvironmentList, project)
                .await?;
        self.ports.store().environments_of_project(project.id).await
    }

    /// The environment `env` of `project`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown project or environment.
    pub async fn get(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
    ) -> Result<Environment, Error> {
        Ok(authorized_environment(
            self.ports.as_ref(),
            actor,
            Action::EnvironmentGet,
            project,
            env,
        )
        .await?
        .1)
    }

    /// Changes the pin and the limits of an environment.
    ///
    /// A new branch is checked like [`EnvironmentService::add`] checks it:
    /// the actor's own connection must administer the repository, and the
    /// actor becomes `pinned_by`. New limits need an admin and go to the
    /// slice at once while the environment runs; otherwise they are only
    /// stored and applied at the next start or deploy.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] for an unknown project or environment;
    /// - [`Error::Forbidden`] for a branch without repository admin, or
    ///   limits from a non-admin;
    /// - [`Error::Upstream`] when systemd fails; nothing is stored then.
    pub async fn update(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
        change: EnvironmentChange,
    ) -> Result<Environment, Error> {
        let (project, mut environment) = authorized_environment(
            self.ports.as_ref(),
            actor,
            Action::EnvironmentUpdate,
            project,
            env,
        )
        .await?;
        let changes_limits = change.cpus.is_some() || change.memory_mb.is_some();
        if changes_limits && !actor.is_admin() {
            return Err(Error::forbidden("only an admin may change limits"));
        }
        if changes_limits {
            compose_only(&environment, "a slice's limits")?;
        }
        if let Some(branch) = change.branch {
            ensure_repo_admin(self.ports.as_ref(), actor, &project).await?;
            environment.pin = Some(Pin {
                branch,
                pinned_by: actor.id,
            });
        }
        if changes_limits {
            let limits = Limits {
                cpus: change.cpus.unwrap_or(environment.limits.cpus),
                memory_mb: change.memory_mb.unwrap_or(environment.limits.memory_mb),
            };
            if environment.state == EnvState::Running {
                self.ports
                    .systemd()
                    .set_slice_limits(&SliceName::new(&project.name, &environment.name), limits)
                    .await?;
            }
            environment.limits = limits;
        }
        self.ports.store().update_environment(&environment).await?;
        Ok(environment)
    }

    /// The kept deploys of an environment, newest first.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown project or environment.
    pub async fn deploys(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
    ) -> Result<Vec<Deploy>, Error> {
        let (_, environment) = authorized_environment(
            self.ports.as_ref(),
            actor,
            Action::EnvironmentDeploys,
            project,
            env,
        )
        .await?;
        self.ports
            .store()
            .deploys_of_environment(environment.id)
            .await
    }

    /// The container logs of an environment, one item per line
    /// (`docker compose logs`); of a function, what it wrote to its
    /// stdout and stderr (a service names nothing in a function, [`Error::Validation`] on `service`).
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] unless `actor` owns the environment (SEC-10);
    /// [`Error::NotFound`] for an unknown project or environment;
    /// whatever docker reports when the command cannot start.
    pub async fn logs(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
        request: &LogsRequest,
    ) -> Result<LogLines, Error> {
        let (project, environment) = authorized_environment(
            self.ports.as_ref(),
            actor,
            Action::EnvironmentLogs,
            project,
            env,
        )
        .await?;
        if environment.kind == EnvKind::Function {
            if request.service.is_some() {
                return Err(Error::invalid(
                    "service",
                    format!("{} is a function: it has no services", environment.name),
                ));
            }
            let lines = self
                .ports
                .functions()
                .logs(environment.id, request.tail, request.follow)
                .await?;
            return Ok(Box::pin(lines));
        }
        compose_only(&environment, "docker compose logs")?;
        let compose = ComposeProject::new(&project.name, &environment.name);
        let lines = self
            .ports
            .docker()
            .logs(
                &compose,
                request.service.as_ref(),
                request.tail,
                request.follow,
            )
            .await?;
        Ok(Box::pin(lines))
    }

    /// Adds the empty environment `name` of `kind` to `project`, pinned to
    /// `branch` by `actor` (`pinned_by`).
    ///
    /// The actor's connection on the project's forge host is asked through
    /// [`Git::repo_role`](crate::ports::Git::repo_role) and must be
    /// [`RepoRole::Admin`](crate::ports::RepoRole::Admin) (ENV-04). Its
    /// limits are the default, or those of the project's tenant
    /// ([`Hooks::tenant_quotas`](crate::ports::Hooks::tenant_quotas),
    /// TEN-05).
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] for an unknown project;
    /// - [`Error::Forbidden`] without a connection on the forge host (the
    ///   message says to run `lof git connect`) or below `admin` (ENV-04);
    /// - [`Error::Validation`] on `kind` for [`EnvKind::Tunnel`]: a tunnel
    ///   is created by `GET /tunnel`, private and owned by its user (VIS-01);
    /// - [`Error::Conflict`] when the project already has an environment
    ///   of that name (ENV-05);
    /// - [`Error::Unavailable`] when the module that keeps the tenant's
    ///   quotas could not answer;
    /// - whatever the forge or the store report.
    pub async fn add(
        &self,
        actor: &User,
        project: &ProjectName,
        name: EnvName,
        branch: Branch,
        kind: EnvKind,
    ) -> Result<Environment, Error> {
        let store = self.ports.store();
        let project =
            authorized_project(self.ports.as_ref(), actor, Action::EnvironmentAdd, project).await?;
        if kind == EnvKind::Tunnel {
            return Err(Error::invalid(
                "kind",
                "a tunnel environment is created by `lof dev` (GET /tunnel), not added",
            ));
        }
        if store
            .environment_by_name(project.id, &name)
            .await?
            .is_some()
        {
            return Err(Error::conflict(format!(
                "environment {name} of {} already exists",
                project.name
            )));
        }
        ensure_repo_admin(self.ports.as_ref(), actor, &project).await?;
        // TEN-05: a tenant's limits go into its new environments.
        let limits = match project.tenant_id {
            Some(tenant) => self
                .ports
                .hooks()
                .tenant_quotas(tenant)
                .await?
                .map_or_else(Limits::default, |quotas| quotas.environment_limits()),
            None => Limits::default(),
        };
        let environment = Environment {
            id: EnvironmentId::new(self.ports.rng().ulid()),
            project_id: project.id,
            name,
            kind,
            pin: Some(Pin {
                branch,
                pinned_by: actor.id,
            }),
            owner_user_id: None,
            limits,
            state: EnvState::Empty,
            current_deploy_id: None,
            visibility: kind.default_visibility(),
            protected: false,
            created_at: self.ports.clock().now(),
        };
        store.insert_environment(&environment).await?;
        Ok(environment)
    }

    /// Stops a running environment: `docker compose down` keeping the
    /// volumes (a function has none to take down), `state = stopped`,
    /// and its domains on the 503 page (ENV-06).
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the environment (SEC-10);
    /// - [`Error::NotFound`] for an unknown project or environment;
    /// - [`Error::Conflict`] while a deploy of it is queued or running
    ///   (ENV-08);
    /// - [`Error::Upstream`] when docker or the proxy fail.
    pub async fn stop(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
    ) -> Result<Environment, Error> {
        let (project, mut environment) = authorized_environment(
            self.ports.as_ref(),
            actor,
            Action::EnvironmentStop,
            project,
            env,
        )
        .await?;
        if environment.kind != EnvKind::Function {
            compose_only(&environment, "docker compose down")?;
        }
        ensure_no_deploy_in_flight(self.ports.as_ref(), &environment).await?;
        if !matches!(environment.state, EnvState::Running | EnvState::Failed) {
            return Err(Error::conflict(format!(
                "environment {} is {}, only a running environment can be stopped",
                environment.name, environment.state
            )));
        }
        // A function has no containers: its domains go to the 503 page
        // and its listener answers 503; its component stays loaded for
        // `start`.
        if environment.kind == EnvKind::Compose {
            let compose = ComposeProject::new(&project.name, &environment.name);
            docker_ok(&self.ports.docker().compose_down(&compose, false).await?)?;
        }
        environment.state = EnvState::Stopped;
        self.ports.store().update_environment(&environment).await?;
        // Docker is down already: the stored state stays `stopped` even
        // when the proxy refuses; the next apply catches up.
        apply_proxy(self.ports.as_ref(), &self.settings).await?;
        Ok(environment)
    }

    /// Starts a stopped environment from the files on disk: `docker
    /// compose up` without pull, `state = running`, its domains back on
    /// their host ports. A function goes back to `running` on its active
    /// component (activated again first when it is not, as the daemon's
    /// start does), its domains back on the function listener.
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the environment (SEC-10);
    /// - [`Error::NotFound`] for an unknown project or environment;
    /// - [`Error::Conflict`] while a deploy of it is queued or running,
    ///   or when it was never deployed;
    /// - [`Error::Upstream`] when docker or the proxy fail;
    /// - whatever [`DeployService`]'s activation reports when a function's
    ///   component does not come back (the environment stays stopped).
    pub async fn start(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
    ) -> Result<Environment, Error> {
        let (project, mut environment) = authorized_environment(
            self.ports.as_ref(),
            actor,
            Action::EnvironmentStart,
            project,
            env,
        )
        .await?;
        if environment.kind != EnvKind::Function {
            compose_only(&environment, "docker compose up")?;
        }
        ensure_no_deploy_in_flight(self.ports.as_ref(), &environment).await?;
        match environment.state {
            EnvState::Stopped => {}
            EnvState::Empty => {
                return Err(Error::conflict(format!(
                    "environment {} has no deploy yet",
                    environment.name
                )));
            }
            EnvState::Running | EnvState::Failed | EnvState::Offline => {
                return Err(Error::conflict(format!(
                    "environment {} is {}, only a stopped environment can be started",
                    environment.name, environment.state
                )));
            }
        }
        if environment.kind == EnvKind::Function {
            // A deploy or the start of the daemon activated its current
            // component; when that failed, it is activated now, and a
            // component that does not come back leaves it stopped.
            if !self.ports.functions().is_active(environment.id).await {
                DeployService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
                    .reactivate(&environment)
                    .await?;
            }
            environment.state = EnvState::Running;
            self.ports.store().update_environment(&environment).await?;
            apply_proxy(self.ports.as_ref(), &self.settings).await?;
            return Ok(environment);
        }
        // Limits set while the environment was stopped were only stored.
        self.ports
            .systemd()
            .set_slice_limits(
                &SliceName::new(&project.name, &environment.name),
                environment.limits,
            )
            .await?;
        let compose = ComposeProject::new(&project.name, &environment.name);
        let up = self
            .ports
            .docker()
            .compose_up(&compose, self.settings.wait_timeout)
            .await?;
        docker_ok(&up)?;
        environment.state = EnvState::Running;
        self.ports.store().update_environment(&environment).await?;
        apply_proxy(self.ports.as_ref(), &self.settings).await?;
        Ok(environment)
    }

    /// Removes an environment for good (`lof env rm <name> --yes`):
    /// `docker compose down -v`, `systemctl stop` of its slice, its
    /// project directory, bindings and deploys deleted. Its domains stay,
    /// unbound, on the 503 page (ENV-07); a tunnel's dev address goes
    /// with it, the `domain_removed` hook told (DEVT-08).
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the environment (SEC-10);
    /// - [`Error::Validation`] unless `confirmed` (ENV-07);
    /// - [`Error::NotFound`] for an unknown project or environment;
    /// - [`Error::Conflict`] while a deploy of it is queued or running;
    /// - [`Error::Upstream`] when docker, systemd or the proxy fail.
    pub async fn remove(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
        confirmed: bool,
    ) -> Result<(), Error> {
        let (project, environment) = authorized_environment(
            self.ports.as_ref(),
            actor,
            Action::EnvironmentRemove,
            project,
            env,
        )
        .await?;
        if !confirmed {
            return Err(Error::invalid(
                "confirm",
                format!("removing {env} deletes its volumes and deploys; confirm with --yes"),
            ));
        }
        ensure_no_deploy_in_flight(self.ports.as_ref(), &environment).await?;
        let compose = ComposeProject::new(&project.name, &environment.name);
        // The host is cleaned first and the rows last, so a failure part
        // way leaves an environment that can simply be removed again.
        // Nothing on disk means nothing was ever started (or an earlier
        // attempt already cleaned the host).
        if self
            .ports
            .files()
            .read(&compose, ProjectFile::Compose)
            .await?
            .is_some()
        {
            docker_ok(&self.ports.docker().compose_down(&compose, true).await?)?;
            self.ports
                .systemd()
                .stop_slice(&SliceName::new(&project.name, &environment.name))
                .await?;
        }
        self.ports.files().remove_all(&compose).await?;
        let store = self.ports.store();
        // A tunnel's dev address is its own (DEVT-08): it goes with it,
        // where the domains of other kinds stay unbound (ENV-07).
        let mut dev_addresses = Vec::new();
        if environment.kind == EnvKind::Tunnel {
            for binding in store.bindings_of_environment(environment.id).await? {
                if let Some(domain) = store.domain(binding.domain_id).await? {
                    dev_addresses.push(domain);
                }
            }
        }
        store.delete_environment(environment.id).await?;
        for domain in &dev_addresses {
            store.delete_domain(domain.id).await?;
        }
        apply_proxy(self.ports.as_ref(), &self.settings).await?;
        for domain in &dev_addresses {
            if let HookResult::Deny(reason) = self.ports.hooks().domain_removed(domain).await {
                tracing::warn!(host = %domain.host, %reason, "domain_removed hook denied after a tunnel's address was removed");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DeployStatus, EnvState, Host, Limits, UserRole};
    use crate::error::System;
    use crate::fakes::{Call, DockerCall, GitCall, SystemdCall, World};
    use crate::ports::{HookResult, RepoRole};
    use crate::proxy_config::{ProxyConfig, Route};

    fn project(raw: &str) -> ProjectName {
        ProjectName::new(raw).unwrap()
    }

    fn env(raw: &str) -> EnvName {
        EnvName::new(raw).unwrap()
    }

    fn branch(raw: &str) -> Branch {
        Branch::new(raw).unwrap()
    }

    fn route_of(config: Option<ProxyConfig>, host: &str) -> Option<Route> {
        let host = Host::new(host).unwrap();
        config?
            .sites
            .into_iter()
            .find(|site| site.host == host)
            .map(|site| site.route)
    }

    /// Docker and systemd calls as `(what, compose project or slice)`.
    fn host_calls(world: &World) -> Vec<(&'static str, String)> {
        world
            .log()
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Docker(DockerCall::Down { project, volumes }) => Some((
                    if volumes { "down -v" } else { "down" },
                    project.as_str().to_owned(),
                )),
                Call::Docker(DockerCall::Up { project, .. }) => {
                    Some(("up", project.as_str().to_owned()))
                }
                Call::Docker(DockerCall::Pull(project)) => {
                    Some(("pull", project.as_str().to_owned()))
                }
                Call::Systemd(SystemdCall::StopSlice(slice)) => {
                    Some(("stop slice", slice.as_str().to_owned()))
                }
                _ => None,
            })
            .collect()
    }

    mod add {
        use super::*;

        // TC: ENV-04
        #[tokio::test]
        async fn pins_branch_by_actor_when_actor_is_repo_admin() {
            let world = World::new().await;
            world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap();
            let stored = world
                .ports
                .store
                .environment_by_name(world.shop.id, &env("staging"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    stored.kind,
                    stored.state,
                    stored.pin.map(|pin| (pin.branch, pin.pinned_by)),
                ),
                (
                    EnvKind::Compose,
                    EnvState::Empty,
                    Some((branch("dev"), world.owner.id))
                )
            );
        }

        // TC: TEN-05 (fail closed: when the module that keeps the
        // tenant's limits cannot answer, no environment is added with the
        // default ones)
        #[tokio::test]
        async fn unavailable_and_nothing_stored_when_tenant_quotas_fail() {
            use crate::domain::TenantId;
            use crate::ports::Rng as _;
            let world = World::new().await;
            let mut shop = world.shop.clone();
            shop.tenant_id = Some(TenantId::new(world.ports.rng.ulid()));
            world.ports.store.update_project(&shop).await.unwrap();
            world
                .ports
                .hooks
                .answer_quotas(Err(Error::unavailable("module tenants failed")));
            let added = world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await;
            let stored = world
                .ports
                .store
                .environment_by_name(world.shop.id, &env("staging"))
                .await
                .unwrap();
            assert_eq!(
                (
                    matches!(added, Err(Error::Unavailable(_))),
                    stored.is_none()
                ),
                (true, true),
                "{added:?}"
            );
        }

        // TC: ENV-04 (the role is asked as the actor's own connection)
        #[tokio::test]
        async fn asks_repo_role_as_actor_when_adding() {
            let world = World::new().await;
            world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap();
            let asked: Vec<(String, String)> = world
                .ports
                .git
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    GitCall::RepoRole { login, repo } => {
                        Some((login.to_string(), repo.to_string()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(asked, vec![("owner".to_owned(), "owner/shop".to_owned())]);
        }

        // TC: ENV-04
        #[tokio::test]
        async fn forbidden_when_actor_role_is_read() {
            let world = World::new().await;
            world
                .ports
                .git
                .grant(&world.owner.login, &world.shop.repo, RepoRole::Read);
            let err = world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
        }

        // TC: ENV-04 (write is not enough either)
        #[tokio::test]
        async fn forbidden_when_actor_role_is_write() {
            let world = World::new().await;
            world
                .ports
                .git
                .grant(&world.owner.login, &world.shop.repo, RepoRole::Write);
            let err = world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
        }

        // TC: GITF-05 (a stored connection past its `expires_at` is not
        // sent to the forge)
        #[tokio::test]
        async fn forbidden_with_git_connect_hint_without_asking_forge_when_connection_expired() {
            let world = World::new().await;
            let host = Host::new("git.example.com").unwrap();
            let mut connection = world
                .ports
                .store
                .git_connection(world.owner.id, &host)
                .await
                .unwrap()
                .unwrap();
            connection.expires_at = Some(world.ports.clock.now());
            world
                .ports
                .store
                .put_git_connection(&connection)
                .await
                .unwrap();
            let err = world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Forbidden(why)
                    if why.contains("expired") && why.contains("lof git connect forgejo --host git.example.com"))
                    && world.ports.git.calls().is_empty(),
                "{err:?} {:?}",
                world.ports.git.calls()
            );
        }

        // TC: GITF-05 (a connection that expires later is still sent)
        #[tokio::test]
        async fn served_when_connection_expires_later() {
            let world = World::new().await;
            let host = Host::new("git.example.com").unwrap();
            let mut connection = world
                .ports
                .store
                .git_connection(world.owner.id, &host)
                .await
                .unwrap()
                .unwrap();
            connection.expires_at = Some(world.ports.clock.now() + time::Duration::days(1));
            world
                .ports
                .store
                .put_git_connection(&connection)
                .await
                .unwrap();
            world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap();
            let asked = world
                .ports
                .git
                .calls()
                .into_iter()
                .filter(|call| matches!(call, GitCall::RepoRole { .. }))
                .count();
            assert_eq!(asked, 1);
        }

        // TC: ENV-04
        #[tokio::test]
        async fn forbidden_with_git_connect_hint_when_actor_has_no_connection() {
            let world = World::new().await;
            world
                .ports
                .store
                .delete_git_connection(world.owner.id, &Host::new("git.example.com").unwrap())
                .await
                .unwrap();
            let err = world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Forbidden(why) if why.contains("lof git connect")),
                "{err:?}"
            );
        }

        // TC: ENV-04 (the actor, not the project's creator, must be admin)
        #[tokio::test]
        async fn forbidden_when_actor_is_read_while_creator_is_admin() {
            let world = World::new().await;
            let alice = world.add_connected_user("alice").await.unwrap();
            world
                .ports
                .git
                .grant(&alice.login, &world.shop.repo, RepoRole::Read);
            let err = world
                .environments()
                .add(
                    &alice,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
        }

        // TC: ENV-04 (the actor's connection is asked and the actor pins)
        #[tokio::test]
        async fn pins_by_actor_asking_as_actor_when_actor_is_not_creator() {
            let world = World::new().await;
            let alice = world.add_connected_user("alice").await.unwrap();
            world
                .ports
                .git
                .grant(&alice.login, &world.shop.repo, RepoRole::Admin);
            // A module admits the non-admin (MOD-05); the forge rule
            // still applies on top of `authorize`.
            world
                .ports
                .hooks
                .answer_next("authorize", HookResult::Allow);
            let added = world
                .environments()
                .add(
                    &alice,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap();
            let asked: Vec<String> = world
                .ports
                .git
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    GitCall::RepoRole { login, .. } => Some(login.to_string()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                (added.pin.map(|pin| pin.pinned_by), asked),
                (Some(alice.id), vec!["alice".to_owned()])
            );
        }

        // TC: ENV-04 (the creator's connection does not stand in for the actor's)
        #[tokio::test]
        async fn forbidden_with_git_connect_hint_when_only_creator_is_connected() {
            let world = World::new().await;
            let alice = world.add_user("alice", UserRole::User).await.unwrap();
            // A module admits the non-admin (MOD-05); the forge rule
            // still applies on top of `authorize`.
            world
                .ports
                .hooks
                .answer_next("authorize", HookResult::Allow);
            let err = world
                .environments()
                .add(
                    &alice,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Forbidden(why) if why.contains("lof git connect")),
                "{err:?}"
            );
        }

        // TC: ENV-04 (a refused add stores nothing)
        #[tokio::test]
        async fn stores_nothing_when_forbidden() {
            let world = World::new().await;
            world
                .ports
                .git
                .grant(&world.owner.login, &world.shop.repo, RepoRole::Read);
            let _refused = world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await;
            let stored = world
                .ports
                .store
                .environment_by_name(world.shop.id, &env("staging"))
                .await
                .unwrap();
            assert_eq!(stored, None);
        }

        // TC: ENV-04
        #[tokio::test]
        async fn not_found_when_project_unknown() {
            let world = World::new().await;
            let err = world
                .environments()
                .add(
                    &world.owner,
                    &project("nope"),
                    env("staging"),
                    branch("dev"),
                    EnvKind::Compose,
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        }

        // TC: ENV-05
        #[tokio::test]
        async fn conflicts_when_name_taken_in_project() {
            let world = World::new().await;
            let err = world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("prod"),
                    branch("release"),
                    EnvKind::Compose,
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: VIS-01 (a tunnel is created by `GET /tunnel`, private and owned by its user; never by this call, which would leave it shared and ownerless)
        #[tokio::test]
        async fn validation_naming_kind_and_nothing_stored_when_kind_is_tunnel() {
            let world = World::new().await;
            let added = world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("shop-dev"),
                    branch("dev"),
                    EnvKind::Tunnel,
                )
                .await;
            let stored = world
                .ports
                .store
                .environment_by_name(world.shop.id, &env("shop-dev"))
                .await
                .unwrap();
            assert!(
                matches!(&added, Err(Error::Validation(v)) if v.field == "kind")
                    && stored.is_none(),
                "{added:?} {stored:?}"
            );
        }

        // TC: VIS-01 (a function starts with the visibility of its kind)
        #[tokio::test]
        async fn shared_without_owner_when_kind_is_function() {
            let world = World::new().await;
            let added = world
                .environments()
                .add(
                    &world.owner,
                    &project("shop"),
                    env("api"),
                    branch("dev"),
                    EnvKind::Function,
                )
                .await
                .unwrap();
            assert_eq!(
                (added.kind, added.visibility, added.owner_user_id),
                (
                    EnvKind::Function,
                    EnvKind::Function.default_visibility(),
                    None
                )
            );
        }

        // TC: ENV-05
        #[tokio::test]
        async fn adds_when_name_taken_in_other_project() {
            let world = World::new().await;
            let blog = world.add_project("blog", "owner/blog").await.unwrap();
            let added = world
                .environments()
                .add(
                    &world.owner,
                    &project("blog"),
                    env("prod"),
                    branch("main"),
                    EnvKind::Compose,
                )
                .await
                .unwrap();
            assert_eq!((added.project_id, added.name.as_str()), (blog.id, "prod"));
        }
    }

    mod stop {
        use super::*;

        // TC: REST-04, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world.add_user("carol", UserRole::User).await.unwrap();
            world.log().clear();
            let error = world
                .environments()
                .stop(&carol, &project("shop"), &env("prod"))
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }

        // TC: ENV-06
        #[tokio::test]
        async fn downs_keeping_volumes_when_running() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world.log().clear();
            world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap();
            assert_eq!(
                host_calls(&world),
                vec![("down", "lf_shop_prod".to_owned())]
            );
        }

        // TC: ENV-06
        #[tokio::test]
        async fn stores_stopped_when_running() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            let returned = world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap();
            let stored = world
                .ports
                .store
                .environment(world.prod.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (returned.state, stored.state),
                (EnvState::Stopped, EnvState::Stopped)
            );
        }

        // TC: ENV-06
        #[tokio::test]
        async fn routes_domains_to_503_when_stopped() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap();
            assert_eq!(
                route_of(world.ports.proxy.applied(), "shop.apps.test"),
                Some(Route::Unavailable)
            );
        }

        // TC: ENV-06 (the proxy follows docker, once)
        #[tokio::test]
        async fn applies_proxy_once_after_down_when_stopped() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world.log().clear();
            world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap();
            let order: Vec<&str> = world
                .log()
                .external()
                .iter()
                .filter_map(|call| match call {
                    Call::Docker(DockerCall::Down { .. }) => Some("down"),
                    Call::Proxy(_) => Some("apply"),
                    _ => None,
                })
                .collect();
            assert_eq!(order, vec!["down", "apply"]);
        }

        // TC: ENV-06 (compensation: a failed down changes nothing)
        #[tokio::test]
        async fn keeps_running_without_apply_when_down_fails() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .ports
                .docker
                .fail_next("compose_down", Error::upstream(System::Docker, "no daemon"));
            let result = world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await;
            let stored = world
                .ports
                .store
                .environment(world.prod.id)
                .await
                .unwrap()
                .unwrap();
            assert!(
                result.is_err()
                    && stored.state == EnvState::Running
                    && world.ports.proxy.apply_count() == 0,
                "{result:?}, state {:?}, applies {}",
                stored.state,
                world.ports.proxy.apply_count()
            );
        }

        // TC: ENV-08
        #[tokio::test]
        async fn conflicts_when_deploy_running() {
            let world = World::new().await;
            let prod = world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .add_deploy(&prod, 2, DeployStatus::Running)
                .await
                .unwrap();
            let err = world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: ENV-08 (a queued deploy would undo the stop)
        #[tokio::test]
        async fn conflicts_when_deploy_queued() {
            let world = World::new().await;
            let prod = world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .add_deploy(&prod, 2, DeployStatus::Queued)
                .await
                .unwrap();
            let err = world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: ENV-08 (docker is not touched)
        #[tokio::test]
        async fn leaves_docker_alone_when_deploy_running() {
            let world = World::new().await;
            let prod = world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .add_deploy(&prod, 2, DeployStatus::Running)
                .await
                .unwrap();
            world.log().clear();
            let _refused = world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await;
            assert_eq!(host_calls(&world), vec![]);
        }

        // TC: ENV-06 (only a running or failed environment has something to stop)
        #[tokio::test]
        async fn conflicts_when_never_deployed() {
            let world = World::new().await;
            let err = world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: ENV-06
        #[tokio::test]
        async fn conflicts_when_already_stopped() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Stopped)
                .await
                .unwrap();
            world.log().clear();
            let err = world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::Conflict(_)) && host_calls(&world).is_empty(),
                "{err:?}"
            );
        }

        // TC: ENV-06 (a failed environment may hold half-started containers)
        #[tokio::test]
        async fn downs_and_stores_stopped_when_failed() {
            let world = World::new().await;
            world.deployed(&world.prod, EnvState::Failed).await.unwrap();
            world.log().clear();
            let stopped = world
                .environments()
                .stop(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap();
            assert_eq!(
                (stopped.state, host_calls(&world)),
                (EnvState::Stopped, vec![("down", "lf_shop_prod".to_owned())])
            );
        }
    }

    mod start {
        use super::*;

        // TC: REST-04, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world.add_user("carol", UserRole::User).await.unwrap();
            world.log().clear();
            let error = world
                .environments()
                .start(&carol, &project("shop"), &env("prod"))
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }

        // TC: ENV-06 (start is the way back: up without pull)
        #[tokio::test]
        async fn ups_without_pull_when_stopped() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Stopped)
                .await
                .unwrap();
            world.log().clear();
            world
                .environments()
                .start(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap();
            assert_eq!(host_calls(&world), vec![("up", "lf_shop_prod".to_owned())]);
        }

        // TC: ENV-06 (limits stored while stopped reach the slice before up)
        #[tokio::test]
        async fn sets_slice_limits_before_up_when_stopped() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Stopped)
                .await
                .unwrap();
            world.log().clear();
            world
                .environments()
                .start(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap();
            let order: Vec<String> = world
                .log()
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Systemd(SystemdCall::SetSliceLimits { slice, limits }) => {
                        assert_eq!(limits, Limits::default());
                        Some(format!("limits {slice}"))
                    }
                    Call::Docker(DockerCall::Up { .. }) => Some("up".to_owned()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                order,
                vec!["limits apps-lf-shop-prod.slice".to_owned(), "up".to_owned()]
            );
        }

        // TC: ENV-06
        #[tokio::test]
        async fn stores_running_and_routes_domains_when_started() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Stopped)
                .await
                .unwrap();
            world
                .environments()
                .start(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap();
            let stored = world
                .ports
                .store
                .environment(world.prod.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    stored.state,
                    route_of(world.ports.proxy.applied(), "shop.apps.test")
                ),
                (
                    EnvState::Running,
                    Some(Route::Upstream(world.binding.host_port))
                )
            );
        }

        // TC: ENV-06 (a failed up leaves it stopped)
        #[tokio::test]
        async fn keeps_stopped_without_apply_when_up_fails() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Stopped)
                .await
                .unwrap();
            world
                .ports
                .docker
                .fail_next_up(["port is already allocated"]);
            let result = world
                .environments()
                .start(&world.owner, &project("shop"), &env("prod"))
                .await;
            let stored = world
                .ports
                .store
                .environment(world.prod.id)
                .await
                .unwrap()
                .unwrap();
            assert!(
                result.is_err()
                    && stored.state == EnvState::Stopped
                    && world.ports.proxy.apply_count() == 0,
                "{result:?}, state {:?}",
                stored.state
            );
        }

        // TC: ENV-06
        #[tokio::test]
        async fn conflicts_when_never_deployed() {
            let world = World::new().await;
            let err = world
                .environments()
                .start(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: ENV-06
        #[tokio::test]
        async fn conflicts_when_already_running() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            let err = world
                .environments()
                .start(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: ENV-08 (start waits for deploys too)
        #[tokio::test]
        async fn conflicts_when_deploy_queued() {
            let world = World::new().await;
            let prod = world
                .deployed(&world.prod, EnvState::Stopped)
                .await
                .unwrap();
            world
                .add_deploy(&prod, 2, DeployStatus::Queued)
                .await
                .unwrap();
            let err = world
                .environments()
                .start(&world.owner, &project("shop"), &env("prod"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }
    }

    mod remove {
        use super::*;

        // TC: REST-04, SEC-09, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world.add_user("carol", UserRole::User).await.unwrap();
            world.log().clear();
            let error = world
                .environments()
                .remove(&carol, &project("shop"), &env("qa"), true)
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }

        // TC: ENV-07
        #[tokio::test]
        async fn validation_when_not_confirmed() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            let err = world
                .environments()
                .remove(&world.owner, &project("shop"), &env("prod"), false)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Validation(_)), "{err:?}");
        }

        // TC: ENV-07 (without --yes nothing happens)
        #[tokio::test]
        async fn touches_nothing_when_not_confirmed() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world.log().clear();
            let _refused = world
                .environments()
                .remove(&world.owner, &project("shop"), &env("prod"), false)
                .await;
            let stored = world.ports.store.environment(world.prod.id).await.unwrap();
            assert_eq!(
                (world.log().only_authorized(), stored.is_some()),
                (true, true)
            );
        }

        // TC: ENV-07
        #[tokio::test]
        async fn downs_with_volumes_then_stops_slice_when_confirmed() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world.log().clear();
            world
                .environments()
                .remove(&world.owner, &project("shop"), &env("prod"), true)
                .await
                .unwrap();
            assert_eq!(
                host_calls(&world),
                vec![
                    ("down -v", "lf_shop_prod".to_owned()),
                    ("stop slice", "apps-lf-shop-prod.slice".to_owned()),
                ]
            );
        }

        // TC: ENV-07
        #[tokio::test]
        async fn deletes_environment_and_bindings_when_confirmed() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .environments()
                .remove(&world.owner, &project("shop"), &env("prod"), true)
                .await
                .unwrap();
            let store = &world.ports.store;
            assert_eq!(
                (
                    store.environment(world.prod.id).await.unwrap(),
                    store.binding_by_domain(world.domain.id).await.unwrap(),
                ),
                (None, None)
            );
        }

        // TC: ENV-07
        #[tokio::test]
        async fn keeps_domains_when_confirmed() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .environments()
                .remove(&world.owner, &project("shop"), &env("prod"), true)
                .await
                .unwrap();
            let kept = world
                .ports
                .store
                .domain_by_host(&Host::new("shop.apps.test").unwrap())
                .await
                .unwrap();
            assert_eq!(kept, Some(world.domain.clone()));
        }

        // TC: ENV-07 (the kept domain answers 503)
        #[tokio::test]
        async fn routes_kept_domains_to_503_when_confirmed() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .environments()
                .remove(&world.owner, &project("shop"), &env("prod"), true)
                .await
                .unwrap();
            assert_eq!(
                route_of(world.ports.proxy.applied(), "shop.apps.test"),
                Some(Route::Unavailable)
            );
        }

        // TC: ENV-07 (the other environment is untouched)
        #[tokio::test]
        async fn keeps_other_environment_when_confirmed() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .environments()
                .remove(&world.owner, &project("shop"), &env("prod"), true)
                .await
                .unwrap();
            let names: Vec<String> = world
                .ports
                .store
                .environments_of_project(world.shop.id)
                .await
                .unwrap()
                .into_iter()
                .map(|e| e.name.to_string())
                .collect();
            assert_eq!(names, vec!["qa".to_owned()]);
        }

        // TC: ENV-08 (remove waits for deploys too)
        #[tokio::test]
        async fn conflicts_when_deploy_running() {
            let world = World::new().await;
            let prod = world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .add_deploy(&prod, 2, DeployStatus::Running)
                .await
                .unwrap();
            let err = world
                .environments()
                .remove(&world.owner, &project("shop"), &env("prod"), true)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }
    }

    mod logs {
        use super::*;

        // TC: REST-04, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world.add_user("carol", UserRole::User).await.unwrap();
            world.log().clear();
            let Err(error) = world
                .environments()
                .logs(
                    &carol,
                    &project("shop"),
                    &env("prod"),
                    &LogsRequest {
                        service: None,
                        tail: None,
                        follow: false,
                    },
                )
                .await
            else {
                panic!("logs served to a non-admin");
            };
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }
    }

    /// Only a compose environment has a compose project and a slice
    /// (a function or a tunnel has neither).
    mod compose_only {
        use super::*;

        /// The tunnel `shop-dev` of `owner`, made to look deployed in
        /// `state`; the log is cleared.
        async fn tunnel_in(world: &World, state: EnvState) -> Environment {
            let tunnel = world
                .add_tunnel(&world.shop, "shop-dev", &world.owner)
                .await
                .unwrap();
            let tunnel = world.deployed(&tunnel, state).await.unwrap();
            world.log().clear();
            tunnel
        }

        /// The function `api` of `shop`, made to look deployed in
        /// `state`; the log is cleared.
        async fn function_in(world: &World, state: EnvState) -> Environment {
            let function = world.records.function(&world.shop, "api").unwrap();
            world
                .ports
                .store
                .insert_environment(&function)
                .await
                .unwrap();
            let function = world.deployed(&function, state).await.unwrap();
            world.log().clear();
            function
        }

        fn names_kind<T: core::fmt::Debug>(result: &Result<T, Error>) -> bool {
            matches!(result, Err(Error::Validation(v)) if v.field == "kind")
        }

        // TC: VIS-01 (no `docker compose down` for a tunnel)
        #[tokio::test]
        async fn validation_naming_kind_without_docker_when_tunnel_stopped() {
            let world = World::new().await;
            let tunnel = tunnel_in(&world, EnvState::Running).await;
            let result = world
                .environments()
                .stop(&world.owner, &project("shop"), &tunnel.name)
                .await;
            assert!(
                names_kind(&result) && host_calls(&world).is_empty(),
                "{result:?} {:?}",
                host_calls(&world)
            );
        }

        /// Whether `world`'s log has a docker or systemd call.
        fn touched_host(world: &World) -> bool {
            world
                .log()
                .calls()
                .into_iter()
                .any(|call| matches!(call, Call::Docker(_) | Call::Systemd(_)))
        }

        // a function stops without docker, its
        // domains on the 503 page.
        #[tokio::test]
        async fn stopped_and_applied_without_host_calls_when_function_stopped() {
            let world = World::new().await;
            let function = function_in(&world, EnvState::Running).await;
            let stopped = world
                .environments()
                .stop(&world.owner, &project("shop"), &function.name)
                .await
                .unwrap();
            let applied = world
                .log()
                .calls()
                .into_iter()
                .any(|call| matches!(call, Call::Proxy(_)));
            assert!(
                stopped.state == EnvState::Stopped && applied && !touched_host(&world),
                "{stopped:?} {:?}",
                world.log().calls()
            );
        }

        // no slice and no `docker compose up` for
        // a function; it runs again on its active component.
        #[tokio::test]
        async fn running_without_host_calls_when_function_started() {
            let world = World::new().await;
            let function = function_in(&world, EnvState::Stopped).await;
            world
                .ports
                .functions
                .set_active(function.id, &crate::fakes::FakeFunctions::component("v1"));
            let started = world
                .environments()
                .start(&world.owner, &project("shop"), &function.name)
                .await
                .unwrap();
            assert!(
                started.state == EnvState::Running && !touched_host(&world),
                "{started:?} {:?}",
                world.log().calls()
            );
        }

        /// The function `api` of `shop`, stopped, whose current deploy #1
        /// is of the kept component `v1`, nothing active (a daemon whose
        /// start could not activate it); the log is cleared. Returns it
        /// and the component.
        async fn stopped_function_with_kept_component(world: &World) -> (Environment, Vec<u8>) {
            let component = crate::fakes::FakeFunctions::component("v1");
            let hash = world.ports.components.put(&component);
            let api = world.records.function(&world.shop, "api").unwrap();
            world.ports.store.insert_environment(&api).await.unwrap();
            let deploy = Deploy {
                component_hash: Some(hash),
                ..world
                    .records
                    .deploy(&api, 1, DeployStatus::Succeeded, &world.owner)
                    .unwrap()
            };
            world.ports.store.insert_deploy(&deploy).await.unwrap();
            let api = Environment {
                state: EnvState::Stopped,
                current_deploy_id: Some(deploy.id),
                ..api
            };
            world.ports.store.update_environment(&api).await.unwrap();
            world.log().clear();
            (api, component)
        }

        // `start` of a function whose component
        // is not active (its activation failed at the daemon's start)
        // activates the current one again before it runs.
        #[tokio::test]
        async fn current_component_active_when_function_started_without_active_one() {
            let world = World::new().await;
            let (api, component) = stopped_function_with_kept_component(&world).await;
            let started = world
                .environments()
                .start(&world.owner, &project("shop"), &api.name)
                .await
                .unwrap();
            assert_eq!(
                (started.state, world.ports.functions.active(api.id)),
                (
                    EnvState::Running,
                    Some(crate::domain::ComponentHash::of(&component))
                )
            );
        }

        // a component that does not come back is
        // the error of `start`, and the function stays stopped (never
        // `running` with nothing to answer).
        #[tokio::test]
        async fn error_and_stopped_when_function_started_and_component_does_not_activate() {
            let world = World::new().await;
            let (api, _component) = stopped_function_with_kept_component(&world).await;
            world
                .ports
                .functions
                .fail_next("activate", Error::invalid("component", "trap"));
            let result = world
                .environments()
                .start(&world.owner, &project("shop"), &api.name)
                .await;
            let stored = world
                .ports
                .store
                .environment(api.id)
                .await
                .unwrap()
                .unwrap();
            assert!(
                result.is_err() && stored.state == EnvState::Stopped,
                "{result:?} {stored:?}"
            );
        }

        // a function never deployed has nothing
        // to start.
        #[tokio::test]
        async fn conflict_when_function_started_without_deploy() {
            let world = World::new().await;
            let function = world.records.function(&world.shop, "api").unwrap();
            world
                .ports
                .store
                .insert_environment(&function)
                .await
                .unwrap();
            let result = world
                .environments()
                .start(&world.owner, &project("shop"), &function.name)
                .await;
            assert!(matches!(result, Err(Error::Conflict(_))), "{result:?}");
        }

        // `lof logs` of a function is what it
        // wrote, through the function host.
        #[tokio::test]
        async fn lines_of_function_when_logs_of_function_asked() {
            let world = World::new().await;
            let function = function_in(&world, EnvState::Running).await;
            world.ports.functions.write_log(function.id, "one");
            world.ports.functions.write_log(function.id, "two");
            world.ports.functions.write_log(function.id, "three");
            let lines = world
                .environments()
                .logs(
                    &world.owner,
                    &project("shop"),
                    &function.name,
                    &LogsRequest {
                        service: None,
                        tail: Some(2),
                        follow: false,
                    },
                )
                .await
                .unwrap();
            let lines: Vec<String> = futures_util::StreamExt::collect::<Vec<_>>(lines)
                .await
                .into_iter()
                .map(Result::unwrap)
                .collect();
            assert!(
                lines == ["two", "three"] && !touched_host(&world),
                "{lines:?} {:?}",
                world.log().calls()
            );
        }

        // a function has no compose services.
        #[tokio::test]
        async fn validation_naming_service_when_logs_of_function_name_service() {
            let world = World::new().await;
            let function = function_in(&world, EnvState::Running).await;
            let result = world
                .environments()
                .logs(
                    &world.owner,
                    &project("shop"),
                    &function.name,
                    &LogsRequest {
                        service: Some(ServiceName::new("web").unwrap()),
                        tail: None,
                        follow: false,
                    },
                )
                .await;
            assert!(
                matches!(&result, Err(Error::Validation(v)) if v.field == "service"),
                "{:?}",
                result.err()
            );
        }

        // TC: VIS-01 (a function has no slice to limit)
        #[tokio::test]
        async fn validation_naming_kind_and_kept_when_limits_set_on_function() {
            let world = World::new().await;
            let function = function_in(&world, EnvState::Running).await;
            let result = world
                .environments()
                .update(
                    &world.owner,
                    &project("shop"),
                    &function.name,
                    EnvironmentChange {
                        memory_mb: Some(MemoryMb::new(2048).unwrap()),
                        ..EnvironmentChange::default()
                    },
                )
                .await;
            let stored = world
                .ports
                .store
                .environment(function.id)
                .await
                .unwrap()
                .map(|stored| stored.limits);
            assert!(
                names_kind(&result) && stored == Some(Limits::default()),
                "{result:?} {stored:?}"
            );
        }

        // TC: VIS-01 (a function's pin still moves: CI deploys it)
        #[tokio::test]
        async fn repinned_when_branch_patched_on_function() {
            let world = World::new().await;
            let function = function_in(&world, EnvState::Running).await;
            let changed = world
                .environments()
                .update(
                    &world.owner,
                    &project("shop"),
                    &function.name,
                    EnvironmentChange {
                        branch: Some(branch("main")),
                        ..EnvironmentChange::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(changed.pin.map(|pin| pin.branch), Some(branch("main")));
        }

        // TC: VIS-01 (no `docker compose logs` for a tunnel)
        #[tokio::test]
        async fn validation_naming_kind_without_docker_when_tunnel_logs_read() {
            let world = World::new().await;
            let tunnel = tunnel_in(&world, EnvState::Running).await;
            let result = world
                .environments()
                .logs(
                    &world.owner,
                    &project("shop"),
                    &tunnel.name,
                    &LogsRequest::default(),
                )
                .await
                .map(drop);
            let touched_docker = world
                .log()
                .calls()
                .into_iter()
                .any(|call| matches!(call, Call::Docker(_)));
            assert!(names_kind(&result) && !touched_docker, "{result:?}");
        }
    }

    mod update {
        use super::*;

        fn cpus(raw: &str) -> Cpus {
            Cpus::parse(raw).unwrap()
        }

        // TC: REST-04 (repinning needs repository admin)
        #[tokio::test]
        async fn repinned_by_the_actor_when_admin_of_the_repository() {
            let world = World::new().await;
            let bob = world.add_connected_user("bob").await.unwrap();
            world
                .ports
                .git
                .grant(&bob.login, &world.shop.repo, RepoRole::Admin);
            // A module admits the non-admin (MOD-05); the forge rule
            // still applies on top of `authorize`.
            world
                .ports
                .hooks
                .answer_next("authorize", HookResult::Allow);
            let qa = world
                .environments()
                .update(
                    &bob,
                    &project("shop"),
                    &env("qa"),
                    EnvironmentChange {
                        branch: Some(branch("staging")),
                        ..EnvironmentChange::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                qa.pin,
                Some(Pin {
                    branch: branch("staging"),
                    pinned_by: bob.id
                })
            );
        }

        // TC: REST-04
        #[tokio::test]
        async fn forbidden_and_kept_when_repinned_without_repository_admin() {
            let world = World::new().await;
            let bob = world.add_connected_user("bob").await.unwrap();
            let error = world
                .environments()
                .update(
                    &bob,
                    &project("shop"),
                    &env("qa"),
                    EnvironmentChange {
                        branch: Some(branch("staging")),
                        ..EnvironmentChange::default()
                    },
                )
                .await
                .unwrap_err();
            let stored = world.ports.store.environment(world.qa.id).await.unwrap();
            assert!(
                matches!(error, Error::Forbidden(_))
                    && stored.and_then(|qa| qa.pin).map(|pin| pin.branch) == Some(branch("dev")),
                "{error:?}"
            );
        }

        // TC: REST-04 (`lof admin env set`: a running slice gets the limits
        // at once)
        #[tokio::test]
        async fn slice_limited_at_once_when_running_environment_gets_limits() {
            let world = World::new().await;
            let _prod = world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            let prod = world
                .environments()
                .update(
                    &world.owner,
                    &project("shop"),
                    &env("prod"),
                    EnvironmentChange {
                        cpus: Some(cpus("0.5")),
                        ..EnvironmentChange::default()
                    },
                )
                .await
                .unwrap();
            let expected = Limits {
                cpus: cpus("0.5"),
                memory_mb: MemoryMb::DEFAULT,
            };
            assert_eq!(
                (
                    prod.limits,
                    world
                        .ports
                        .systemd
                        .limits(&SliceName::new(&world.shop.name, &world.prod.name))
                ),
                (expected, Some(expected))
            );
        }

        // TC: REST-04
        #[tokio::test]
        async fn forbidden_when_a_non_admin_changes_limits() {
            let world = World::new().await;
            let bob = world.add_user("bob", UserRole::User).await.unwrap();
            let error = world
                .environments()
                .update(
                    &bob,
                    &project("shop"),
                    &env("qa"),
                    EnvironmentChange {
                        memory_mb: Some(MemoryMb::new(2048).unwrap()),
                        ..EnvironmentChange::default()
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Forbidden(_)), "{error:?}");
        }
    }
}
