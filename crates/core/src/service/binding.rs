//! `BindingService` (test cases BND-01..BND-03).
//!
//! A binding routes a domain of the project to `service:port` of one of
//! its compose environments, or ties it to a
//! function environment ([`BindingService::bind_function`]). Binding takes a host
//! port round the `[domains] port_range`, regenerates
//! `liftoff.override.yaml` when the environment has a current deploy and,
//! when it runs, recreates it with `docker compose up -d` without pull;
//! then the proxy. Unbinding frees the port and puts the domain back on
//! the 503 page. A failing later step takes the binding back.

use std::sync::Arc;

use super::settings::Settings;
use super::support::{
    apply_override, apply_proxy, authorized_domain, authorized_environment, current_compose,
    ensure_no_deploy_in_flight, project_of,
};
use crate::domain::{
    Binding, BindingId, ContainerPort, EnvKind, EnvName, Environment, Host, Project, ProjectName,
    ServiceName, User,
};
use crate::error::Error;
use crate::ports::{Action, Ports, Rng as _, Store as _};

/// `lof bind <service>:<port> <host>`, validated by the adapter (a
/// function environment takes `lof bind <host>`:
/// [`BindingService::bind_function`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindRequest {
    /// The compose service that listens.
    pub service: ServiceName,
    /// The port it listens on inside its container.
    pub port: ContainerPort,
    /// The domain to route to it.
    pub host: Host,
}

/// The `service` of a function's binding: a function has no compose
/// service; the binding only ties its address to it, and Caddy sends the
/// address to `[functions] bind`, as a tunnel's goes to `[tunnel] bind`.
const FUNCTION_SERVICE: &str = "function";

/// The `container_port` of a function's binding (see [`FUNCTION_SERVICE`]).
const FUNCTION_CONTAINER_PORT: u16 = 80;

/// See the [module documentation](self).
#[derive(Debug)]
pub struct BindingService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for BindingService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> BindingService<P> {
    /// A service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// Binds `request.host` to `request.service:request.port` of the
    /// environment `env` of `project` (BND-01, BND-02).
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the environment (SEC-10);
    /// - [`Error::NotFound`] for an unknown project, environment or domain;
    /// - [`Error::Forbidden`] for a domain of another project (BND-01);
    /// - [`Error::Conflict`] when the domain is already bound (BND-01) or
    ///   a deploy of the environment is in progress;
    /// - [`Error::Unavailable`] when every host port is taken;
    /// - [`Error::Upstream`] when docker or the proxy fail; the binding is
    ///   taken back.
    pub async fn bind(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
        request: BindRequest,
    ) -> Result<Binding, Error> {
        let BindRequest {
            service,
            port,
            host,
        } = request;
        let ports = self.ports.as_ref();
        let store = ports.store();
        let (project, environment) =
            authorized_environment(ports, actor, Action::BindingBind, project, env).await?;
        // A binding is `service:port` of a compose project with a host
        // port; the port-less bindings of the
        // `function` and `tunnel` kinds must not take this path,
        // and they are refused here.
        let EnvKind::Compose = environment.kind else {
            return Err(Error::invalid(
                "service",
                format!(
                    "{} is a {} environment: it has no services or ports to bind \
                     (bind a function environment by its host alone)",
                    environment.name, environment.kind
                ),
            ));
        };
        let domain = store
            .domain_by_host(&host)
            .await?
            .ok_or_else(|| Error::not_found(format!("domain {host}")))?;
        if domain.project_id != project.id {
            return Err(Error::forbidden(format!(
                "{host} belongs to another project"
            )));
        }
        if store.binding_by_domain(domain.id).await?.is_some() {
            return Err(Error::conflict(format!("{host} is already bound")));
        }
        ensure_no_deploy_in_flight(ports, &environment).await?;
        if let Some(compose) = current_compose(ports, &environment).await?
            && !compose.services().contains(&service)
        {
            return Err(Error::invalid(
                "service",
                format!(
                    "{service} is not in the current compose file of {}",
                    environment.name
                ),
            ));
        }
        let host_port = store
            .free_host_port(&self.settings.port_range)
            .await?
            .ok_or_else(|| Error::unavailable("no free host port"))?;
        let binding = Binding {
            id: BindingId::new(ports.rng().ulid()),
            environment_id: environment.id,
            domain_id: domain.id,
            service,
            container_port: port,
            host_port,
        };
        store.insert_binding(&binding).await?;
        let applied = match apply_override(ports, &self.settings, &project, &environment).await {
            Ok(()) => apply_proxy(ports, &self.settings).await,
            Err(error) => Err(error),
        };
        if let Err(error) = applied {
            self.take_back(&project, &environment, &binding).await;
            return Err(error);
        }
        Ok(binding)
    }

