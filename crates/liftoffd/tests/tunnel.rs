#![cfg(test)]
//! The dev address of `lof dev` end to end in the daemon: `DEVT-01`, `DEVT-03`, `DEVT-05`,
//! `DEVT-06`, `DEVT-13` and the application WebSocket of `TUN-08` over
//! loopback. The REST API (`GET /api/v1/tunnel`, its
//! connections handed to [`DaemonTunnels`]) and the
//! [`TunnelListener`] serve the world's fakes on ephemeral `127.0.0.1`
//! ports and share one [`TunnelServer`]; the CLI's side is the crate's
//! [`TunnelClient`] bridging to a local HTTP app that records every
//! request it gets; the browser is hyper, asking the listener as Caddy
//! does. Tests wait on events: the client's state and what
//! [`DaemonTunnels::activity`] counts, never on a polling loop.

use core::convert::Infallible;
use core::time::Duration;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use liftoff_core::domain::{EnvName, EnvState, Environment, UserRole, Visibility};
use liftoff_core::fakes::FakePorts;
use liftoff_core::fakes::World;
use liftoff_core::ports::Store as _;
use liftoff_core::service::{Settings, TunnelsService};
use liftoff_rest::{AppState, RestConfig, Services, serve_app_on};
use liftoff_tunnel::{
    Backoff, ClientConfig, ClientState, ServerConfig, TunnelClient, TunnelServer,
};
use liftoffd::listeners::tunnel::TunnelListener;
use liftoffd::tunnels::{DaemonTunnels, TunnelActivity};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// The dev address of `shop` in single-user mode.
const DEV_HOST: &str = "shop-dev.apps.test";

/// What a browser sends.
const BROWSER_ACCEPT: &str = "text/html,application/xhtml+xml,*/*;q=0.8";

/// What the developer's local app answers.
const DEV_APP: &[u8] = b"from the dev app";

/// How long one exchange or state change may take before the test fails
/// instead of hanging.
const EXCHANGE: Duration = Duration::from_secs(10);

/// One request the developer's app got.
type Received = (String, HeaderMap);

/// The developer's local app: answers `200` with [`DEV_APP`] and records
/// the path and headers of every request it gets.
struct DevApp {
    port: u16,
    received: Arc<Mutex<Vec<Received>>>,
}

