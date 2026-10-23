//! [`TunnelsService`]: the tunnel environments of `lof dev`.
//!
//! - [`TunnelsService::open`] (`GET /api/v1/tunnel` with the project in
//!   `X-Liftoff-Project`): finds or creates the actor's tunnel environment
//!   of the project, `<project>-dev` in single-user mode and
//!   `<project>-<login>` with the `tenants` module (DEVT-01, DEVT-02),
//!   private and owned by the actor, with its managed domain
//!   `<env>.<base>` (the `domain_created` hook asked) bound to it; for a
//!   private environment it issues the one-time link the upgrade answers
//!   in `X-Liftoff-Login-Url` (DEVT-07). A second `open` finds the same
//!   environment and domain (DEVT-04);
//! - [`TunnelsService::connected`]: the client's handshake ended, the
//!   environment is `running` and its domain goes to `[tunnel] bind`
//!   ([`Proxy::apply`](crate::ports::Proxy::apply), DEVT-01, DEVT-03);
//! - [`TunnelsService::disconnected`]: the connection ended. A
//!   [`TunnelEnd::Shutdown`] (Ctrl-C) is `offline` at once; a
//!   [`TunnelEnd::Lost`] connection waits `tunnel.offline_after`
//!   ([`Settings::tunnel_offline_after`]) for its client, after which
//!   [`TunnelsService::expire`] makes it `offline` (Caddy's «dev
//!   offline» page, DEVT-03); an [`TunnelEnd::Evicted`] one was replaced
//!   and changes nothing;
//! - the start-up reconciliation makes every tunnel `offline`, and `rm` of a tunnel removes its domain with the route
//!   (DEVT-08).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use super::domain::DomainService;
use super::links::LinksService;
use super::settings::Settings;
use super::support::{apply_proxy, authorized_project};
use crate::domain::{
    Binding, BindingId, ContainerPort, DomainKind, EnvKind, EnvName, EnvState, Environment,
    EnvironmentId, Host, Label, Limits, LinkTarget, ModuleName, Project, ProjectName, ServiceName,
    Timestamp, User, Visibility,
};
use crate::error::Error;
use crate::ports::{Action, Clock as _, Ports, Rng as _, Store as _};

/// The module whose presence names tunnels after their user.
const TENANTS_MODULE: &str = "tenants";

/// The `service` of a tunnel's binding: a tunnel has no compose service;
/// the binding only ties its dev address to it.
const TUNNEL_SERVICE: &str = "tunnel";

/// The `container_port` of a tunnel's binding (see [`TUNNEL_SERVICE`]).
const TUNNEL_CONTAINER_PORT: u16 = 80;

/// What [`TunnelsService::open`] found or created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedTunnel {
    /// The tunnel environment of the actor in the project.
    pub environment: Environment,
    /// Its dev address, `<env>.<base>`.
    pub host: Host,
    /// A one-time link of the actor to the environment
    /// (`https://<api>/api/v1/auth/links/<token>`) while it is private,
    /// for `lof dev` to print; `None` for a shared one.
    pub login_url: Option<String>,
}

/// How a tunnel connection ended, as the daemon's tunnel server tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelEnd {
    /// The client sent `Shutdown` (Ctrl-C): offline at once.
    Shutdown,
    /// The connection broke or went idle: offline after
    /// `tunnel.offline_after` unless the client comes back.
    Lost,
    /// A newer connection of the same environment replaced it.
    Evicted,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct TunnelsService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
    /// The newest connection of each tunnel and, while its client is
    /// away, when it was lost; shared between clones.
    live: Arc<Mutex<HashMap<EnvironmentId, Live>>>,
}

/// What [`TunnelsService`] knows of a tunnel's newest connection.
#[derive(Debug, Clone, Copy)]
struct Live {
    /// The tunnel server's number of the connection: a later
    /// registration has a greater one.
    connection: u64,
    /// When the connection was lost, while its client did not come back.
    lost_since: Option<Timestamp>,
}

impl<P> Clone for TunnelsService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
            live: Arc::clone(&self.live),
        }
    }
}

