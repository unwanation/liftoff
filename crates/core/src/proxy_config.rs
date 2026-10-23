//! [`ProxyConfig`]: the whole Caddy configuration, derived from the store.
//!
//! The model says what Caddy serves; `liftoff-proxy`
//! renders it to Caddy's JSON and pushes it with `POST /load`:
//!
//! - one [`Site`] per domain: `reverse_proxy 127.0.0.1:<host_port>` when it
//!   is bound to a running compose environment, the function listener
//!   (`[functions] bind`) or the tunnel listener (`[tunnel] bind`) with
//!   `Host` unchanged when it is bound to a running function or tunnel
//!   environment, the «dev offline» page for an
//!   offline tunnel, liftoff's 503 page otherwise;
//! - the [`ApiSite`]: the host of `[api] public_url` to `[api] bind`;
//! - the gate: a site bound to a running, protected environment (of any
//!   kind that runs: the listeners check access again) asks the
//!   gate listener on `[gate] bind` with `forward_auth` first
//!   ([`Site::gate`]);
//! - the [`Tls`] mode: `acme` (80 and 443, on-demand certificates with
//!   `ask` for external domains) or `edge` (one plain listener on
//!   `127.0.0.1:<edge_port>`);
//! - client-sent `X-Liftoff-*` request headers are stripped on every site
//!   (the renderer's job; gate bullet).

use core::net::{Ipv4Addr, SocketAddr};
use std::collections::HashMap;

use crate::domain::{
    AcmeEmail, Binding, Domain, DomainId, DomainKind, EnvKind, EnvState, Environment,
    EnvironmentId, Host, HostPort,
};
use crate::error::Error;
use crate::ports::Store;
use crate::service::{Settings, TlsMode};

/// The daemon's path Caddy asks before an on-demand certificate, on
/// [`ApiSite::upstream`]: 200 issues, anything else blocks.
pub const TLS_ASK_PATH: &str = "/internal/tls-ask";

/// The complete proxy configuration liftoff pushes in one piece.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProxyConfig {
    /// How Caddy listens and terminates TLS.
    pub tls: Tls,
    /// The API and the daemon's pages.
    pub api: ApiSite,
    /// `[artifacts] host`, when artifacts have a host of their own: served like [`ProxyConfig::api`],
    /// to the same `[api] bind`, whose router answers that host with the
    /// artifacts alone. `None` serves them under `/a/` of the API host.
    pub artifacts: Option<Host>,
    /// One site per domain of the store, by host.
    pub sites: Vec<Site>,
}

/// How Caddy listens (`[proxy] tls`).
///
/// Exhaustive on purpose: a new mode must not compile in the renderer
/// until it is rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tls {
    /// Caddy on `:443` with automatic HTTPS and a redirect from `:80`;
    /// external domains get their certificate on demand once `ask`
    /// answers 200.
    ///
    /// Caddy asks [`TLS_ASK_PATH`] on [`ApiSite::upstream`] (appending
    /// `?domain=<host>`); the renderer derives that URL, so it is not
    /// carried here.
    Acme {
        /// The ACME account (`[proxy] acme_email`).
        email: AcmeEmail,
    },
    /// One server on `listen` (`127.0.0.1:<edge_port>`) without TLS; the
    /// host's proxy terminates it.
    Edge {
        /// Where Caddy listens.
        listen: SocketAddr,
    },
}

/// The site of the API and the daemon's pages (third bullet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiSite {
    /// The host of `[api] public_url`.
    pub host: Host,
    /// `[api] bind`.
    pub upstream: SocketAddr,
}

/// What Caddy serves for one domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// The domain.
    pub host: Host,
    /// Managed (certificate when added) or external (certificate on
    /// demand).
    pub kind: DomainKind,
    /// Where its requests go.
    pub route: Route,
    /// The gate listener (`[gate] bind`) Caddy asks with `forward_auth`
    /// before [`Site::route`] when the domain is bound to a running,
    /// protected environment (gate bullet); `None` for every other site.
    pub gate: Option<SocketAddr>,
}