impl DevApp {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&received);
        tokio::spawn(async move {
            while let Ok((tcp, _peer)) = listener.accept().await {
                let record = Arc::clone(&record);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                        record
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push((request.uri().path().to_owned(), request.headers().clone()));
                        async move {
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                                DEV_APP,
                            ))))
                        }
                    });
                    let _served = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tcp), service)
                        .await;
                });
            }
        });
        Self { port, received }
    }

    /// The paths of the requests the app got so far.
    fn received(&self) -> Vec<String> {
        self.requests().into_iter().map(|(path, _)| path).collect()
    }

    /// The paths and headers of the requests the app got so far.
    fn requests(&self) -> Vec<Received> {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The daemon's REST API and tunnel listener over a fresh world.
struct Daemon {
    world: World,
    tunnels: DaemonTunnels<FakePorts>,
    api: SocketAddr,
    listener: SocketAddr,
    _stop: Vec<oneshot::Sender<()>>,
}

impl Daemon {
    async fn start() -> Self {
        Self::start_with(World::TUNNEL_OFFLINE_AFTER).await
    }

    /// The daemon whose lost tunnels go offline after `offline_after`.
    async fn start_with(offline_after: Duration) -> Self {
        let world = World::new().await;
        let server = TunnelServer::new(ServerConfig::default());
        let settings = Arc::new(Settings {
            tunnel_offline_after: offline_after,
            ..(*world.settings).clone()
        });
        let tunnels = DaemonTunnels::new(
            server.clone(),
            TunnelsService::new(Arc::clone(&world.ports), settings),
        );
        let state = AppState::new().with_tunnels(tunnels.clone());
        let services = Services::new(Arc::clone(&world.ports), Arc::clone(&world.settings))
            .with_tunnels(tunnels.service().clone());

        let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = api_listener.local_addr().unwrap();
        let (stop_api, api_stopped) = oneshot::channel::<()>();
        tokio::spawn(serve_app_on(
            api_listener,
            state,
            services,
            RestConfig::default(),
            async {
                let _either: Result<(), oneshot::error::RecvError> = api_stopped.await;
            },
        ));

        let tunnel_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener = tunnel_listener.local_addr().unwrap();
        let (stop_listener, listener_stopped) = oneshot::channel::<()>();
        tokio::spawn(
            TunnelListener::new(
                Arc::clone(&world.ports),
                Arc::clone(&world.settings),
                server,
            )
            .serve(tunnel_listener, async {
                let _either: Result<(), oneshot::error::RecvError> = listener_stopped.await;
            }),
        );
        Self {
            world,
            tunnels,
            api,
            listener,
            _stop: vec![stop_api, stop_listener],
        }
    }

    /// A client of `shop` presenting `token`, bridging to `local_port`,
    /// with short waits between attempts.
    fn client(&self, token: &str, local_port: u16) -> TunnelClient {
        let mut config = ClientConfig::new(
            format!("ws://{}/api/v1/tunnel", self.api),
            token.to_owned(),
            "shop".to_owned(),
            local_port,
        );
        config.backoff = Backoff {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(200),
        };
        TunnelClient::start(config).unwrap()
    }

    /// `owner`'s `lof dev` of `shop`, connected, bridging to `app`, once
    /// the daemon reported the connection.
    async fn owner_connected(&self, app: &DevApp) -> TunnelClient {
        let secret = self.world.login(&self.world.owner).await.unwrap();
        self.connected(secret.expose(), app).await
    }

    /// The `lof dev` of `shop` presenting `token`, connected, bridging to
    /// `app`, once the daemon reported the connection.
    async fn connected(&self, token: &str, app: &DevApp) -> TunnelClient {
        let before = self.tunnels.activity().borrow().connected;
        let client = self.client(token, app.port);
        wait_state(&client, |state| {
            matches!(state, ClientState::Connected { .. })
        })
        .await;
        self.wait_activity(|activity| activity.connected > before)
            .await;
        client
    }

    /// Waits until what the daemon's tunnels did matches.
    async fn wait_activity(&self, matches: impl Fn(&TunnelActivity) -> bool + Send + Sync) {
        let mut activity = self.tunnels.activity();
        let reached = matches!(
            tokio::time::timeout(EXCHANGE, activity.wait_for(|seen| matches(seen))).await,
            Ok(Ok(_))
        );
        assert!(
            reached,
            "the daemon's tunnels did not get there in time: {:?}",
            *activity.borrow()
        );
    }

    /// `request` to the tunnel listener over a fresh HTTP/1.1
    /// connection, the response with its body read.
    async fn send(&self, request: Request<Full<Bytes>>) -> Response<Bytes> {
        tokio::time::timeout(EXCHANGE, exchange(self.listener, request))
            .await
            .expect("the listener answers in time")
    }

    /// `shop-dev` as stored now.
    async fn dev(&self) -> Option<Environment> {
        self.world
            .ports
            .store
            .environment_by_name(self.world.shop.id, &EnvName::new("shop-dev").unwrap())
            .await
            .unwrap()
    }

    /// The state `shop-dev` is stored in now.
    async fn dev_state(&self) -> Option<EnvState> {
        self.dev().await.map(|dev| dev.state)
    }

    /// The hosts of `shop`'s dev addresses.
    async fn dev_hosts(&self) -> Vec<String> {
        self.world
            .ports
            .store
            .domains_of_project(self.world.shop.id)
            .await
            .unwrap()
            .into_iter()
            .map(|domain| domain.host.as_str().to_owned())
            .filter(|host| host.starts_with("shop-dev"))
            .collect()
    }

    /// Stores `owner`'s tunnel `shop-dev` of `visibility` in `state` at
    /// [`DEV_HOST`], without any client.
    async fn arrange_dev(&self, visibility: Visibility, state: EnvState) -> Environment {
        let world = &self.world;
        let dev = Environment {
            visibility,
            state,
            ..world
                .records
                .tunnel(&world.shop, "shop-dev", &world.owner)
                .unwrap()
        };
        world.ports.store.insert_environment(&dev).await.unwrap();
        world
            .add_address(&world.shop, &dev, DEV_HOST)
            .await
            .unwrap();
        dev
    }
}

/// Waits until the client's state matches.
async fn wait_state(client: &TunnelClient, matches: impl Fn(&ClientState) -> bool + Send + Sync) {
    let mut state = client.state();
    let reached = matches!(
        tokio::time::timeout(EXCHANGE, state.wait_for(|s| matches(s))).await,
        Ok(Ok(_))
    );
    assert!(
        reached,
        "the client did not reach the state in time; it is {:?}",
        *state.borrow()
    );
}

/// One exchange with the listener at `addr`.
async fn exchange(addr: SocketAddr, request: Request<Full<Bytes>>) -> Response<Bytes> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let response = sender.send_request(request).await.unwrap();
    let (head, body) = response.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    Response::from_parts(head, body)
}

