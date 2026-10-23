//! DEVT-01…DEVT-08 at the core: the tunnel environment of `lof dev`, its
//! dev address, its state over the fake clock, start-up and `rm`.

use std::collections::BTreeMap;

use time::Duration;
use ulid::Ulid;

use super::*;
use crate::domain::{
    DomainKind, EnvKind, EnvName, EnvState, InstalledModule, LinkSecret, LinkTarget, ModuleName,
    ModuleVersion, TenantId, Timestamp, UserRole, Visibility,
};
use crate::fakes::{FakePorts, HookCall, World};
use crate::proxy_config::Route;
use crate::service::Reconciler;

/// The dev address of `shop` in single-user mode.
const DEV_HOST: &str = "shop-dev.apps.test";

/// `raw` as a [`Host`].
fn host(raw: &str) -> Host {
    Host::new(raw).unwrap()
}

/// `shop`, the project every test opens its tunnel in.
fn shop(world: &World) -> ProjectName {
    world.shop.name.clone()
}

/// `environment` as stored now.
async fn stored(world: &World, environment: EnvironmentId) -> Environment {
    world
        .ports
        .store
        .environment(environment)
        .await
        .unwrap()
        .unwrap()
}

/// The state of `environment` as stored now.
async fn state_of(world: &World, environment: EnvironmentId) -> EnvState {
    stored(world, environment).await.state
}

/// Where the last configuration the proxy was given sends `raw`; `None`
/// when it has no site for it (or nothing was applied).
fn route_of(world: &World, raw: &str) -> Option<Route> {
    let wanted = host(raw);
    world
        .ports
        .proxy
        .applied()?
        .sites
        .into_iter()
        .find(|site| site.host == wanted)
        .map(|site| site.route)
}