/// Where the requests of a [`Site`] go (first bullet).
///
/// Exhaustive on purpose, like [`Tls`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Route {
    /// `reverse_proxy 127.0.0.1:<host_port>`: the domain is bound to a
    /// running compose environment.
    Upstream(HostPort),
    /// liftoff's 503 page: no binding, or the environment does not run
    /// (stopped, empty, failed).
    Unavailable,
    /// `reverse_proxy` to one of the daemon's listeners with `Host`
    /// unchanged: `[functions] bind` for a domain bound to a running
    /// function environment, `[tunnel] bind` for one bound to a running
    /// tunnel environment (first bullet).
    Listener(SocketAddr),
    /// liftoff's 503 page «dev offline»: the domain is bound to a tunnel
    /// environment whose client is gone (`offline`).
    Offline,
}

impl ProxyConfig {
    /// The configuration of `domains`, their `bindings` and the
    /// `environments` they are bound to under `settings`:
    ///
    /// - a domain bound to a running compose environment goes to the
    ///   binding's host port, one bound to a running function or tunnel
    ///   environment to its listener, one bound to an offline tunnel to
    ///   the «dev offline» page; every other domain gets the 503 page;
    ///   sites come out sorted by host;
    /// - a domain bound to a running, protected environment asks the gate
    ///   on `settings.proxy.gate_bind` first;
    /// - the API site is `settings.proxy.api_host` to
    ///   `settings.proxy.api_bind`;
    /// - [`TlsMode::Acme`] becomes
    ///   [`Tls::Acme`] with the same account (Caddy asks
    ///   [`TLS_ASK_PATH`] on `api_bind`);
    ///   [`TlsMode::Edge`] becomes
    ///   [`Tls::Edge`] on `127.0.0.1:<port>`.
    #[must_use]
    pub fn from(
        domains: &[Domain],
        bindings: &[Binding],
        environments: &[Environment],
        settings: &Settings,
    ) -> Self {
        let bindings: HashMap<DomainId, &Binding> = bindings
            .iter()
            .map(|binding| (binding.domain_id, binding))
            .collect();
        let environments: HashMap<EnvironmentId, &Environment> = environments
            .iter()
            .map(|environment| (environment.id, environment))
            .collect();
        let mut sites: Vec<Site> = domains
            .iter()
            .map(|domain| {
                let binding = bindings.get(&domain.id);
                let route = binding
                    .and_then(|binding| {
                        let environment = environments.get(&binding.environment_id)?;
                        Some(Self::route_of(environment, binding, settings))
                    })
                    .unwrap_or(Route::Unavailable);
                let gate = binding
                    .filter(|binding| {
                        matches!(route, Route::Upstream(_) | Route::Listener(_))
                            && environments
                                .get(&binding.environment_id)
                                .is_some_and(|environment| environment.protected)
                    })
                    .map(|_| settings.proxy.gate_bind);
                Site {
                    host: domain.host.clone(),
                    kind: domain.kind,
                    route,
                    gate,
                }
            })
            .collect();
        sites.sort_by(|a, b| a.host.as_str().cmp(b.host.as_str()));
        let proxy = &settings.proxy;
        let tls = match &proxy.tls {
            TlsMode::Acme { email } => Tls::Acme {
                email: email.clone(),
            },
            TlsMode::Edge { port } => Tls::Edge {
                listen: SocketAddr::from((Ipv4Addr::LOCALHOST, *port)),
            },
        };
        Self {
            tls,
            api: ApiSite {
                host: proxy.api_host.clone(),
                upstream: proxy.api_bind,
            },
            artifacts: settings.artifacts.host.clone(),
            sites,
        }
    }

    /// Where a domain bound to `environment` by `binding` goes: a
    /// running compose environment to the binding's host port, a running
    /// function to `[functions] bind`, a running tunnel to `[tunnel]
    /// bind` (first bullet), an offline tunnel to the «dev
    /// offline» page; anything else (stopped, empty,
    /// failed) to the 503 page.
    const fn route_of(environment: &Environment, binding: &Binding, settings: &Settings) -> Route {
        match (environment.kind, environment.state) {
            (EnvKind::Compose, EnvState::Running) => Route::Upstream(binding.host_port),
            (EnvKind::Function, EnvState::Running) => {
                Route::Listener(settings.proxy.functions_bind)
            }
            (EnvKind::Tunnel, EnvState::Running) => Route::Listener(settings.proxy.tunnel_bind),
            (EnvKind::Tunnel, EnvState::Offline) => Route::Offline,
            _ => Route::Unavailable,
        }
    }

