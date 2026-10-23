//! `GET /api/v1/tunnel`, the WebSocket upgrade of `lof dev`: `DEVT-01`, `DEVT-06`, `DEVT-07` over
//! the whole API served on an ephemeral `127.0.0.1` port (an upgrade
//! needs a real connection), asked with a hand-written HTTP/1.1 upgrade
//! request, and a hand-written fake of the daemon's tunnel server that
//! records the environments it is handed.

use core::fmt::Write as _;
use core::pin::Pin;
use core::time::Duration;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use http::StatusCode;
use liftoff_core::domain::{Environment, Visibility};
use liftoff_core::fakes::World;
use liftoff_core::ports::Store as _;
use liftoff_rest::{AppState, RestConfig, Services, TunnelHandoff, UpgradedIo, serve_app_on};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};

use crate::support::UNKNOWN_SESSION;

/// The sample nonce of RFC 6455 (4.2.2), whose accept key the RFC gives.
const SAMPLE_NONCE: &str = "dGhlIHNhbXBsZSBub25jZQ=="; // gitleaks:allow

/// How long one exchange may take before the test fails instead of
/// hanging.
const EXCHANGE: Duration = Duration::from_secs(10);

/// The fake tunnel server: records every environment it is handed and
/// holds the connection until the test ends.
#[derive(Debug, Default)]
struct RecordingHandoff {
    handed: Mutex<Vec<Environment>>,
    arrived: Notify,
}

impl RecordingHandoff {
    fn handed(&self) -> Vec<Environment> {
        self.handed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits until an environment was handed over.
    async fn wait_handed(&self) -> Vec<Environment> {
        tokio::time::timeout(EXCHANGE, async {
            loop {
                let notified = self.arrived.notified();
                let handed = self.handed();
                if !handed.is_empty() {
                    return handed;
                }
                notified.await;
            }
        })
        .await
        .expect("an environment is handed to the tunnel server in time")
    }
}

/// The handoff the router owns, sharing the record with the test.
#[derive(Debug, Clone)]
struct Shared(Arc<RecordingHandoff>);

impl TunnelHandoff for Shared {
    fn serve(
        &self,
        environment: Environment,
        io: UpgradedIo,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        let record = Arc::clone(&self.0);
        Box::pin(async move {
            record
                .handed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(environment);
            record.arrived.notify_waiters();
            // Held open until the test drops its end.
            let mut io = io;
            let mut sink = Vec::new();
            let _read = io.read_to_end(&mut sink).await;
        })
    }
}

/// The whole API over a fresh world, served.
struct Served {
    world: World,
    handoff: Arc<RecordingHandoff>,
    addr: SocketAddr,
    _stop: oneshot::Sender<()>,
}

impl Served {
    async fn start() -> Self {
        let world = World::new().await;
        let handoff = Arc::new(RecordingHandoff::default());
        let state = AppState::new().with_tunnels(Shared(Arc::clone(&handoff)));
        let services = Services::new(Arc::clone(&world.ports), Arc::clone(&world.settings));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        tokio::spawn(serve_app_on(
            listener,
            state,
            services,
            RestConfig::default(),
            async {
                let _either: Result<(), oneshot::error::RecvError> = stopped.await;
            },
        ));
        Self {
            world,
            handoff,
            addr,
            _stop: stop,
        }
    }

    /// The upgrade request of `lof dev` with `bearer` and, when given,
    /// `project` in `X-Liftoff-Project`; the head of the answer.
    async fn upgrade(&self, bearer: Option<&str>, project: Option<&str>) -> Head {
        let mut request = format!(
            "GET /api/v1/tunnel HTTP/1.1\r\n\
             Host: liftoff.test\r\n\
             Connection: Upgrade\r\n\
             Upgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: {SAMPLE_NONCE}\r\n",
        );
        if let Some(bearer) = bearer {
            write!(request, "Authorization: Bearer {bearer}\r\n").unwrap();
        }
        if let Some(project) = project {
            write!(request, "X-Liftoff-Project: {project}\r\n").unwrap();
        }
        request.push_str("\r\n");
        tokio::time::timeout(EXCHANGE, head_of(self.addr, request))
            .await
            .expect("the API answers the upgrade in time")
    }

    /// The upgrade of `owner`'s fresh `cli` session for `shop`.
    async fn owner_upgrade(&self) -> Head {
        let secret = self.world.login(&self.world.owner).await.unwrap();
        self.upgrade(Some(secret.expose()), Some("shop")).await
    }
}

/// The status line and headers of an answer (its body is never read:
/// after a `101` the connection is the tunnel's).
#[derive(Debug)]
struct Head {
    status: StatusCode,
    headers: Vec<(String, String)>,
    /// Kept open: dropping it closes the upgraded connection.
    _stream: TcpStream,
}

impl Head {
    /// The value of the header `name` (any case), if any.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Sends `request` to `addr` and reads the head of the answer.
async fn head_of(addr: SocketAddr, request: String) -> Head {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut read = Vec::new();
    let mut byte = [0_u8; 1];
    while !read.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).await.unwrap();
        assert_ne!(n, 0, "the API closed before the end of its head: {read:?}");
        read.push(byte[0]);
    }
    let text = String::from_utf8(read).unwrap();
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Head {
        status,
        headers,
        _stream: stream,
    }
}