    /// Binds the domain `host` to the function environment `env` of
    /// `project` (`lof create` of a function template, `lof bind <host>`):
    /// the domain answers the function once it runs.
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the environment, or for
    ///   a domain of another project;
    /// - [`Error::NotFound`] for an unknown project, environment or domain;
    /// - [`Error::Validation`] on `kind` for an environment that is no
    ///   function;
    /// - [`Error::Conflict`] when the domain is already bound;
    /// - [`Error::Unavailable`] when every host port is taken;
    /// - [`Error::Upstream`] when the proxy fails; the binding is taken
    ///   back.
    pub async fn bind_function(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
        host: &Host,
    ) -> Result<Binding, Error> {
        let ports = self.ports.as_ref();
        let store = ports.store();
        let (project, environment) =
            authorized_environment(ports, actor, Action::BindingBind, project, env).await?;
        if environment.kind != EnvKind::Function {
            return Err(Error::invalid(
                "kind",
                format!(
                    "{} is a {} environment, not a function: bind it to a service and port",
                    environment.name, environment.kind
                ),
            ));
        }
        let domain = store
            .domain_by_host(host)
            .await?
            .ok_or_else(|| Error::not_found(format!("domain {host}")))?;
        if domain.project_id != project.id {
            return Err(Error::forbidden(format!(
                "{host} belongs to another project"
            )));
        }
        if store.binding_by_domain(domain.id).await?.is_some() {
            return Err(Error::conflict(format!("{host} is already bound")));
        }
        // The port is never listened on: it only keeps the row's place
        // among the bindings, whose host ports are unique.
        let host_port = store
            .free_host_port(&self.settings.port_range)
            .await?
            .ok_or_else(|| Error::unavailable("no free host port"))?;
        let binding = Binding {
            id: BindingId::new(ports.rng().ulid()),
            environment_id: environment.id,
            domain_id: domain.id,
            service: ServiceName::new(FUNCTION_SERVICE)?,
            container_port: ContainerPort::new(FUNCTION_CONTAINER_PORT)?,
            host_port,
        };
        store.insert_binding(&binding).await?;
        if let Err(error) = apply_proxy(ports, &self.settings).await {
            if let Err(restore) = store.delete_binding(binding.id).await {
                tracing::error!(error = %restore, %host, "could not take back a function binding");
            }
            return Err(error);
        }
        Ok(binding)
    }