/// The hosts every `domain_created` call was about, oldest first.
fn created_hosts(world: &World) -> Vec<String> {
    world
        .ports
        .hooks
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            HookCall::DomainCreated { host, .. } => Some(host.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

/// The tenant `shop` is in once [`with_tenants`] ran.
fn tenant() -> TenantId {
    TenantId::new(Ulid::from_parts(7, 7))
}

/// Makes the world one with the `tenants` module: the module installed
/// and enabled, and `shop` in its tenant (as the module's
/// `project_tenant` would have put it).
async fn with_tenants(world: &World) {
    let store = &world.ports.store;
    store
        .insert_module(&InstalledModule {
            name: ModuleName::new("tenants").unwrap(),
            version: ModuleVersion::new("0.1.0").unwrap(),
            sha256: "ab".repeat(32),
            enabled: true,
            config: BTreeMap::new(),
            error: None,
            installed_at: Timestamp::UNIX_EPOCH,
        })
        .await
        .unwrap();
    let mut project = world.shop.clone();
    project.tenant_id = Some(tenant());
    store.update_project(&project).await.unwrap();
}

/// The tunnel server's number of a tunnel's first connection.
const FIRST: u64 = 0;

/// The number of the connection that replaced or followed [`FIRST`].
const SECOND: u64 = 1;

/// `owner`'s tunnel of `shop`, opened and connected through `tunnels` as
/// its [`FIRST`] connection.
async fn connected_tunnel(world: &World, tunnels: &TunnelsService<FakePorts>) -> Environment {
    let opened = tunnels.open(&world.owner, &shop(world)).await.unwrap();
    tunnels
        .connected(opened.environment.id, FIRST)
        .await
        .unwrap()
}

/// The fake clock moved forward by `seconds`.
fn advance(world: &World, seconds: i64) {
    world.ports.clock.advance(Duration::seconds(seconds));
}

mod open {
    use super::*;

    // TC: DEVT-01
    #[tokio::test]
    async fn creates_private_shop_dev_tunnel_of_actor_when_first_opened() {
        let world = World::new().await;
        let opened = world.tunnels().open(&world.owner, &shop(&world)).await;
        let environment = opened.map(|opened| opened.environment);
        assert_eq!(
            environment.ok().map(|environment| (
                environment.name.to_string(),
                environment.kind,
                environment.owner_user_id,
                environment.visibility,
            )),
            Some((
                "shop-dev".to_owned(),
                EnvKind::Tunnel,
                Some(world.owner.id),
                Visibility::Private
            ))
        );
    }

    // TC: DEVT-01
    #[tokio::test]
    async fn stores_created_tunnel_when_first_opened() {
        let world = World::new().await;
        let opened = world
            .tunnels()
            .open(&world.owner, &shop(&world))
            .await
            .unwrap();
        let found = world
            .ports
            .store
            .environment_by_name(world.shop.id, &EnvName::new("shop-dev").unwrap())
            .await
            .unwrap();
        assert_eq!(found.map(|found| found.id), Some(opened.environment.id));
    }

    // TC: DEVT-01
    #[tokio::test]
    async fn answers_dev_address_under_base_when_first_opened() {
        let world = World::new().await;
        let opened = world.tunnels().open(&world.owner, &shop(&world)).await;
        assert_eq!(opened.ok().map(|opened| opened.host), Some(host(DEV_HOST)));
    }

    // TC: DEVT-01
    #[tokio::test]
    async fn binds_managed_dev_domain_to_tunnel_when_first_opened() {
        let world = World::new().await;
        let opened = world
            .tunnels()
            .open(&world.owner, &shop(&world))
            .await
            .unwrap();
        let store = &world.ports.store;
        let domain = store
            .domain_by_host(&host(DEV_HOST))
            .await
            .unwrap()
            .unwrap();
        let binding = store.binding_by_domain(domain.id).await.unwrap();
        assert_eq!(
            (
                domain.kind,
                domain.project_id,
                binding.map(|b| b.environment_id)
            ),
            (
                DomainKind::Managed,
                world.shop.id,
                Some(opened.environment.id)
            )
        );
    }

    // TC: DEVT-01
    #[tokio::test]
    async fn asks_domain_created_about_dev_address_when_first_opened() {
        let world = World::new().await;
        world
            .tunnels()
            .open(&world.owner, &shop(&world))
            .await
            .unwrap();
        assert_eq!(created_hosts(&world), vec![DEV_HOST.to_owned()]);
    }

    // TC: DEVT-01
    #[tokio::test]
    async fn not_found_when_project_unknown() {
        let world = World::new().await;
        let opened = world
            .tunnels()
            .open(&world.owner, &ProjectName::new("nope").unwrap())
            .await;
        assert!(matches!(opened, Err(Error::NotFound(_))), "{opened:?}");
    }

    // TC: DEVT-02
    #[tokio::test]
    async fn names_tunnel_after_login_when_tenants_installed() {
        let world = World::new().await;
        with_tenants(&world).await;
        let opened = world.tunnels().open(&world.owner, &shop(&world)).await;
        assert_eq!(
            opened
                .ok()
                .map(|opened| (opened.environment.name.to_string(), opened.host)),
            Some(("shop-owner".to_owned(), host("shop-owner.apps.test")))
        );
    }

    // TC: DEVT-02
    #[tokio::test]
    async fn two_tunnels_when_two_users_open_with_tenants() {
        let world = World::new().await;
        with_tenants(&world).await;
        let alice = world.add_user("alice", UserRole::Admin).await.unwrap();
        let tunnels = world.tunnels();
        let of_owner = tunnels.open(&world.owner, &shop(&world)).await.unwrap();
        let of_alice = tunnels.open(&alice, &shop(&world)).await.unwrap();
        assert_eq!(
            (
                of_owner.environment.id != of_alice.environment.id,
                of_owner.environment.owner_user_id,
                of_alice.environment.name.to_string(),
                of_alice.environment.owner_user_id,
                of_alice.host,
            ),
            (
                true,
                Some(world.owner.id),
                "shop-alice".to_owned(),
                Some(alice.id),
                host("shop-alice.apps.test"),
            )
        );
    }

    // TC: DEVT-02 (the dev address is a domain of the project, so the
    // `tenants` module's `tenant_of(host)` finds its tenant)
    #[tokio::test]
    async fn dev_domain_is_in_projects_tenant_when_tenants_installed() {
        let world = World::new().await;
        with_tenants(&world).await;
        world
            .tunnels()
            .open(&world.owner, &shop(&world))
            .await
            .unwrap();
        let domain = world
            .ports
            .store
            .domain_by_host(&host("shop-owner.apps.test"))
            .await
            .unwrap();
        assert_eq!(domain.map(|domain| domain.tenant_id), Some(Some(tenant())));
    }

    // TC: DEVT-02 (a login becomes a label, lowercased, every other character `-`)
    #[tokio::test]
    async fn mangles_login_into_name_when_tenants_installed() {
        let world = World::new().await;
        with_tenants(&world).await;
        let bob = world.add_user("Bob.Smith", UserRole::Admin).await.unwrap();
        let opened = world.tunnels().open(&bob, &shop(&world)).await;
        assert_eq!(
            opened
                .ok()
                .map(|opened| (opened.environment.name.to_string(), opened.host)),
            Some((
                "shop-bob-smith".to_owned(),
                host("shop-bob-smith.apps.test")
            ))
        );
    }

    // TC: DEVT-01 (a compose environment named like the tunnel is no
    // tunnel to reuse)
    #[tokio::test]
    async fn conflict_when_name_taken_by_compose_environment() {
        let world = World::new().await;
        world
            .add_environment(&world.shop, "shop-dev")
            .await
            .unwrap();
        let opened = world.tunnels().open(&world.owner, &shop(&world)).await;
        assert!(matches!(opened, Err(Error::Conflict(_))), "{opened:?}");
    }

    // TC: DEVT-01 (single-user mode has one `<project>-dev`: it is its
    // owner's, a second user may not take it over)
    #[tokio::test]
    async fn conflict_when_second_user_opens_single_user_tunnel() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::Admin).await.unwrap();
        let tunnels = world.tunnels();
        tunnels.open(&world.owner, &shop(&world)).await.unwrap();
        let opened = tunnels.open(&alice, &shop(&world)).await;
        assert!(matches!(opened, Err(Error::Conflict(_))), "{opened:?}");
    }

    // TC: DEVT-04
    #[tokio::test]
    async fn same_environment_when_opened_again() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let first = tunnels.open(&world.owner, &shop(&world)).await.unwrap();
        let again = tunnels.open(&world.owner, &shop(&world)).await.unwrap();
        assert_eq!(
            (again.environment.id, again.host),
            (first.environment.id, first.host)
        );
    }

    // TC: DEVT-04 (no new domain, and the hook is not asked again)
    #[tokio::test]
    async fn no_new_domain_when_opened_again() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        tunnels.open(&world.owner, &shop(&world)).await.unwrap();
        let before = world
            .ports
            .store
            .domains_of_project(world.shop.id)
            .await
            .unwrap()
            .len();
        tunnels.open(&world.owner, &shop(&world)).await.unwrap();
        let after = world
            .ports
            .store
            .domains_of_project(world.shop.id)
            .await
            .unwrap()
            .len();
        assert_eq!(
            (after, created_hosts(&world)),
            (before, vec![DEV_HOST.to_owned()])
        );
    }

    // TC: DEVT-07
    #[tokio::test]
    async fn login_link_on_api_host_when_tunnel_private() {
        let world = World::new().await;
        let opened = world
            .tunnels()
            .open(&world.owner, &shop(&world))
            .await
            .unwrap();
        let url = opened.login_url.unwrap_or_default();
        assert!(
            url.starts_with("https://liftoff.test/api/v1/auth/links/lfl_"),
            "{url}"
        );
    }

    // TC: DEVT-07 (the link is a one-time link of 2.1 to the tunnel)
    #[tokio::test]
    async fn login_link_redeems_for_tunnel_when_issued_by_open() {
        let world = World::new().await;
        let opened = world
            .tunnels()
            .open(&world.owner, &shop(&world))
            .await
            .unwrap();
        let url = opened.login_url.unwrap();
        let token = LinkSecret::parse(url.rsplit('/').next().unwrap()).unwrap();
        let session = world.links().redeem(&token, None).await.unwrap();
        assert_eq!(
            (session.target, session.session.user_id),
            (
                LinkTarget::Environment(opened.environment.id),
                world.owner.id
            )
        );
    }

    // TC: DEVT-07 (a shared address needs no login)
    #[tokio::test]
    async fn no_login_link_when_tunnel_shared() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let first = tunnels.open(&world.owner, &shop(&world)).await.unwrap();
        world
            .visibility()
            .share(&world.owner, &world.shop.name, &first.environment.name)
            .await
            .unwrap();
        let again = tunnels.open(&world.owner, &shop(&world)).await.unwrap();
        assert_eq!(again.login_url, None);
    }
}

