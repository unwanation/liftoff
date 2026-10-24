//! EXP-01…EXP-07 and DEVT-01…DEVT-08 at the core: the tunnel of `lof
//! expose`, its address, its state over the fake clock, start-up and
//! `rm`.

use std::net::Ipv4Addr;

use time::Duration;

use super::*;
use crate::domain::{DomainKind, EnvKind, EnvState, LinkSecret, LinkTarget, UserRole, Visibility};
use crate::fakes::{FakePorts, HookCall, World};
use crate::proxy_config::Route;
use crate::service::Reconciler;

/// The address of port 3000 exposed on the server.
const DEV_HOST: &str = "3000.localhost.apps.test";

/// `raw` as a [`Host`].
fn host(raw: &str) -> Host {
    Host::new(raw).unwrap()
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

/// `port` exposed from the daemon's own machine (the loopback), not
/// shared.
fn on_server(port: u16) -> Exposure {
    Exposure {
        port: ContainerPort::new(port).unwrap(),
        client: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        share: false,
    }
}

/// `port` exposed from a laptop somewhere else, not shared.
fn elsewhere(port: u16) -> Exposure {
    Exposure {
        client: Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7))),
        ..on_server(port)
    }
}

/// The installation's project of exposed ports, as stored now.
async fn exposures(world: &World) -> Option<Project> {
    world
        .ports
        .store
        .project_by_repo(
            &host(World::BASE),
            &RepoRef::new("liftoff/localhost").unwrap(),
        )
        .await
        .unwrap()
}

/// The tunnel server's number of a tunnel's first connection.
const FIRST: u64 = 0;

/// The number of the connection that replaced or followed [`FIRST`].
const SECOND: u64 = 1;

/// `owner`'s tunnel of port 3000 on the server, exposed and connected
/// through `tunnels` as its [`FIRST`] connection.
async fn connected_tunnel(world: &World, tunnels: &TunnelsService<FakePorts>) -> Environment {
    let opened = tunnels.expose(&world.owner, on_server(3000)).await.unwrap();
    tunnels
        .connected(opened.environment.id, FIRST)
        .await
        .unwrap()
}

/// The fake clock moved forward by `seconds`.
fn advance(world: &World, seconds: i64) {
    world.ports.clock.advance(Duration::seconds(seconds));
}

mod expose {
    use super::*;

    // TC: EXP-03
    #[tokio::test]
    async fn creates_private_tunnel_of_actor_when_first_exposed() {
        let world = World::new().await;
        let opened = world.tunnels().expose(&world.owner, on_server(3000)).await;
        let environment = opened.map(|opened| opened.environment);
        assert_eq!(
            environment.ok().map(|environment| (
                environment.name.to_string(),
                environment.kind,
                environment.owner_user_id,
                environment.visibility,
            )),
            Some((
                "3000".to_owned(),
                EnvKind::Tunnel,
                Some(world.owner.id),
                Visibility::Private
            ))
        );
    }

    // TC: EXP-01
    #[tokio::test]
    async fn port_under_localhost_zone_when_exposed_on_server() {
        let world = World::new().await;
        let opened = world.tunnels().expose(&world.owner, on_server(3000)).await;
        assert_eq!(opened.ok().map(|opened| opened.host), Some(host(DEV_HOST)));
    }

    // TC: EXP-01 (the server's public IP is its own machine too)
    #[tokio::test]
    async fn port_under_localhost_zone_when_client_is_public_ip() {
        let world = World::new().await;
        let exposure = Exposure {
            client: Some(IpAddr::V4(World::PUBLIC_IP)),
            ..on_server(3000)
        };
        let opened = world.tunnels().expose(&world.owner, exposure).await;
        assert_eq!(opened.ok().map(|opened| opened.host), Some(host(DEV_HOST)));
    }

    // TC: EXP-02
    #[tokio::test]
    async fn login_and_id_under_localhost_zone_when_exposed_elsewhere() {
        let world = World::new().await;
        let opened = world.tunnels().expose(&world.owner, elsewhere(3000)).await;
        let name = exposure_name("owner", &world.owner.id.to_string(), 3000, false);
        assert_eq!(
            opened.ok().map(|opened| opened.host),
            Some(host(&format!("{name}.localhost.apps.test")))
        );
    }

    // TC: EXP-02 (a client the adapter cannot place is somewhere else)
    #[tokio::test]
    async fn login_and_id_when_client_unknown() {
        let world = World::new().await;
        let exposure = Exposure {
            client: None,
            ..on_server(3000)
        };
        let opened = world
            .tunnels()
            .expose(&world.owner, exposure)
            .await
            .unwrap();
        assert!(
            opened.environment.name.as_str().starts_with("owner-"),
            "{}",
            opened.environment.name
        );
    }

    // TC: EXP-04 (no project of a user: the installation's own one)
    #[tokio::test]
    async fn tunnel_in_exposures_project_when_first_exposed() {
        let world = World::new().await;
        let opened = world
            .tunnels()
            .expose(&world.owner, on_server(3000))
            .await
            .unwrap();
        let project = exposures(&world).await;
        assert_eq!(
            project.map(|project| (project.id, project.name.to_string())),
            Some((opened.environment.project_id, "localhost".to_owned()))
        );
    }