/// `GET /page` on `host` with the headers `headers`.
fn get(host: &str, headers: &[(&str, &str)]) -> Request<Full<Bytes>> {
    let mut builder = Request::builder()
        .method("GET")
        .uri("/page")
        .header("host", host);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Full::new(Bytes::new())).unwrap()
}

/// A well-formed session token that was never stored.
const UNKNOWN_SESSION: &str = "lfs_0123456789abcdefghijABCDEFGHIJ0123456789";

mod upgrade {
    use super::*;

    // TC: DEVT-06 (the crate's client takes the 401 as final)
    #[tokio::test]
    async fn client_refused_with_401_when_session_unknown() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let client = daemon.client(UNKNOWN_SESSION, app.port);
        wait_state(&client, |state| {
            *state
                == ClientState::Refused {
                    status: StatusCode::UNAUTHORIZED,
                }
        })
        .await;
    }

    // TC: DEVT-06
    #[tokio::test]
    async fn client_refused_with_403_when_deploy_token() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let token = daemon.world.deploy_token(&daemon.world.shop).await.unwrap();
        let client = daemon.client(token.expose(), app.port);
        wait_state(&client, |state| {
            *state
                == ClientState::Refused {
                    status: StatusCode::FORBIDDEN,
                }
        })
        .await;
    }

    // TC: DEVT-01 (the server's `Hello` names the environment it found)
    #[tokio::test]
    async fn client_connected_to_shop_dev_when_owner_opens() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let secret = daemon.world.login(&daemon.world.owner).await.unwrap();
        let client = daemon.client(secret.expose(), app.port);
        wait_state(&client, |state| {
            matches!(state, ClientState::Connected { environment, .. } if environment == "shop-dev")
        })
        .await;
    }

    // TC: DEVT-07 (the client hands the upgrade's link to `lof dev`)
    #[tokio::test]
    async fn client_holds_login_url_when_tunnel_is_private() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let secret = daemon.world.login(&daemon.world.owner).await.unwrap();
        let client = daemon.client(secret.expose(), app.port);
        wait_state(&client, |state| {
            matches!(
                state,
                ClientState::Connected { login_url: Some(url), .. } if url.contains("/auth/links/")
            )
        })
        .await;
    }

    // TC: DEVT-01, DEVT-03
    #[tokio::test]
    async fn shop_dev_running_when_client_connected() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let _client = daemon.owner_connected(&app).await;
        assert_eq!(daemon.dev_state().await, Some(EnvState::Running));
    }

    // TC: DEVT-03 (Ctrl-C: offline at once, no `offline_after`)
    #[tokio::test]
    async fn shop_dev_offline_when_client_shuts_down() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let client = daemon.owner_connected(&app).await;
        client.shutdown().await;
        daemon.wait_activity(|activity| activity.ended == 1).await;
        assert_eq!(daemon.dev_state().await, Some(EnvState::Offline));
    }
}

mod lost {
    use super::*;

    /// A short `offline_after`, so the test waits on the daemon's timer
    /// instead of 30 seconds; long enough for a test to act between the
    /// loss and the timer.
    const OFFLINE_AFTER: Duration = Duration::from_secs(1);