impl<P: Ports> TunnelsService<P> {
    /// A service over `ports` configured by `settings`. Clones share
    /// what the service knows of connections: the daemon builds one and
    /// hands clones to the REST services and to its tunnel server.
    #[must_use]
    pub fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self {
            ports,
            settings,
            live: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// How long a lost tunnel waits for its client (`tunnel.offline_after`):
    /// the daemon calls [`TunnelsService::expire`] that long after a loss.
    #[must_use]
    pub fn offline_after(&self) -> core::time::Duration {
        self.settings.tunnel_offline_after
    }

    /// `GET /api/v1/tunnel`: the tunnel environment of `actor` in
    /// `project`, found or created (see the [module
    /// documentation](self)). The `authorize` hook is asked once
    /// ([`Action::TunnelOpen`] about the project).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown project; [`Error::Forbidden`]
    /// when `authorize` refuses or the domain hook denies;
    /// [`Error::Conflict`] when the name or the dev address is taken by
    /// something else than the actor's tunnel; [`Error::Validation`] when
    /// the name would not be an environment name; whatever the store or
    /// the proxy report.
    pub async fn open(&self, actor: &User, project: &ProjectName) -> Result<OpenedTunnel, Error> {
        let ports = self.ports.as_ref();
        let project = authorized_project(ports, actor, Action::TunnelOpen, project).await?;
        let name = self.tunnel_name(&project, actor).await?;
        let environment = self.find_or_create(&project, &name, actor).await?;
        let host = self.dev_address(&project, &environment).await?;
        let login_url = match environment.visibility {
            Visibility::Private => Some(
                LinksService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
                    .issue_checked(actor, &LinkTarget::Environment(environment.id))
                    .await?
                    .url,
            ),
            Visibility::Shared => None,
        };
        Ok(OpenedTunnel {
            environment,
            host,
            login_url,
        })
    }

    /// The client of `environment` finished its handshake on the tunnel
    /// server's `connection` (the number of its registration, greater for
    /// a later one): `running`, its domain routed to `[tunnel] bind`. A
    /// connection older than one already connected changes nothing.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown tunnel environment; whatever
    /// the store or the proxy report.
    pub async fn connected(
        &self,
        environment: EnvironmentId,
        connection: u64,
    ) -> Result<Environment, Error> {
        let newest = {
            let mut live = self.live();
            let newest = live
                .get(&environment)
                .is_none_or(|known| known.connection <= connection);
            if newest {
                let _earlier = live.insert(
                    environment,
                    Live {
                        connection,
                        lost_since: None,
                    },
                );
            }
            newest
        };
        let tunnel = self.tunnel(environment).await?;
        if !newest || tunnel.state == EnvState::Running {
            return Ok(tunnel);
        }
        let running = Environment {
            state: EnvState::Running,
            ..tunnel
        };
        self.store_state(&running).await?;
        tracing::info!(environment = %running.name, "a tunnel connected");
        Ok(running)
    }

    /// The tunnel server's `connection` of `environment` ended as `end`.
    /// The end of a connection a newer one replaced changes nothing,
    /// whatever it reports (an evicted connection leaves the state alone), so a late `Lost` of it cannot take a connected
    /// tunnel offline.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown tunnel environment; whatever
    /// the store or the proxy report.
    pub async fn disconnected(
        &self,
        environment: EnvironmentId,
        connection: u64,
        end: TunnelEnd,
    ) -> Result<(), Error> {
        if end == TunnelEnd::Evicted {
            return Ok(());
        }
        let replaced = self
            .live()
            .get(&environment)
            .is_some_and(|known| known.connection > connection);
        if replaced {
            tracing::debug!(%environment, ?end, "a replaced tunnel connection ended");
            return Ok(());
        }
        match end {
            TunnelEnd::Evicted => Ok(()),
            TunnelEnd::Lost => {
                let now = self.ports.clock().now();
                let _earlier = self.live().insert(
                    environment,
                    Live {
                        connection,
                        lost_since: Some(now),
                    },
                );
                tracing::info!(%environment, "a tunnel lost its client; waiting for it");
                Ok(())
            }
            TunnelEnd::Shutdown => {
                let _forgotten = self.live().remove(&environment);
                let tunnel = self.tunnel(environment).await?;
                self.go_offline(tunnel).await?;
                Ok(())
            }
        }
    }

    /// Every tunnel whose connection was lost at least
    /// `tunnel.offline_after` ago and whose client did not come back is
    /// `offline` now; returns them.
    ///
    /// # Errors
    ///
    /// Whatever the store or the proxy report.
    pub async fn expire(&self) -> Result<Vec<EnvironmentId>, Error> {
        let now = self.ports.clock().now();
        let wait = time::Duration::try_from(self.settings.tunnel_offline_after)
            .unwrap_or(time::Duration::MAX);
        let due: Vec<EnvironmentId> = {
            let mut live = self.live();
            let due: Vec<EnvironmentId> = live
                .iter()
                .filter(|(_, known)| {
                    known
                        .lost_since
                        .is_some_and(|since| since.saturating_add(wait) <= now)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in &due {
                let _forgotten = live.remove(id);
            }
            due
        };
        let mut expired = Vec::new();
        for id in due {
            // A tunnel removed meanwhile has nothing to take offline.
            let Some(tunnel) = self
                .ports
                .store()
                .environment(id)
                .await?
                .filter(|environment| environment.kind == EnvKind::Tunnel)
            else {
                continue;
            };
            if tunnel.state != EnvState::Offline {
                let offline = Environment {
                    state: EnvState::Offline,
                    ..tunnel
                };
                self.ports.store().update_environment(&offline).await?;
                tracing::info!(environment = %offline.name, "a tunnel without its client is offline");
            }
            expired.push(id);
        }
        if !expired.is_empty() {
            apply_proxy(self.ports.as_ref(), &self.settings).await?;
        }
        Ok(expired)
    }

    /// `<project>-dev` in single-user mode, `<project>-<login>` with the
    /// `tenants` module.
    async fn tunnel_name(&self, project: &Project, actor: &User) -> Result<EnvName, Error> {
        let tenants = ModuleName::new(TENANTS_MODULE)?;
        let with_tenants = self
            .ports
            .store()
            .module(&tenants)
            .await?
            .is_some_and(|module| module.enabled);
        let login = with_tenants.then(|| actor.login.as_str());
        let raw = crate::dev::tunnel_name(project.name.as_str(), login);
        EnvName::new(&raw).map_err(|_invalid| {
            Error::invalid(
                "project",
                format!(
                    "the tunnel {raw} of {} would not be an environment name",
                    project.name
                ),
            )
        })
    }

    /// The tunnel `name` of `actor` in `project`: the stored one, else a
    /// new private one, offline until its client connects.
    async fn find_or_create(
        &self,
        project: &Project,
        name: &EnvName,
        actor: &User,
    ) -> Result<Environment, Error> {
        let store = self.ports.store();
        if let Some(found) = store.environment_by_name(project.id, name).await? {
            if found.kind != EnvKind::Tunnel || found.owner_user_id != Some(actor.id) {
                return Err(Error::conflict(format!(
                    "environment {name} of {} exists and is not {}'s tunnel",
                    project.name, actor.login
                )));
            }
            return Ok(found);
        }
        let environment = Environment {
            id: EnvironmentId::new(self.ports.rng().ulid()),
            project_id: project.id,
            name: name.clone(),
            kind: EnvKind::Tunnel,
            pin: None,
            owner_user_id: Some(actor.id),
            limits: Limits::default(),
            state: EnvState::Offline,
            current_deploy_id: None,
            visibility: EnvKind::Tunnel.default_visibility(),
            protected: false,
            created_at: self.ports.clock().now(),
        };
        store.insert_environment(&environment).await?;
        tracing::info!(environment = %environment.name, by = %actor.login, "created a tunnel");
        Ok(environment)
    }

    /// The dev address of `tunnel`: the domain bound to it, else the
    /// managed domain `<env>.<base>` added (the `domain_created` hook
    /// asked) and bound.
    async fn dev_address(&self, project: &Project, tunnel: &Environment) -> Result<Host, Error> {
        let store = self.ports.store();
        if let Some(binding) = store.bindings_of_environment(tunnel.id).await?.first()
            && let Some(domain) = store.domain(binding.domain_id).await?
        {
            return Ok(domain.host);
        }
        let label = Label::new(tunnel.name.as_str())?;
        let domains = DomainService::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        let host = domains.admit_managed(&label).await?;
        let domain = domains
            .store_and_announce(domains.new_domain(project, host, DomainKind::Managed, None))
            .await?;
        // The port is never listened on: Caddy sends a tunnel's address to
        // `[tunnel] bind`. It only keeps the row's place among the
        // bindings, whose host ports are unique.
        let host_port = store
            .free_host_port(&self.settings.port_range)
            .await?
            .ok_or_else(|| Error::unavailable("no free host port"))?;
        let binding = Binding {
            id: BindingId::new(self.ports.rng().ulid()),
            environment_id: tunnel.id,
            domain_id: domain.id,
            service: ServiceName::new(TUNNEL_SERVICE)?,
            container_port: ContainerPort::new(TUNNEL_CONTAINER_PORT)?,
            host_port,
        };
        store.insert_binding(&binding).await?;
        apply_proxy(self.ports.as_ref(), &self.settings).await?;
        Ok(domain.host)
    }

    /// The tunnel environment `id`.
    async fn tunnel(&self, id: EnvironmentId) -> Result<Environment, Error> {
        self.ports
            .store()
            .environment(id)
            .await?
            .filter(|environment| environment.kind == EnvKind::Tunnel)
            .ok_or_else(|| Error::not_found(format!("tunnel {id}")))
    }

    /// `tunnel` offline, its address on the «dev offline» page.
    async fn go_offline(&self, tunnel: Environment) -> Result<(), Error> {
        if tunnel.state == EnvState::Offline {
            return Ok(());
        }
        let offline = Environment {
            state: EnvState::Offline,
            ..tunnel
        };
        self.store_state(&offline).await?;
        tracing::info!(environment = %offline.name, "a tunnel went offline");
        Ok(())
    }

    /// Stores `environment` and pushes the proxy configuration.
    async fn store_state(&self, environment: &Environment) -> Result<(), Error> {
        self.ports.store().update_environment(environment).await?;
        apply_proxy(self.ports.as_ref(), &self.settings).await
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<EnvironmentId, Live>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests;