    /// [`ProxyConfig::from`] over everything `store` holds.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn from_store<S: Store>(store: &S, settings: &Settings) -> Result<Self, Error> {
        let domains = store.domains().await?;
        let bindings = store.bindings().await?;
        let environments = store.environments().await?;
        Ok(Self::from(&domains, &bindings, &environments, settings))
    }
}

#[cfg(test)]
mod tests {
    use core::net::SocketAddr;

    use super::*;
    use crate::domain::{EnvState, Timestamp};
    use crate::fakes::World;
    use crate::service::TlsMode;

    /// The records `ProxyConfig::from` reads.
    struct Inputs {
        domains: Vec<Domain>,
        bindings: Vec<Binding>,
        environments: Vec<Environment>,
    }

    impl Inputs {
        fn config(&self, settings: &Settings) -> ProxyConfig {
            ProxyConfig::from(&self.domains, &self.bindings, &self.environments, settings)
        }
    }

    fn host(raw: &str) -> Host {
        Host::new(raw).unwrap()
    }

    fn addr(raw: &str) -> SocketAddr {
        raw.parse().unwrap()
    }

    fn in_state(environment: &Environment, state: EnvState) -> Environment {
        let mut changed = environment.clone();
        changed.state = state;
        changed
    }

    /// PRX-01: `shop.apps.test` → `web:3000` on running `prod` at 20000,
    /// external `shop.example.org` → `web:3000` on running `qa` at 20001.
    fn bound_and_running(world: &World) -> Inputs {
        let mut external = world
            .records
            .domain(&world.shop, "shop.example.org")
            .unwrap();
        external.kind = DomainKind::External;
        external.verified_at = Some(Timestamp::UNIX_EPOCH);
        let external_binding = world
            .records
            .binding(&world.qa, &external, "web", 3000, 20_001)
            .unwrap();
        Inputs {
            domains: vec![external, world.domain.clone()],
            bindings: vec![world.binding.clone(), external_binding],
            environments: vec![
                in_state(&world.prod, EnvState::Running),
                in_state(&world.qa, EnvState::Running),
            ],
        }
    }

    /// PRX-02: unbound `docs.apps.test`, and `shop.apps.test` bound to
    /// `prod` that is stopped.
    fn unbound_and_stopped(world: &World) -> Inputs {
        let docs = world.records.domain(&world.shop, "docs.apps.test").unwrap();
        Inputs {
            domains: vec![world.domain.clone(), docs],
            bindings: vec![world.binding.clone()],
            environments: vec![
                in_state(&world.prod, EnvState::Stopped),
                in_state(&world.qa, EnvState::Running),
            ],
        }
    }

    /// PRX-07: `api.apps.test` bound to the environment `api` of `shop`
    /// of `kind` in `state` (a function's or a tunnel's address; the
    /// binding's service and ports mean nothing for these kinds).
    fn one_address(world: &World, kind: EnvKind, state: EnvState) -> Inputs {
        let environment = match kind {
            EnvKind::Tunnel => world.records.tunnel(&world.shop, "api", &world.owner),
            _ => world.records.function(&world.shop, "api"),
        }
        .unwrap();
        let domain = world.records.domain(&world.shop, "api.apps.test").unwrap();
        let binding = world
            .records
            .binding(&environment, &domain, "web", 80, 20_100)
            .unwrap();
        Inputs {
            domains: vec![domain],
            bindings: vec![binding],
            environments: vec![in_state(&environment, state)],
        }
    }

    fn no_domains(world: &World) -> Inputs {
        Inputs {
            domains: Vec::new(),
            bindings: Vec::new(),
            environments: vec![world.prod.clone(), world.qa.clone()],
        }
    }

    fn edge(world: &World) -> Settings {
        let mut settings = (*world.settings).clone();
        settings.proxy.tls = TlsMode::Edge { port: 7002 };
        settings
    }

    fn route_of(config: &ProxyConfig, raw: &str) -> Option<Route> {
        let wanted = host(raw);
        config
            .sites
            .iter()
            .find(|site| site.host == wanted)
            .map(|site| site.route)
    }

    mod from {
        use super::*;

        // TC: PRX-01
        #[tokio::test]
        async fn upstream_on_host_port_when_bound_environment_runs() {
            let world = World::new().await;
            let config = bound_and_running(&world).config(&world.settings);
            assert_eq!(
                (
                    route_of(&config, "shop.apps.test"),
                    route_of(&config, "shop.example.org")
                ),
                (
                    Some(Route::Upstream(HostPort::new(20_000).unwrap())),
                    Some(Route::Upstream(HostPort::new(20_001).unwrap()))
                )
            );
        }