mod connected {
    use super::*;

    // TC: DEVT-03
    #[tokio::test]
    async fn running_when_client_connected() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        assert_eq!(
            (tunnel.state, state_of(&world, tunnel.id).await),
            (EnvState::Running, EnvState::Running)
        );
    }

    // TC: DEVT-01 (the route: Caddy sends the dev address to the tunnel
    // listener)
    #[tokio::test]
    async fn routes_dev_address_to_tunnel_listener_when_connected() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        connected_tunnel(&world, &tunnels).await;
        assert_eq!(
            route_of(&world, DEV_HOST),
            Some(Route::Listener(World::TUNNEL_BIND))
        );
    }
}

mod disconnected {
    use super::*;

    // TC: DEVT-03
    #[tokio::test]
    async fn still_running_when_lost_shorter_than_offline_after() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Lost)
            .await
            .unwrap();
        advance(&world, 29);
        let expired = tunnels.expire().await.unwrap();
        assert_eq!(
            (expired, state_of(&world, tunnel.id).await),
            (vec![], EnvState::Running)
        );
    }

    // TC: DEVT-03
    #[tokio::test]
    async fn offline_when_lost_for_offline_after() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Lost)
            .await
            .unwrap();
        advance(&world, 30);
        let expired = tunnels.expire().await.unwrap();
        assert_eq!(
            (expired, state_of(&world, tunnel.id).await),
            (vec![tunnel.id], EnvState::Offline)
        );
    }

    // TC: DEVT-03 (Caddy answers 503 «dev offline»)
    #[tokio::test]
    async fn dev_offline_page_when_lost_tunnel_expired() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Lost)
            .await
            .unwrap();
        advance(&world, 30);
        tunnels.expire().await.unwrap();
        assert_eq!(route_of(&world, DEV_HOST), Some(Route::Offline));
    }

    // TC: DEVT-03 (Ctrl-C takes the route down at once)
    #[tokio::test]
    async fn offline_at_once_when_client_shuts_down() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Shutdown)
            .await
            .unwrap();
        assert_eq!(
            (
                state_of(&world, tunnel.id).await,
                route_of(&world, DEV_HOST)
            ),
            (EnvState::Offline, Some(Route::Offline))
        );
    }

    // TC: DEVT-04 (the client came back within `offline_after`)
    #[tokio::test]
    async fn running_when_client_returns_before_offline_after() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Lost)
            .await
            .unwrap();
        advance(&world, 10);
        tunnels.connected(tunnel.id, SECOND).await.unwrap();
        advance(&world, 30);
        let expired = tunnels.expire().await.unwrap();
        assert_eq!(
            (expired, state_of(&world, tunnel.id).await),
            (vec![], EnvState::Running)
        );
    }

    // TC: DEVT-04
    #[tokio::test]
    async fn running_when_reconnected_after_offline() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Shutdown)
            .await
            .unwrap();
        let again = tunnels.open(&world.owner, &shop(&world)).await.unwrap();
        let reconnected = tunnels
            .connected(again.environment.id, SECOND)
            .await
            .unwrap();
        assert_eq!(
            (
                reconnected.id,
                reconnected.state,
                route_of(&world, DEV_HOST)
            ),
            (
                tunnel.id,
                EnvState::Running,
                Some(Route::Listener(World::TUNNEL_BIND))
            )
        );
    }

    // TC: DEVT-04 (reconnecting after offline makes no new domain)
    #[tokio::test]
    async fn no_new_domain_when_reconnected_after_offline() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Shutdown)
            .await
            .unwrap();
        let again = tunnels.open(&world.owner, &shop(&world)).await.unwrap();
        tunnels
            .connected(again.environment.id, SECOND)
            .await
            .unwrap();
        assert_eq!(created_hosts(&world), vec![DEV_HOST.to_owned()]);
    }

    // TC: DEVT-04 (a connection a newer one replaced does not take the
    // tunnel offline)
    #[tokio::test]
    async fn stays_running_when_connection_evicted() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels.connected(tunnel.id, SECOND).await.unwrap();
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Evicted)
            .await
            .unwrap();
        advance(&world, 60);
        let expired = tunnels.expire().await.unwrap();
        assert_eq!(
            (expired, state_of(&world, tunnel.id).await),
            (vec![], EnvState::Running)
        );
    }
    // TC: DEVT-03 (a replaced connection «состояние не трогает», also when it reports a loss after its successor connected)
    #[tokio::test]
    async fn stays_running_when_replaced_connection_reports_lost() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels.connected(tunnel.id, SECOND).await.unwrap();
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Lost)
            .await
            .unwrap();
        advance(&world, 60);
        let expired = tunnels.expire().await.unwrap();
        assert_eq!(
            (expired, state_of(&world, tunnel.id).await),
            (vec![], EnvState::Running)
        );
    }

    // TC: DEVT-03 (as above, a `Shutdown` of the replaced connection)
    #[tokio::test]
    async fn stays_running_when_replaced_connection_shuts_down() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels.connected(tunnel.id, SECOND).await.unwrap();
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Shutdown)
            .await
            .unwrap();
        assert_eq!(state_of(&world, tunnel.id).await, EnvState::Running);
    }

    // TC: DEVT-04 (the loss of the replaced connection came first: the
    // successor's `connected` still wins)
    #[tokio::test]
    async fn running_when_successor_connects_after_replaced_lost() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let tunnel = connected_tunnel(&world, &tunnels).await;
        tunnels
            .disconnected(tunnel.id, FIRST, TunnelEnd::Lost)
            .await
            .unwrap();
        tunnels.connected(tunnel.id, SECOND).await.unwrap();
        advance(&world, 60);
        let expired = tunnels.expire().await.unwrap();
        assert_eq!(
            (expired, state_of(&world, tunnel.id).await),
            (vec![], EnvState::Running)
        );
    }
}