mod upgrade {
    use super::*;

    // TC: DEVT-06
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let served = Served::start().await;
        let head = served.upgrade(None, Some("shop")).await;
        assert_eq!(head.status, StatusCode::UNAUTHORIZED);
    }

    // TC: DEVT-06
    #[tokio::test]
    async fn unauthorized_when_session_unknown() {
        let served = Served::start().await;
        let head = served.upgrade(Some(UNKNOWN_SESSION), Some("shop")).await;
        assert_eq!(head.status, StatusCode::UNAUTHORIZED);
    }

    // TC: DEVT-06
    #[tokio::test]
    async fn forbidden_when_bearer_is_deploy_token() {
        let served = Served::start().await;
        let token = served.world.deploy_token(&served.world.shop).await.unwrap();
        let head = served.upgrade(Some(token.expose()), Some("shop")).await;
        assert_eq!(head.status, StatusCode::FORBIDDEN);
    }

    // TC: DEVT-06 (a session that is no `cli` one is refused for good)
    #[tokio::test]
    async fn forbidden_when_bearer_is_browser_session() {
        let served = Served::start().await;
        let browser = served
            .world
            .browser_login(&served.world.owner)
            .await
            .unwrap();
        let head = served.upgrade(Some(browser.expose()), Some("shop")).await;
        assert_eq!(head.status, StatusCode::FORBIDDEN);
    }

    // TC: DEVT-06 (nothing is handed to the tunnel server)
    #[tokio::test]
    async fn hands_nothing_over_when_refused() {
        let served = Served::start().await;
        let token = served.world.deploy_token(&served.world.shop).await.unwrap();
        let with_token = served.upgrade(Some(token.expose()), Some("shop")).await;
        let without = served.upgrade(None, Some("shop")).await;
        assert_eq!(
            (with_token.status, without.status, served.handoff.handed()),
            (StatusCode::FORBIDDEN, StatusCode::UNAUTHORIZED, vec![])
        );
    }

    // TC: DEVT-01 (the project is read at the boundary)
    #[tokio::test]
    async fn bad_request_when_project_header_missing() {
        let served = Served::start().await;
        let secret = served.world.login(&served.world.owner).await.unwrap();
        let head = served.upgrade(Some(secret.expose()), None).await;
        assert_eq!(head.status, StatusCode::BAD_REQUEST);
    }

    // TC: DEVT-01
    #[tokio::test]
    async fn switches_to_websocket_when_cli_session_names_project() {
        let served = Served::start().await;
        let head = served.owner_upgrade().await;
        assert_eq!(
            (
                head.status,
                head.header("upgrade").map(str::to_ascii_lowercase)
            ),
            (
                StatusCode::SWITCHING_PROTOCOLS,
                Some("websocket".to_owned())
            )
        );
    }

    // TC: DEVT-01 (RFC 6455 4.2.2: the accept key of the sample nonce)
    #[tokio::test]
    async fn answers_accept_key_of_nonce_when_upgraded() {
        let served = Served::start().await;
        let head = served.owner_upgrade().await;
        assert_eq!(
            head.header("sec-websocket-accept"),
            Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
    }

    // TC: DEVT-01
    #[tokio::test]
    async fn hands_shop_dev_to_tunnel_server_when_upgraded() {
        let served = Served::start().await;
        let _head = served.owner_upgrade().await;
        let handed = served.handoff.wait_handed().await;
        assert_eq!(
            handed
                .iter()
                .map(|environment| environment.name.to_string())
                .collect::<Vec<_>>(),
            vec!["shop-dev".to_owned()]
        );
    }

    // TC: DEVT-07
    #[tokio::test]
    async fn login_url_header_when_tunnel_private() {
        let served = Served::start().await;
        let head = served.owner_upgrade().await;
        let url = head.header("x-liftoff-login-url").unwrap_or_default();
        assert!(
            url.starts_with("https://liftoff.test/api/v1/auth/links/lfl_"),
            "{:?} {:?}",
            head.status,
            head.headers
        );
    }

    // TC: DEVT-07 (a shared address needs no login)
    #[tokio::test]
    async fn no_login_url_header_when_tunnel_shared() {
        let served = Served::start().await;
        let world = &served.world;
        let tunnel = Environment {
            visibility: Visibility::Shared,
            ..world
                .records
                .tunnel(&world.shop, "shop-dev", &world.owner)
                .unwrap()
        };
        world.ports.store.insert_environment(&tunnel).await.unwrap();
        world
            .add_address(&world.shop, &tunnel, "shop-dev.apps.test")
            .await
            .unwrap();
        let head = served.owner_upgrade().await;
        assert_eq!(
            (head.status, head.header("x-liftoff-login-url")),
            (StatusCode::SWITCHING_PROTOCOLS, None)
        );
    }
}