        // TC: PRX-01 (external domains are the on-demand ones)
        #[tokio::test]
        async fn keeps_domain_kinds_when_managed_and_external() {
            let world = World::new().await;
            let config = bound_and_running(&world).config(&world.settings);
            let kinds: Vec<(String, DomainKind)> = config
                .sites
                .iter()
                .map(|site| (site.host.to_string(), site.kind))
                .collect();
            assert_eq!(
                kinds,
                vec![
                    ("shop.apps.test".to_owned(), DomainKind::Managed),
                    ("shop.example.org".to_owned(), DomainKind::External),
                ]
            );
        }

        // TC: PRX-01 (the ACME account; the ask URL is the renderer's,
        // checked by the PRX-01 snapshot)
        #[tokio::test]
        async fn acme_keeps_account_email_when_tls_is_acme() {
            let world = World::new().await;
            let config = bound_and_running(&world).config(&world.settings);
            assert_eq!(
                config.tls,
                Tls::Acme {
                    email: AcmeEmail::new("ops@example.com").unwrap(),
                }
            );
        }

        // TC: PRX-02 (domain without a binding)
        #[tokio::test]
        async fn unavailable_when_domain_is_unbound() {
            let world = World::new().await;
            let config = unbound_and_stopped(&world).config(&world.settings);
            assert_eq!(
                route_of(&config, "docs.apps.test"),
                Some(Route::Unavailable)
            );
        }

        // TC: PRX-02 (domain of a stopped environment)
        #[tokio::test]
        async fn unavailable_when_bound_environment_is_stopped() {
            let world = World::new().await;
            let config = unbound_and_stopped(&world).config(&world.settings);
            assert_eq!(
                route_of(&config, "shop.apps.test"),
                Some(Route::Unavailable)
            );
        }

        // TC: PRX-02 (sites by host, whatever the store order)
        #[tokio::test]
        async fn sites_sorted_by_host_when_store_order_differs() {
            let world = World::new().await;
            let config = unbound_and_stopped(&world).config(&world.settings);
            let hosts: Vec<String> = config
                .sites
                .iter()
                .map(|site| site.host.to_string())
                .collect();
            assert_eq!(hosts, vec!["docs.apps.test", "shop.apps.test"]);
        }

        // TC: PRX-03
        #[tokio::test]
        async fn edge_listens_on_loopback_edge_port_when_tls_is_edge() {
            let world = World::new().await;
            let config = bound_and_running(&world).config(&edge(&world));
            assert_eq!(
                config.tls,
                Tls::Edge {
                    listen: addr("127.0.0.1:7002")
                }
            );
        }

        // TC: PRX-04
        #[tokio::test]
        async fn api_site_goes_to_api_bind_when_built() {
            let world = World::new().await;
            let config = bound_and_running(&world).config(&world.settings);
            assert_eq!(
                config.api,
                ApiSite {
                    host: host("liftoff.test"),
                    upstream: addr("127.0.0.1:7000"),
                }
            );
        }

        // TC: TEN-06 (a protected environment's domain asks the gate on
        // `[gate] bind` before its upstream)
        #[tokio::test]
        async fn gate_on_gate_bind_when_bound_environment_is_protected() {
            let world = World::new().await;
            let mut inputs = bound_and_running(&world);
            for environment in &mut inputs.environments {
                if environment.id == world.prod.id {
                    environment.protected = true;
                }
            }
            let config = inputs.config(&world.settings);
            let gates: Vec<(String, Option<SocketAddr>)> = config
                .sites
                .iter()
                .map(|site| (site.host.to_string(), site.gate))
                .collect();
            assert_eq!(
                gates,
                vec![
                    ("shop.apps.test".to_owned(), Some(addr("127.0.0.1:7001"))),
                    ("shop.example.org".to_owned(), None),
                ]
            );
        }

        // TC: TEN-06 (v1 SSO-03: a protected environment that is stopped
        // gets the 503 page without the gate)
        #[tokio::test]
        async fn no_gate_when_protected_environment_is_stopped() {
            let world = World::new().await;
            let mut inputs = unbound_and_stopped(&world);
            for environment in &mut inputs.environments {
                if environment.id == world.prod.id {
                    environment.protected = true;
                }
            }
            let config = inputs.config(&world.settings);
            let shop: Vec<(Route, Option<SocketAddr>)> = config
                .sites
                .into_iter()
                .filter(|site| site.host == host("shop.apps.test"))
                .map(|site| (site.route, site.gate))
                .collect();
            assert_eq!(shop, vec![(Route::Unavailable, None)]);
        }