mod startup {
    use super::*;

    // TC: DEVT-03 (no client is connected to a daemon that just started)
    #[tokio::test]
    async fn offline_when_daemon_starts_with_running_tunnel() {
        let world = World::new().await;
        let mut tunnel = world
            .add_tunnel(&world.shop, "shop-dev", &world.owner)
            .await
            .unwrap();
        tunnel.state = EnvState::Running;
        world.ports.store.update_environment(&tunnel).await.unwrap();
        world
            .add_address(&world.shop, &tunnel, DEV_HOST)
            .await
            .unwrap();
        Reconciler::new(Arc::clone(&world.ports), Arc::clone(&world.settings))
            .startup()
            .await
            .unwrap();
        assert_eq!(
            (
                state_of(&world, tunnel.id).await,
                route_of(&world, DEV_HOST)
            ),
            (EnvState::Offline, Some(Route::Offline))
        );
    }
}

mod remove {
    use super::*;

    /// `owner`'s running tunnel `shop-dev` at [`DEV_HOST`], arranged in
    /// the store.
    async fn running_tunnel(world: &World) -> Environment {
        let mut tunnel = world
            .add_tunnel(&world.shop, "shop-dev", &world.owner)
            .await
            .unwrap();
        tunnel.state = EnvState::Running;
        world.ports.store.update_environment(&tunnel).await.unwrap();
        world
            .add_address(&world.shop, &tunnel, DEV_HOST)
            .await
            .unwrap();
        world.log().clear();
        tunnel
    }