    /// Removes the binding of `host`: the port is freed, the override
    /// regenerated, a running environment brought up again without pull,
    /// and the domain answers the 503 page (BND-03).
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the domain (SEC-10);
    /// - [`Error::NotFound`] for an unknown or unbound domain;
    /// - [`Error::Conflict`] while a deploy of the environment is in
    ///   progress;
    /// - [`Error::Upstream`] when docker or the proxy fail.
    pub async fn unbind(&self, actor: &User, host: &Host) -> Result<(), Error> {
        let ports = self.ports.as_ref();
        let store = ports.store();
        let domain = authorized_domain(ports, actor, Action::BindingUnbind, host).await?;
        let binding = store
            .binding_by_domain(domain.id)
            .await?
            .ok_or_else(|| Error::not_found(format!("binding of {host}")))?;
        let environment = store
            .environment(binding.environment_id)
            .await?
            .ok_or_else(|| {
                Error::internal_msg(format!("binding {} has no environment", binding.id))
            })?;
        let project = project_of(ports, &environment).await?;
        ensure_no_deploy_in_flight(ports, &environment).await?;
        // The routing goes first: if the proxy refuses, nothing else has
        // changed once the row is back.
        store.delete_binding(binding.id).await?;
        if let Err(error) = apply_proxy(ports, &self.settings).await {
            if let Err(restore) = store.insert_binding(&binding).await {
                tracing::error!(error = %restore, %host, "could not put back a binding after a refused proxy configuration");
            }
            return Err(error);
        }
        if let Err(error) = apply_override(ports, &self.settings, &project, &environment).await {
            // Put the binding back with its port in the override and its
            // route, so the files, the proxy and the store agree again.
            if let Err(restore) = store.insert_binding(&binding).await {
                tracing::error!(error = %restore, %host, "could not put back a binding after a failed unbind");
            } else if let Err(restore) =
                apply_override(ports, &self.settings, &project, &environment).await
            {
                tracing::error!(error = %restore, %host, "could not restore the override after a failed unbind");
            } else if let Err(restore) = apply_proxy(ports, &self.settings).await {
                tracing::error!(error = %restore, %host, "could not restore the route after a failed unbind");
            }
            return Err(error);
        }
        Ok(())
    }