        // TC: PRX-07 (a function domain goes to `[functions] bind`)
        #[tokio::test]
        async fn function_listener_when_bound_function_environment_runs() {
            let world = World::new().await;
            let config =
                one_address(&world, EnvKind::Function, EnvState::Running).config(&world.settings);
            assert_eq!(
                route_of(&config, "api.apps.test"),
                Some(Route::Listener(addr("127.0.0.1:7003")))
            );
        }

        // TC: PRX-07 (a protected function is gated like a compose environment; gate bullet)
        #[tokio::test]
        async fn gate_on_gate_bind_when_bound_function_environment_is_protected() {
            let world = World::new().await;
            let mut inputs = one_address(&world, EnvKind::Function, EnvState::Running);
            for environment in &mut inputs.environments {
                environment.protected = true;
            }
            let config = inputs.config(&world.settings);
            let gates: Vec<Option<SocketAddr>> =
                config.sites.iter().map(|site| site.gate).collect();
            assert_eq!(gates, vec![Some(addr("127.0.0.1:7001"))]);
        }

        // TC: PRX-07 (a function never deployed gets the 503 page)
        #[tokio::test]
        async fn unavailable_when_bound_function_environment_is_empty() {
            let world = World::new().await;
            let config =
                one_address(&world, EnvKind::Function, EnvState::Empty).config(&world.settings);
            assert_eq!(route_of(&config, "api.apps.test"), Some(Route::Unavailable));
        }

        // TC: PRX-07 (a function whose deploy failed gets the 503 page)
        #[tokio::test]
        async fn unavailable_when_bound_function_environment_failed() {
            let world = World::new().await;
            let config =
                one_address(&world, EnvKind::Function, EnvState::Failed).config(&world.settings);
            assert_eq!(route_of(&config, "api.apps.test"), Some(Route::Unavailable));
        }

        // TC: PRX-07 (a running tunnel domain goes to `[tunnel] bind`)
        #[tokio::test]
        async fn tunnel_listener_when_bound_tunnel_environment_runs() {
            let world = World::new().await;
            let config =
                one_address(&world, EnvKind::Tunnel, EnvState::Running).config(&world.settings);
            assert_eq!(
                route_of(&config, "api.apps.test"),
                Some(Route::Listener(addr("127.0.0.1:7004")))
            );
        }

        // TC: PRX-07 (an offline tunnel gets the «dev offline» page)
        #[tokio::test]
        async fn offline_when_bound_tunnel_environment_is_offline() {
            let world = World::new().await;
            let config =
                one_address(&world, EnvKind::Tunnel, EnvState::Offline).config(&world.settings);
            assert_eq!(route_of(&config, "api.apps.test"), Some(Route::Offline));
        }

        // TC: PRX-04 (the API site exists without any domain)
        #[tokio::test]
        async fn only_api_site_when_store_has_no_domain() {
            let world = World::new().await;
            let config = no_domains(&world).config(&world.settings);
            assert_eq!(
                (config.api.host, config.sites),
                (host("liftoff.test"), Vec::new())
            );
        }
    }

    mod artifacts_host {
        use super::*;

        fn with_artifacts_host(world: &World) -> Settings {
            let mut settings = (*world.settings).clone();
            settings.artifacts.host = Some(host("artifacts.test"));
            settings
        }

        // TC: VIEW-08 (`[artifacts] host` is a site of its own, to the
        // API's upstream)
        #[tokio::test]
        async fn artifacts_site_when_artifacts_host_set() {
            let world = World::new().await;
            let config = no_domains(&world).config(&with_artifacts_host(&world));
            assert_eq!(config.artifacts, Some(host("artifacts.test")));
        }

        // TC: VIEW-08 (no host of their own: artifacts live under `/a/`
        // of the API host)
        #[tokio::test]
        async fn no_artifacts_site_when_artifacts_host_unset() {
            let world = World::new().await;
            let config = no_domains(&world).config(&world.settings);
            assert_eq!(config.artifacts, None);
        }
    }
}