    /// `owner` removes `tunnel` (`lof env rm shop-dev --yes`).
    async fn remove(world: &World, tunnel: &Environment) {
        world
            .environments()
            .remove(&world.owner, &world.shop.name, &tunnel.name, true)
            .await
            .unwrap();
    }

    // TC: DEVT-08
    #[tokio::test]
    async fn dev_domain_gone_when_owner_removes_tunnel() {
        let world = World::new().await;
        let tunnel = running_tunnel(&world).await;
        remove(&world, &tunnel).await;
        let domain = world
            .ports
            .store
            .domain_by_host(&host(DEV_HOST))
            .await
            .unwrap();
        assert_eq!(domain, None);
    }

    // TC: DEVT-08
    #[tokio::test]
    async fn no_site_for_dev_address_when_owner_removes_tunnel() {
        let world = World::new().await;
        let tunnel = running_tunnel(&world).await;
        remove(&world, &tunnel).await;
        let hosts: Vec<Host> = world
            .ports
            .proxy
            .applied()
            .map(|config| config.sites.into_iter().map(|site| site.host).collect())
            .unwrap_or_default();
        assert_eq!(
            (
                hosts.contains(&host(DEV_HOST)),
                hosts.contains(&host("shop.apps.test"))
            ),
            (false, true)
        );
    }

    // TC: DEVT-08 (a module that made the domain's DNS record removes it)
    #[tokio::test]
    async fn asks_domain_removed_about_dev_address_when_owner_removes_tunnel() {
        let world = World::new().await;
        let tunnel = running_tunnel(&world).await;
        remove(&world, &tunnel).await;
        let removed: Vec<String> = world
            .ports
            .hooks
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                HookCall::DomainRemoved { host, .. } => Some(host.as_str().to_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(removed, vec![DEV_HOST.to_owned()]);
    }
}