    // TC: EXP-04
    #[tokio::test]
    async fn one_exposures_project_when_two_ports_exposed() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let first = tunnels.expose(&world.owner, on_server(3000)).await.unwrap();
        let second = tunnels.expose(&world.owner, on_server(5173)).await.unwrap();
        assert_eq!(first.environment.project_id, second.environment.project_id);
    }

    // TC: EXP-04 (`lof projects` never shows it)
    #[tokio::test]
    async fn exposures_project_not_listed_when_tunnel_exposed() {
        let world = World::new().await;
        world
            .tunnels()
            .expose(&world.owner, on_server(3000))
            .await
            .unwrap();
        let names: Vec<String> = world
            .projects()
            .list(&world.owner)
            .await
            .unwrap()
            .into_iter()
            .map(|project| project.name.to_string())
            .collect();
        assert_eq!(names, vec!["shop".to_owned()]);
    }

    // TC: EXP-03
    #[tokio::test]
    async fn binds_managed_domain_to_tunnel_when_first_exposed() {
        let world = World::new().await;
        let opened = world
            .tunnels()
            .expose(&world.owner, on_server(3000))
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
            (domain.kind, binding.map(|b| b.environment_id)),
            (DomainKind::Managed, Some(opened.environment.id))
        );
    }

    // TC: EXP-03
    #[tokio::test]
    async fn asks_domain_created_about_address_when_first_exposed() {
        let world = World::new().await;
        world
            .tunnels()
            .expose(&world.owner, on_server(3000))
            .await
            .unwrap();
        assert_eq!(created_hosts(&world), vec![DEV_HOST.to_owned()]);
    }

    // TC: EXP-05 (without modules only the admin exposes)
    #[tokio::test]
    async fn forbidden_when_actor_is_no_admin_without_modules() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let opened = world.tunnels().expose(&alice, elsewhere(3000)).await;
        assert!(matches!(opened, Err(Error::Forbidden(_))), "{opened:?}");
    }

    // TC: EXP-05 (the hook is asked once, about the installation)
    #[tokio::test]
    async fn asks_authorize_about_installation_when_exposed() {
        let world = World::new().await;
        world
            .tunnels()
            .expose(&world.owner, on_server(3000))
            .await
            .unwrap();
        assert_eq!(
            world.log().authorized(),
            vec![(world.owner.id, Action::TunnelExpose, Resource::Admin)]
        );
    }

    // TC: EXP-06 (a port on the server is its first user's)
    #[tokio::test]
    async fn conflict_when_second_user_exposes_same_port_on_server() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::Admin).await.unwrap();
        let tunnels = world.tunnels();
        tunnels.expose(&world.owner, on_server(3000)).await.unwrap();
        let opened = tunnels.expose(&alice, on_server(3000)).await;
        assert!(matches!(opened, Err(Error::Conflict(_))), "{opened:?}");
    }

    // TC: EXP-06 (elsewhere each user has an address of their own)
    #[tokio::test]
    async fn two_tunnels_when_two_users_expose_same_port_elsewhere() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::Admin).await.unwrap();
        let tunnels = world.tunnels();
        let of_owner = tunnels.expose(&world.owner, elsewhere(3000)).await.unwrap();
        let of_alice = tunnels.expose(&alice, elsewhere(3000)).await.unwrap();
        assert_eq!(
            (
                of_owner.host != of_alice.host,
                of_alice.environment.owner_user_id
            ),
            (true, Some(alice.id))
        );
    }

    // TC: DEVT-04
    #[tokio::test]
    async fn same_environment_when_exposed_again() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let first = tunnels.expose(&world.owner, elsewhere(3000)).await.unwrap();
        let again = tunnels.expose(&world.owner, elsewhere(3000)).await.unwrap();
        assert_eq!(
            (again.environment.id, again.host),
            (first.environment.id, first.host)
        );
    }

    // TC: DEVT-04 (no new domain, and the hook is not asked again)
    #[tokio::test]
    async fn no_new_domain_when_exposed_again() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        tunnels.expose(&world.owner, on_server(3000)).await.unwrap();
        tunnels.expose(&world.owner, on_server(3000)).await.unwrap();
        assert_eq!(created_hosts(&world), vec![DEV_HOST.to_owned()]);
    }

    // TC: DEVT-07
    #[tokio::test]
    async fn login_link_on_api_host_when_tunnel_private() {
        let world = World::new().await;
        let opened = world
            .tunnels()
            .expose(&world.owner, on_server(3000))
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
    async fn login_link_redeems_for_tunnel_when_issued_by_expose() {
        let world = World::new().await;
        let opened = world
            .tunnels()
            .expose(&world.owner, on_server(3000))
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

    // TC: DEVT-07, EXP-07 (`--share`: open to everyone, no login)
    #[tokio::test]
    async fn shared_without_login_link_when_exposed_with_share() {
        let world = World::new().await;
        let exposure = Exposure {
            share: true,
            ..on_server(3000)
        };
        let opened = world
            .tunnels()
            .expose(&world.owner, exposure)
            .await
            .unwrap();
        assert_eq!(
            (opened.environment.visibility, opened.login_url),
            (Visibility::Shared, None)
        );
    }

    // TC: EXP-07 (a run without `--share` closes it again)
    #[tokio::test]
    async fn private_again_when_exposed_without_share() {
        let world = World::new().await;
        let tunnels = world.tunnels();
        let shared = Exposure {
            share: true,
            ..on_server(3000)
        };
        tunnels.expose(&world.owner, shared).await.unwrap();
        let again = tunnels.expose(&world.owner, on_server(3000)).await.unwrap();
        assert_eq!(
            (
                again.environment.visibility,
                stored(&world, again.environment.id).await.visibility
            ),
            (Visibility::Private, Visibility::Private)
        );
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
        let again = tunnels.expose(&world.owner, on_server(3000)).await.unwrap();
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
        let again = tunnels.expose(&world.owner, on_server(3000)).await.unwrap();
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