    /// The fake clock of the core moved by [`OFFLINE_AFTER`], as the
    /// real one would while the daemon's timer runs.
    fn offline_after_passes(daemon: &Daemon) {
        daemon
            .world
            .ports
            .clock
            .advance(time::Duration::try_from(OFFLINE_AFTER).unwrap());
    }

    // TC: DEVT-03 (a lost connection waits for its client: still
    // running before `offline_after`)
    #[tokio::test]
    async fn shop_dev_running_when_client_just_lost() {
        let daemon = Daemon::start_with(OFFLINE_AFTER).await;
        let app = DevApp::start().await;
        let client = daemon.owner_connected(&app).await;
        // Dropped without `Shutdown`: the connection breaks.
        drop(client);
        daemon.wait_activity(|activity| activity.ended == 1).await;
        assert_eq!(daemon.dev_state().await, Some(EnvState::Running));
    }

    // TC: DEVT-03 (offline once `offline_after` passed without the
    // client)
    #[tokio::test]
    async fn shop_dev_offline_when_client_lost_for_offline_after() {
        let daemon = Daemon::start_with(OFFLINE_AFTER).await;
        let app = DevApp::start().await;
        let client = daemon.owner_connected(&app).await;
        drop(client);
        daemon.wait_activity(|activity| activity.ended == 1).await;
        offline_after_passes(&daemon);
        daemon
            .wait_activity(|activity| activity.expiries == 1)
            .await;
        assert_eq!(daemon.dev_state().await, Some(EnvState::Offline));
    }

    // TC: DEVT-04 (the client came back within `offline_after`: the
    // daemon's timer finds it connected, running on the same address)
    #[tokio::test]
    async fn shop_dev_running_on_same_domain_when_lost_client_returns() {
        let daemon = Daemon::start_with(OFFLINE_AFTER).await;
        let app = DevApp::start().await;
        let secret = daemon.world.login(&daemon.world.owner).await.unwrap();
        let first = daemon.connected(secret.expose(), &app).await;
        drop(first);
        daemon.wait_activity(|activity| activity.ended == 1).await;
        let _again = daemon.connected(secret.expose(), &app).await;
        offline_after_passes(&daemon);
        daemon
            .wait_activity(|activity| activity.expiries == 1)
            .await;
        assert_eq!(
            (daemon.dev_state().await, daemon.dev_hosts().await),
            (Some(EnvState::Running), vec![DEV_HOST.to_owned()])
        );
    }

    // TC: DEVT-04 (after Ctrl-C: running again on the same address, no
    // new domain)
    #[tokio::test]
    async fn shop_dev_running_on_same_domain_when_client_reconnects() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let first = daemon.owner_connected(&app).await;
        first.shutdown().await;
        daemon.wait_activity(|activity| activity.ended == 1).await;
        let _again = daemon.owner_connected(&app).await;
        assert_eq!(
            (daemon.dev_state().await, daemon.dev_hosts().await),
            (Some(EnvState::Running), vec![DEV_HOST.to_owned()])
        );
    }
}

mod listener {
    use super::*;