    /// Undoes an insert after the override, `up` or the proxy failed: the
    /// row goes, and the override loses the port again.
    async fn take_back(&self, project: &Project, environment: &Environment, binding: &Binding) {
        let ports = self.ports.as_ref();
        if let Err(error) = ports.store().delete_binding(binding.id).await {
            tracing::error!(%error, "could not take back a binding");
            return;
        }
        if let Err(error) = apply_override(ports, &self.settings, project, environment).await {
            tracing::error!(%error, "could not restore the override after a failed bind");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ComposeProject, EnvState, HostPort};
    use crate::error::System;
    use crate::fakes::{Call, DockerCall, World};
    use crate::ports::ProjectFile;
    use crate::proxy_config::{ProxyConfig, Route};

    fn project(raw: &str) -> ProjectName {
        ProjectName::new(raw).unwrap()
    }

    fn env(raw: &str) -> EnvName {
        EnvName::new(raw).unwrap()
    }

    fn host(raw: &str) -> Host {
        Host::new(raw).unwrap()
    }

    fn request(service: &str, port: u16, on: &str) -> BindRequest {
        BindRequest {
            service: ServiceName::new(service).unwrap(),
            port: ContainerPort::new(port).unwrap(),
            host: host(on),
        }
    }

    fn compose(project: &str, env: &str) -> ComposeProject {
        ComposeProject::new(
            &ProjectName::new(project).unwrap(),
            &EnvName::new(env).unwrap(),
        )
    }

    fn route_of(config: Option<ProxyConfig>, raw: &str) -> Option<Route> {
        let wanted = host(raw);
        config?
            .sites
            .into_iter()
            .find(|site| site.host == wanted)
            .map(|site| site.route)
    }

    /// Docker calls as `(command, compose project)`.
    fn docker_calls(world: &World) -> Vec<(&'static str, String)> {
        world
            .log()
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Docker(DockerCall::Up { project, .. }) => {
                    Some(("up", project.as_str().to_owned()))
                }
                Call::Docker(DockerCall::Pull(project)) => {
                    Some(("pull", project.as_str().to_owned()))
                }
                Call::Docker(DockerCall::Down { project, .. }) => {
                    Some(("down", project.as_str().to_owned()))
                }
                _ => None,
            })
            .collect()
    }

    /// `qa` deployed in `state` and an unbound domain `api.apps.test` of
    /// `shop`; the log is cleared.
    async fn arranged(state: EnvState) -> World {
        let world = World::new().await;
        world.deployed(&world.qa, state).await.unwrap();
        world
            .add_domain(&world.shop, "api.apps.test")
            .await
            .unwrap();
        world.log().clear();
        world
    }

    async fn bind_api_to_qa(world: &World) -> Result<Binding, Error> {
        world
            .bindings()
            .bind(
                &world.owner,
                &project("shop"),
                &env("qa"),
                request("web", 8080, "api.apps.test"),
            )
            .await
    }

    mod bind {
        use super::*;

        // TC: REST-05, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world
                .add_user("carol", crate::domain::UserRole::User)
                .await
                .unwrap();
            world.log().clear();
            let error = world
                .bindings()
                .bind(
                    &carol,
                    &project("shop"),
                    &env("qa"),
                    request("web", 3000, "shop.apps.test"),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }

        // TC: VIS-01 (a tunnel has no services or ports: the compose
        // binding refuses it before it takes a host port)
        #[tokio::test]
        async fn invalid_when_environment_is_a_tunnel() {
            let world = arranged(EnvState::Running).await;
            world
                .add_tunnel(&world.shop, "shop-dev", &world.owner)
                .await
                .unwrap();
            let err = world
                .bindings()
                .bind(
                    &world.owner,
                    &project("shop"),
                    &env("shop-dev"),
                    request("web", 8080, "api.apps.test"),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Validation(v) if v.field == "service"),
                "{err:?}"
            );
        }

        // TC: BND-01
        #[tokio::test]
        async fn forbidden_when_domain_of_other_project() {
            let world = arranged(EnvState::Running).await;
            let blog = world.add_project("blog", "owner/blog").await.unwrap();
            world.add_domain(&blog, "blog.apps.test").await.unwrap();
            let err = world
                .bindings()
                .bind(
                    &world.owner,
                    &project("shop"),
                    &env("qa"),
                    request("web", 8080, "blog.apps.test"),
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
        }

        // TC: BND-01
        #[tokio::test]
        async fn conflicts_when_domain_already_bound() {
            let world = arranged(EnvState::Running).await;
            let err = world
                .bindings()
                .bind(
                    &world.owner,
                    &project("shop"),
                    &env("qa"),
                    request("web", 8080, "shop.apps.test"),
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: BND-01 (a refused bind changes nothing)
        #[tokio::test]
        async fn binds_and_runs_nothing_when_refused() {
            let world = arranged(EnvState::Running).await;
            let _refused = world
                .bindings()
                .bind(
                    &world.owner,
                    &project("shop"),
                    &env("qa"),
                    request("web", 8080, "shop.apps.test"),
                )
                .await;
            let bound = world
                .ports
                .store
                .bindings_of_environment(world.qa.id)
                .await
                .unwrap();
            assert_eq!((bound, world.log().external()), (vec![], vec![]));
        }

        // TC: BND-01
        #[tokio::test]
        async fn not_found_when_domain_unknown() {
            let world = arranged(EnvState::Running).await;
            let err = world
                .bindings()
                .bind(
                    &world.owner,
                    &project("shop"),
                    &env("qa"),
                    request("web", 8080, "nope.apps.test"),
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        }

        // TC: BND-01 (the service must be in the current compose file)
        #[tokio::test]
        async fn validation_on_service_when_not_in_compose() {
            let world = arranged(EnvState::Running).await;
            let err = world
                .bindings()
                .bind(
                    &world.owner,
                    &project("shop"),
                    &env("qa"),
                    request("api", 8080, "api.apps.test"),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Validation(v) if v.field == "service")
                    && world.log().external().is_empty(),
                "{err:?}"
            );
        }

        // TC: BND-02 (a range with every port taken)
        #[tokio::test]
        async fn unavailable_without_binding_when_range_exhausted() {
            let world = arranged(EnvState::Running).await;
            let one_port = BindingService::new(
                Arc::clone(&world.ports),
                Arc::new(Settings {
                    port_range: HostPort::range(20_000, 20_000).unwrap(),
                    ..(*world.settings).clone()
                }),
            );
            let result = one_port
                .bind(
                    &world.owner,
                    &project("shop"),
                    &env("qa"),
                    request("web", 8080, "api.apps.test"),
                )
                .await;
            let bound = world
                .ports
                .store
                .bindings_of_environment(world.qa.id)
                .await
                .unwrap();
            assert!(
                matches!(result, Err(Error::Unavailable(_))) && bound.is_empty(),
                "{result:?}, {bound:?}"
            );
        }

        // TC: BND-02 (the world's binding took 20000)
        #[tokio::test]
        async fn takes_next_host_port_when_bound() {
            let world = arranged(EnvState::Running).await;
            bind_api_to_qa(&world).await.unwrap();
            let domain = world
                .ports
                .store
                .domain_by_host(&host("api.apps.test"))
                .await
                .unwrap()
                .unwrap();
            let stored = world
                .ports
                .store
                .binding_by_domain(domain.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    stored.environment_id,
                    stored.service.as_str(),
                    stored.container_port.get(),
                    stored.host_port.get(),
                ),
                (world.qa.id, "web", 8080, 20_001)
            );
        }

        // TC: BND-02 (round the range: a port just freed is not reused)
        #[tokio::test]
        async fn skips_just_freed_port_when_bound_again() {
            let world = arranged(EnvState::Running).await;
            bind_api_to_qa(&world).await.unwrap();
            world
                .bindings()
                .unbind(&world.owner, &host("api.apps.test"))
                .await
                .unwrap();
            let again = bind_api_to_qa(&world).await.unwrap();
            assert_eq!(again.host_port.get(), 20_002);
        }

        // TC: BND-02
        #[tokio::test]
        async fn regenerates_override_with_published_port_when_bound() {
            let world = arranged(EnvState::Running).await;
            bind_api_to_qa(&world).await.unwrap();
            let rendered = world
                .ports
                .files
                .file(&compose("shop", "qa"), ProjectFile::Override)
                .unwrap_or_default();
            assert!(
                rendered.contains("- \"127.0.0.1:20001:8080\""),
                "{rendered}"
            );
        }

        // TC: BND-02
        #[tokio::test]
        async fn ups_without_pull_when_running() {
            let world = arranged(EnvState::Running).await;
            bind_api_to_qa(&world).await.unwrap();
            assert_eq!(docker_calls(&world), vec![("up", "lf_shop_qa".to_owned())]);
        }

        // TC: BND-02 (only a running environment is recreated)
        #[tokio::test]
        async fn leaves_docker_alone_when_stopped() {
            let world = arranged(EnvState::Stopped).await;
            bind_api_to_qa(&world).await.unwrap();
            assert_eq!(docker_calls(&world), vec![]);
        }

        // TC: BND-02 (the proxy routes the domain to the new port)
        #[tokio::test]
        async fn routes_domain_to_host_port_when_running() {
            let world = arranged(EnvState::Running).await;
            bind_api_to_qa(&world).await.unwrap();
            assert_eq!(
                route_of(world.ports.proxy.applied(), "api.apps.test"),
                Some(Route::Upstream(HostPort::new(20_001).unwrap()))
            );
        }

        // TC: BND-02 (a stopped environment stays on the 503 page)
        #[tokio::test]
        async fn routes_domain_to_503_when_stopped() {
            let world = arranged(EnvState::Stopped).await;
            bind_api_to_qa(&world).await.unwrap();
            assert_eq!(
                route_of(world.ports.proxy.applied(), "api.apps.test"),
                Some(Route::Unavailable)
            );
        }

        // TC: BND-02 (compensation: a failed up takes the binding back)
        #[tokio::test]
        async fn takes_binding_back_when_up_fails() {
            let world = arranged(EnvState::Running).await;
            world
                .ports
                .docker
                .fail_next_up(["container web is unhealthy"]);
            let result = bind_api_to_qa(&world).await;
            let bound = world
                .ports
                .store
                .bindings_of_environment(world.qa.id)
                .await
                .unwrap();
            assert!(result.is_err() && bound.is_empty(), "{result:?}, {bound:?}");
        }

        // TC: BND-02 (compensation: a refused config takes it back)
        #[tokio::test]
        async fn takes_binding_back_when_proxy_rejects() {
            let world = arranged(EnvState::Running).await;
            world
                .ports
                .proxy
                .fail_next("apply", Error::upstream(System::Caddy, "bad config"));
            let result = bind_api_to_qa(&world).await;
            let bound = world
                .ports
                .store
                .bindings_of_environment(world.qa.id)
                .await
                .unwrap();
            assert!(result.is_err() && bound.is_empty(), "{result:?}, {bound:?}");
        }
    }

    mod bind_function {
        use super::*;

        /// `api` of `shop`, a function in `state`, and the unbound domain
        /// `api.apps.test`; the log is cleared.
        async fn arranged_function(state: EnvState) -> World {
            let world = World::new().await;
            let function = Environment {
                state,
                ..world.records.function(&world.shop, "api").unwrap()
            };
            world
                .ports
                .store
                .insert_environment(&function)
                .await
                .unwrap();
            world
                .add_domain(&world.shop, "api.apps.test")
                .await
                .unwrap();
            world.log().clear();
            world
        }

        async fn bind_api(world: &World, env_name: &str) -> Result<Binding, Error> {
            world
                .bindings()
                .bind_function(
                    &world.owner,
                    &project("shop"),
                    &env(env_name),
                    &host("api.apps.test"),
                )
                .await
        }

        // TC: CRE-06 (a function's domain answers the function listener)
        #[tokio::test]
        async fn routes_domain_to_function_listener_when_running() {
            let world = arranged_function(EnvState::Running).await;
            bind_api(&world, "api").await.unwrap();
            assert_eq!(
                route_of(world.ports.proxy.applied(), "api.apps.test"),
                Some(Route::Listener(world.settings.proxy.functions_bind))
            );
        }

        // TC: CRE-06 (no compose: binding a function runs no docker)
        #[tokio::test]
        async fn runs_no_docker_when_bound() {
            let world = arranged_function(EnvState::Running).await;
            bind_api(&world, "api").await.unwrap();
            assert_eq!(docker_calls(&world), Vec::new());
        }

        // TC: CRE-06 (only a function takes this path)
        #[tokio::test]
        async fn invalid_on_kind_when_environment_is_compose() {
            let world = arranged_function(EnvState::Running).await;
            let error = bind_api(&world, "qa").await.unwrap_err();
            assert!(
                matches!(&error, Error::Validation(v) if v.field == "kind"),
                "{error:?}"
            );
        }

        // TC: BND-01 (one binding per domain)
        #[tokio::test]
        async fn conflicts_when_domain_already_bound() {
            let world = arranged_function(EnvState::Running).await;
            bind_api(&world, "api").await.unwrap();
            let error = bind_api(&world, "api").await.unwrap_err();
            assert!(matches!(error, Error::Conflict(_)), "{error:?}");
        }

        // TC: BND-02 (compensation: a refused config takes it back)
        #[tokio::test]
        async fn takes_binding_back_when_proxy_rejects() {
            let world = arranged_function(EnvState::Running).await;
            world
                .ports
                .proxy
                .fail_next("apply", Error::upstream(System::Caddy, "bad config"));
            let result = bind_api(&world, "api").await;
            let domain = world
                .ports
                .store
                .domain_by_host(&host("api.apps.test"))
                .await
                .unwrap()
                .unwrap();
            let bound = world
                .ports
                .store
                .binding_by_domain(domain.id)
                .await
                .unwrap();
            assert!(result.is_err() && bound.is_none(), "{result:?}, {bound:?}");
        }
    }

    mod unbind {
        use super::*;

        // TC: REST-05, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world
                .add_user("carol", crate::domain::UserRole::User)
                .await
                .unwrap();
            world.log().clear();
            let error = world
                .bindings()
                .unbind(&carol, &host("shop.apps.test"))
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }

        // TC: BND-03
        #[tokio::test]
        async fn deletes_binding_when_unbound() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .bindings()
                .unbind(&world.owner, &host("shop.apps.test"))
                .await
                .unwrap();
            let bound = world
                .ports
                .store
                .binding_by_domain(world.domain.id)
                .await
                .unwrap();
            assert_eq!(bound, None);
        }

        // TC: BND-03 (the port is free again: a one-port range can be reused)
        #[tokio::test]
        async fn frees_host_port_when_unbound() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .add_domain(&world.shop, "api.apps.test")
                .await
                .unwrap();
            let one_port = BindingService::new(
                Arc::clone(&world.ports),
                Arc::new(Settings {
                    port_range: HostPort::range(20_000, 20_000).unwrap(),
                    ..(*world.settings).clone()
                }),
            );
            one_port
                .unbind(&world.owner, &host("shop.apps.test"))
                .await
                .unwrap();
            let rebound = one_port
                .bind(
                    &world.owner,
                    &project("shop"),
                    &env("prod"),
                    request("web", 3000, "api.apps.test"),
                )
                .await
                .unwrap();
            assert_eq!(rebound.host_port.get(), 20_000);
        }

        // TC: BND-03
        #[tokio::test]
        async fn routes_domain_to_503_when_unbound() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .bindings()
                .unbind(&world.owner, &host("shop.apps.test"))
                .await
                .unwrap();
            assert_eq!(
                route_of(world.ports.proxy.applied(), "shop.apps.test"),
                Some(Route::Unavailable)
            );
        }

        // TC: BND-03 (the override no longer publishes the port)
        #[tokio::test]
        async fn regenerates_override_without_port_when_unbound() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .bindings()
                .unbind(&world.owner, &host("shop.apps.test"))
                .await
                .unwrap();
            let rendered = world
                .ports
                .files
                .file(&compose("shop", "prod"), ProjectFile::Override);
            assert!(
                rendered
                    .as_deref()
                    .is_some_and(|text| !text.contains("127.0.0.1:20000:3000")),
                "{rendered:?}"
            );
        }

        // TC: BND-03 (a running environment is recreated without pull)
        #[tokio::test]
        async fn ups_without_pull_when_running() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world.log().clear();
            world
                .bindings()
                .unbind(&world.owner, &host("shop.apps.test"))
                .await
                .unwrap();
            assert_eq!(
                docker_calls(&world),
                vec![("up", "lf_shop_prod".to_owned())]
            );
        }

        // TC: BND-03 (compensation: a refused config keeps the binding)
        #[tokio::test]
        async fn keeps_binding_without_up_when_proxy_rejects() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world.log().clear();
            world
                .ports
                .proxy
                .fail_next("apply", Error::upstream(System::Caddy, "bad config"));
            let result = world
                .bindings()
                .unbind(&world.owner, &host("shop.apps.test"))
                .await;
            let bound = world
                .ports
                .store
                .binding_by_domain(world.domain.id)
                .await
                .unwrap();
            assert!(
                result.is_err()
                    && bound.as_ref() == Some(&world.binding)
                    && docker_calls(&world).is_empty(),
                "{result:?}, {bound:?}"
            );
        }

        // TC: BND-03 (compensation: a failed up restores the binding)
        #[tokio::test]
        async fn restores_binding_when_up_fails() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .ports
                .docker
                .fail_next_up(["container web is unhealthy"]);
            let result = world
                .bindings()
                .unbind(&world.owner, &host("shop.apps.test"))
                .await;
            let bound = world
                .ports
                .store
                .binding_by_domain(world.domain.id)
                .await
                .unwrap();
            assert!(
                result.is_err() && bound.as_ref() == Some(&world.binding),
                "{result:?}, {bound:?}"
            );
        }

        // TC: BND-03 (compensation: the override publishes the port again)
        #[tokio::test]
        async fn restores_override_port_and_route_when_up_fails() {
            let world = World::new().await;
            world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            world
                .ports
                .docker
                .fail_next_up(["container web is unhealthy"]);
            let _failed = world
                .bindings()
                .unbind(&world.owner, &host("shop.apps.test"))
                .await;
            let rendered = world
                .ports
                .files
                .file(&compose("shop", "prod"), ProjectFile::Override)
                .unwrap_or_default();
            assert!(
                rendered.contains("- \"127.0.0.1:20000:3000\"")
                    && route_of(world.ports.proxy.applied(), "shop.apps.test")
                        == Some(Route::Upstream(world.binding.host_port)),
                "{rendered}"
            );
        }

        // TC: BND-03
        #[tokio::test]
        async fn not_found_when_domain_unbound() {
            let world = World::new().await;
            world
                .add_domain(&world.shop, "api.apps.test")
                .await
                .unwrap();
            let err = world
                .bindings()
                .unbind(&world.owner, &host("api.apps.test"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        }
    }
}