    // TC: DEVT-05 (the owner's `cli` session passes)
    #[tokio::test]
    async fn dev_app_answers_when_owner_bearer() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let _client = daemon.owner_connected(&app).await;
        let secret = daemon.world.login(&daemon.world.owner).await.unwrap();
        let bearer = format!("Bearer {}", secret.expose());
        let response = daemon
            .send(get(DEV_HOST, &[("authorization", &bearer)]))
            .await;
        assert_eq!(
            (response.status(), response.body().as_ref(), app.received()),
            (StatusCode::OK, DEV_APP, vec!["/page".to_owned()])
        );
    }

    // TC: DEVT-05
    #[tokio::test]
    async fn login_redirect_and_no_byte_through_when_private_without_session() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let _client = daemon.owner_connected(&app).await;
        let response = daemon
            .send(get(DEV_HOST, &[("accept", BROWSER_ACCEPT)]))
            .await;
        assert_eq!(
            (response.status(), app.received()),
            (StatusCode::FOUND, Vec::<String>::new())
        );
    }

    // TC: DEVT-05
    #[tokio::test]
    async fn unauthorized_and_no_byte_through_when_json_client_without_session() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let _client = daemon.owner_connected(&app).await;
        let response = daemon
            .send(get(DEV_HOST, &[("accept", "application/json")]))
            .await;
        assert_eq!(
            (response.status(), app.received()),
            (StatusCode::UNAUTHORIZED, Vec::<String>::new())
        );
    }

    // TC: DEVT-05 (a session of someone who may not open it)
    #[tokio::test]
    async fn forbidden_and_no_byte_through_when_session_of_stranger() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let _client = daemon.owner_connected(&app).await;
        let carol = daemon
            .world
            .add_user("carol", UserRole::User)
            .await
            .unwrap();
        let secret = daemon.world.login(&carol).await.unwrap();
        let bearer = format!("Bearer {}", secret.expose());
        let response = daemon
            .send(get(DEV_HOST, &[("authorization", &bearer)]))
            .await;
        assert_eq!(
            (response.status(), app.received()),
            (StatusCode::FORBIDDEN, Vec::<String>::new())
        );
    }

    // TC: DEVT-05 (a shared address needs no session)
    #[tokio::test]
    async fn dev_app_answers_when_shared_without_session() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let _client = daemon.owner_connected(&app).await;
        let world = &daemon.world;
        world
            .visibility()
            .share(
                &world.owner,
                &world.shop.name,
                &EnvName::new("shop-dev").unwrap(),
            )
            .await
            .unwrap();
        let response = daemon.send(get(DEV_HOST, &[])).await;
        assert_eq!(
            (response.status(), response.body().as_ref()),
            (StatusCode::OK, DEV_APP)
        );
    }

    // TC: DEVT-03 (a request without a tunnel is `503` from the server)
    #[tokio::test]
    async fn dev_offline_when_no_client_connected() {
        let daemon = Daemon::start().await;
        daemon
            .arrange_dev(Visibility::Shared, EnvState::Running)
            .await;
        let response = daemon.send(get(DEV_HOST, &[])).await;
        let body = String::from_utf8_lossy(response.body()).into_owned();
        assert_eq!(
            (response.status(), body.contains("dev offline")),
            (StatusCode::SERVICE_UNAVAILABLE, true),
            "{body}"
        );
    }

    // TC: DEVT-03 (an offline tunnel is answered by the listener
    // itself, before any access check)
    #[tokio::test]
    async fn dev_offline_when_tunnel_offline() {
        let daemon = Daemon::start().await;
        daemon
            .arrange_dev(Visibility::Private, EnvState::Offline)
            .await;
        let response = daemon
            .send(get(DEV_HOST, &[("accept", BROWSER_ACCEPT)]))
            .await;
        let body = String::from_utf8_lossy(response.body()).into_owned();
        assert_eq!(
            (response.status(), body.contains("dev offline")),
            (StatusCode::SERVICE_UNAVAILABLE, true),
            "{body}"
        );
    }

    // TC: DEVT-05 (a host that is no tunnel's address)
    #[tokio::test]
    async fn not_found_when_host_unknown() {
        let daemon = Daemon::start().await;
        let response = daemon.send(get("nobody.apps.test", &[])).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // TC: DEVT-05 (a compose environment's address is not the
    // listener's)
    #[tokio::test]
    async fn not_found_when_host_is_compose_address() {
        let daemon = Daemon::start().await;
        let response = daemon.send(get("shop.apps.test", &[])).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

mod headers {
    use super::*;

    /// The headers the app got for one request of the owner through the
    /// listener, carrying `forged` besides the owner's bearer.
    async fn seen_by_app(forged: &[(&str, &str)]) -> (Daemon, HeaderMap) {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let _client = daemon.owner_connected(&app).await;
        let secret = daemon.world.login(&daemon.world.owner).await.unwrap();
        let bearer = format!("Bearer {}", secret.expose());
        let mut sent = vec![("authorization", bearer.as_str())];
        sent.extend_from_slice(forged);
        let response = daemon.send(get(DEV_HOST, &sent)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut requests = app.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let (_path, headers) = requests.remove(0);
        (daemon, headers)
    }

    /// Every value of `name` in `headers`.
    fn values(headers: &HeaderMap, name: &str) -> Vec<String> {
        headers
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect()
    }

    // TC: DEVT-13 (the browser's forged forwarding headers do not reach
    // the app: it sees the listener's)
    #[tokio::test]
    async fn listeners_forwarding_headers_when_browser_forged_them() {
        let (_daemon, headers) = seen_by_app(&[
            ("x-forwarded-for", "10.9.9.9, 198.51.100.4"),
            ("x-forwarded-proto", "gopher"),
            ("x-forwarded-host", "evil.example"),
            ("forwarded", "for=10.9.9.9"),
        ])
        .await;
        assert_eq!(
            (
                values(&headers, "x-forwarded-for"),
                values(&headers, "x-forwarded-proto"),
                values(&headers, "x-forwarded-host"),
                values(&headers, "forwarded"),
            ),
            (
                vec!["198.51.100.4".to_owned()],
                vec!["https".to_owned()],
                vec![DEV_HOST.to_owned()],
                vec![],
            )
        );
    }

    // TC: DEVT-13 (the app learns who was let through from the
    // listener, never from the browser)
    #[tokio::test]
    async fn owner_in_liftoff_user_when_browser_forged_it() {
        let (daemon, headers) = seen_by_app(&[
            ("x-liftoff-user", "mallory"),
            ("x-liftoff-email", "mallory@evil.example"),
        ])
        .await;
        let owner = &daemon.world.owner;
        let email = if owner.email.is_empty() {
            vec![]
        } else {
            vec![owner.email.clone()]
        };
        assert_eq!(
            (
                values(&headers, "x-liftoff-user"),
                values(&headers, "x-liftoff-email")
            ),
            (vec![owner.login.as_str().to_owned()], email)
        );
    }

    // TC: DEVT-13 (liftoff's bearer and cookies stay with liftoff)
    #[tokio::test]
    async fn no_liftoff_credentials_when_request_carried_them() {
        let (_daemon, headers) = seen_by_app(&[("cookie", "__Host-liftoff_gate=x; app=1")]).await;
        assert_eq!(
            (
                values(&headers, "authorization"),
                values(&headers, "cookie")
            ),
            (vec![], vec!["app=1".to_owned()])
        );
    }
}

mod websocket {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    use super::*;

    /// The developer's local WebSocket app: echoes every message.
    async fn echo_app() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((tcp, _peer)) = listener.accept().await {
                tokio::spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(tcp).await else {
                        return;
                    };
                    while let Some(Ok(message)) = socket.next().await {
                        if (message.is_text() || message.is_binary())
                            && socket.send(message).await.is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    /// The owner's `lof dev` of `shop` bridging to the echo app.
    async fn owner_bridging_to_echo(daemon: &Daemon) -> TunnelClient {
        let echo = echo_app().await;
        let secret = daemon.world.login(&daemon.world.owner).await.unwrap();
        let before = daemon.tunnels.activity().borrow().connected;
        let client = daemon.client(secret.expose(), echo);
        wait_state(&client, |state| {
            matches!(state, ClientState::Connected { .. })
        })
        .await;
        daemon
            .wait_activity(|activity| activity.connected > before)
            .await;
        client
    }

    /// A browser's WebSocket to `ws://shop-dev.apps.test/socket` through
    /// the daemon's tunnel listener with `headers`: what came back for
    /// one text and one binary message, after the `101`.
    async fn echoed(daemon: &Daemon, headers: &[(&'static str, String)]) -> Vec<Message> {
        let mut request = format!("ws://{DEV_HOST}/socket")
            .into_client_request()
            .unwrap();
        for (name, value) in headers {
            request.headers_mut().insert(*name, value.parse().unwrap());
        }
        let tcp = TcpStream::connect(daemon.listener).await.unwrap();
        let (mut socket, response) =
            tokio::time::timeout(EXCHANGE, tokio_tungstenite::client_async(request, tcp))
                .await
                .expect("the upgrade ends in time")
                .expect("the browser's WebSocket opens through the listener");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        let sent = [
            Message::text("ping through the daemon"),
            Message::binary(vec![0_u8, 1, 2, 255]),
        ];
        let mut back = Vec::new();
        for message in sent {
            socket.send(message).await.expect("the browser sends");
            let answer = tokio::time::timeout(EXCHANGE, socket.next())
                .await
                .expect("the echo comes back in time");
            back.extend(answer.and_then(Result::ok));
        }
        back
    }

    // TC: TUN-08, DEVT-05 (`Connection` and `Upgrade` pass the listener; a shared address needs no session)
    #[tokio::test]
    async fn echoes_app_websocket_when_shared_address_upgrades() {
        let daemon = Daemon::start().await;
        let _client = owner_bridging_to_echo(&daemon).await;
        let world = &daemon.world;
        world
            .visibility()
            .share(
                &world.owner,
                &world.shop.name,
                &EnvName::new("shop-dev").unwrap(),
            )
            .await
            .unwrap();

        let back = echoed(&daemon, &[]).await;

        assert_eq!(
            back,
            [
                Message::text("ping through the daemon"),
                Message::binary(vec![0_u8, 1, 2, 255]),
            ]
        );
    }

    // TC: TUN-08, DEVT-05 (the owner's session opens the private address's
    // WebSocket too)
    #[tokio::test]
    async fn echoes_app_websocket_when_owner_bearer_upgrades_private_address() {
        let daemon = Daemon::start().await;
        let _client = owner_bridging_to_echo(&daemon).await;
        let secret = daemon.world.login(&daemon.world.owner).await.unwrap();

        let back = echoed(
            &daemon,
            &[("authorization", format!("Bearer {}", secret.expose()))],
        )
        .await;

        assert_eq!(
            back,
            [
                Message::text("ping through the daemon"),
                Message::binary(vec![0_u8, 1, 2, 255]),
            ]
        );
    }
}

mod handover {
    use liftoff_core::domain::LinkTarget;

    use super::*;

    /// The values of the header `name` of `response`.
    fn header_values(response: &Response<Bytes>, name: &str) -> Vec<String> {
        response
            .headers()
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok().map(ToOwned::to_owned))
            .collect()
    }

    /// The `Location` of `response`; empty without one.
    fn location(response: &Response<Bytes>) -> String {
        header_values(response, "location")
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    /// `GET path` on `host` with `headers`; an empty `path` (no
    /// `Location` to follow) asks for `/`.
    fn get_path(host: &str, path: &str, headers: &[(&str, &str)]) -> Request<Full<Bytes>> {
        let mut builder = Request::builder()
            .method("GET")
            .uri(if path.is_empty() { "/" } else { path })
            .header("host", host);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Full::new(Bytes::new())).unwrap()
    }

    // TC: OTL-07 (the link of `lof dev` hands the session over to the dev
    // address: the tunnel listener redeems the code for the host's own
    // session `__Host-liftoff_gate`, and nothing reaches the dev app)
    #[tokio::test]
    async fn sets_host_cookie_when_callback_has_code_of_dev_link() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let dev = daemon
            .arrange_dev(Visibility::Private, EnvState::Running)
            .await;
        let url = daemon
            .world
            .links()
            .issue(&daemon.world.owner, &LinkTarget::Environment(dev.id))
            .await
            .unwrap()
            .url;
        let path = url
            .strip_prefix("https://liftoff.test")
            .unwrap_or(&url)
            .to_owned();
        let opened = tokio::time::timeout(
            EXCHANGE,
            exchange(
                daemon.api,
                get_path("liftoff.test", &path, &[("accept", BROWSER_ACCEPT)]),
            ),
        )
        .await
        .expect("the API answers in time");
        let callback = location(&opened);
        let callback = callback
            .strip_prefix(&format!("https://{DEV_HOST}"))
            .unwrap_or(&callback)
            .to_owned();
        let entered = daemon
            .send(get_path(DEV_HOST, &callback, &[("accept", BROWSER_ACCEPT)]))
            .await;
        let set_cookie = header_values(&entered, "set-cookie");
        assert_eq!(
            (
                entered.status(),
                set_cookie
                    .iter()
                    .any(|line| line.starts_with("__Host-liftoff_gate=lfg_")),
                app.received()
            ),
            (StatusCode::FOUND, true, Vec::<String>::new()),
            "{url} -> {callback} -> {set_cookie:?}"
        );
    }

    /// Whether `response` is a page of the listener's: HTML with the v1
    /// headers (`nosniff`, `no-store`, the pages' CSP) and no script.
    fn is_page(response: &Response<Bytes>) -> bool {
        let body = String::from_utf8_lossy(response.body()).to_ascii_lowercase();
        header_values(response, "content-type")
            .first()
            .is_some_and(|value| value.starts_with("text/html"))
            && header_values(response, "x-content-type-options") == ["nosniff"]
            && header_values(response, "cache-control") == ["no-store"]
            && header_values(response, "content-security-policy") == [liftoff_rest::pages::CSP]
            && !body.contains("<script")
    }

    // TC: OTL-08 (the offline page keeps the v1 headers)
    #[tokio::test]
    async fn offline_page_has_v1_headers_when_tunnel_offline() {
        let daemon = Daemon::start().await;
        daemon
            .arrange_dev(Visibility::Private, EnvState::Offline)
            .await;
        let response = daemon
            .send(get(DEV_HOST, &[("accept", BROWSER_ACCEPT)]))
            .await;
        assert_eq!(
            (response.status(), is_page(&response)),
            (StatusCode::SERVICE_UNAVAILABLE, true),
            "{:?}",
            response.headers()
        );
    }

    // TC: OTL-08 (the offline page also while the client is gone and the environment still waits for it, `offline_after`)
    #[tokio::test]
    async fn offline_page_when_running_without_client() {
        let daemon = Daemon::start().await;
        daemon
            .arrange_dev(Visibility::Shared, EnvState::Running)
            .await;
        let response = daemon
            .send(get(DEV_HOST, &[("accept", BROWSER_ACCEPT)]))
            .await;
        let body = String::from_utf8_lossy(response.body()).into_owned();
        assert_eq!(
            (
                response.status(),
                is_page(&response),
                body.contains("dev offline")
            ),
            (StatusCode::SERVICE_UNAVAILABLE, true, true),
            "{body}"
        );
    }

    // TC: OTL-08 (a browser refused on a dev address gets the forbidden page)
    #[tokio::test]
    async fn forbidden_page_when_strangers_session_on_dev_address() {
        let daemon = Daemon::start().await;
        let app = DevApp::start().await;
        let _client = daemon.owner_connected(&app).await;
        let carol = daemon
            .world
            .add_user("carol", UserRole::User)
            .await
            .unwrap();
        let secret = daemon.world.host_login(&carol, DEV_HOST).await.unwrap();
        let cookie = format!("__Host-liftoff_gate={}", secret.expose());
        let response = daemon
            .send(get(
                DEV_HOST,
                &[("accept", BROWSER_ACCEPT), ("cookie", &cookie)],
            ))
            .await;
        assert_eq!(
            (response.status(), is_page(&response), app.received()),
            (StatusCode::FORBIDDEN, true, Vec::<String>::new()),
            "{}",
            String::from_utf8_lossy(response.body())
        );
    }

    // TC: OTL-08 (the offline answer of the listener is the offline page for a browser, without script)
    #[tokio::test]
    async fn offline_page_when_tunnel_offline() {
        let daemon = Daemon::start().await;
        daemon
            .arrange_dev(Visibility::Private, EnvState::Offline)
            .await;
        let response = daemon
            .send(get(DEV_HOST, &[("accept", BROWSER_ACCEPT)]))
            .await;
        let body = String::from_utf8_lossy(response.body()).into_owned();
        assert_eq!(
            (
                response.status(),
                header_values(&response, "content-type")
                    .first()
                    .is_some_and(|value| value.starts_with("text/html")),
                body.contains("dev offline"),
                body.to_ascii_lowercase().contains("<script"),
            ),
            (StatusCode::SERVICE_UNAVAILABLE, true, true, false),
            "{body}"
        );
    }
}
