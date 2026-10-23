#![cfg(test)]
//! The tunnel end to end:
//! `TUN-01…09` with the client against the server in process, the server
//! behind a real WebSocket upgrade (axum on an ephemeral `127.0.0.1`
//! port) and the client connecting its streams to a local app.

mod support;

use core::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use http::{Request, StatusCode};
use http_body_util::Full;
use liftoff_tunnel::{
    Backoff, Bandwidth, ClientConfig, ClientState, Disconnect, Frame, PROTOCOL_VERSION,
    ServerConfig, TunnelClient, TunnelError,
};
use support::{
    Daemon, ENVIRONMENT, EXCHANGE, LocalApp, PROJECT, TOKEN, bytes_body, closed_port,
    hello_message, raw_client, send, start, test_ca, tls_front, wait_connected, wait_ended,
    wait_state, websocket_echo,
};
use tokio_tungstenite::tungstenite::Message;

/// The keepalive of tests that do not look at it.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// A server that never drops a connection for being idle during a test.
fn patient() -> ServerConfig {
    ServerConfig {
        max_streams: 64,
        idle_timeout: Duration::from_secs(3600),
        bandwidth: None,
    }
}

/// A daemon, a local app named `app` and a connected client of it.
async fn connected(config: ServerConfig) -> (Daemon, LocalApp, TunnelClient) {
    let daemon = Daemon::start(config).await;
    let app = LocalApp::start("app").await;
    let client = start(daemon.client_config(app.port, KEEPALIVE));
    wait_connected(&client).await;
    daemon.wait_tunnel().await;
    (daemon, app, client)
}

mod handshake {
    use super::*;

    // TC: TUN-01 (the upgrade the daemon routes names the project, not
    // the environment)
    #[tokio::test]
    async fn sends_the_session_and_the_project_when_it_connects() {
        let (daemon, _app, _client) = connected(patient()).await;

        let headers = daemon.upgrade_headers();

        let first = headers.first().expect("one upgrade request");
        assert_eq!(
            first
                .get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert_eq!(
            first.get("x-liftoff-project").and_then(|v| v.to_str().ok()),
            Some(PROJECT)
        );
    }

    // TC: TUN-01
    #[tokio::test]
    async fn names_the_environment_of_the_server_hello_when_connected() {
        let (_daemon, _app, client) = connected(patient()).await;

        let state = client.state().borrow().clone();

        assert_eq!(
            state,
            ClientState::Connected {
                environment: ENVIRONMENT.to_owned(),
                login_url: None,
            }
        );
    }
}

mod hello {
    use super::*;

    /// Answers the server's `Hello` with `answer` from a WebSocket of the
    /// test's own and says how the daemon ended that connection.
    async fn answered(answer: Frame) -> (Daemon, Result<Disconnect, TunnelError>) {
        let mut daemon = Daemon::start(patient()).await;
        let (mut socket, _hello) = raw_client(&daemon).await;

        socket
            .send(hello_message(&answer))
            .await
            .expect("the answer goes out");
        let ended = daemon.next_end().await;
        (daemon, ended)
    }

    // TC: TUN-01
    #[tokio::test]
    async fn names_the_environment_and_its_version_when_the_server_says_hello() {
        let daemon = Daemon::start(patient()).await;

        let (_socket, hello) = raw_client(&daemon).await;

        assert_eq!(
            hello,
            Frame::Hello {
                environment: ENVIRONMENT.to_owned(),
                version: PROTOCOL_VERSION,
            }
        );
    }

    // TC: TUN-01
    #[tokio::test]
    async fn refuses_the_connection_when_the_client_speaks_another_version() {
        let (daemon, ended) = answered(Frame::Hello {
            environment: ENVIRONMENT.to_owned(),
            version: PROTOCOL_VERSION + 1,
        })
        .await;

        assert!(matches!(ended, Err(TunnelError::Protocol(_))), "{ended:?}");
        assert!(!daemon.server.is_connected(ENVIRONMENT));
    }

    // TC: TUN-01
    #[tokio::test]
    async fn refuses_the_connection_when_the_client_answers_for_another_environment() {
        let (daemon, ended) = answered(Frame::Hello {
            environment: "other-dev".to_owned(),
            version: PROTOCOL_VERSION,
        })
        .await;

        assert!(matches!(ended, Err(TunnelError::Protocol(_))), "{ended:?}");
        assert!(!daemon.server.is_connected(ENVIRONMENT));
    }

    // TC: TUN-01
    #[tokio::test]
    async fn refuses_the_connection_when_the_client_answers_with_another_frame() {
        let (daemon, ended) = answered(Frame::Shutdown).await;

        assert!(matches!(ended, Err(TunnelError::Protocol(_))), "{ended:?}");
        assert!(!daemon.server.is_connected(ENVIRONMENT));
    }
}

mod config {
    use super::*;

    fn config(url: &str, token: &str, project: &str) -> ClientConfig {
        ClientConfig::new(url.to_owned(), token.to_owned(), project.to_owned(), 5173)
    }

    // TC: TUN-01 (validated at the boundary)
    #[tokio::test]
    async fn refuses_to_start_when_the_url_is_no_websocket_url() {
        for url in [
            "https://liftoff.example.com/api/v1/tunnel",
            "not a url",
            "ws:///api/v1/tunnel",
        ] {
            let started = TunnelClient::start(config(url, TOKEN, PROJECT));

            assert!(
                matches!(started, Err(TunnelError::Invalid(_))),
                "{url}: {started:?}"
            );
        }
    }

    // TC: TUN-01 (validated at the boundary)
    #[tokio::test]
    async fn refuses_to_start_when_the_token_is_no_header_value() {
        let started =
            TunnelClient::start(config("ws://127.0.0.1:1/api/v1/tunnel", "a\nb", PROJECT));

        assert!(
            matches!(started, Err(TunnelError::Invalid(_))),
            "{started:?}"
        );
    }

    // TC: TUN-01 (validated at the boundary)
    #[tokio::test]
    async fn refuses_to_start_when_the_project_is_no_header_value() {
        let started =
            TunnelClient::start(config("ws://127.0.0.1:1/api/v1/tunnel", TOKEN, "shop\r"));

        assert!(
            matches!(started, Err(TunnelError::Invalid(_))),
            "{started:?}"
        );
    }

    // TC: TUN-01 (the session is a secret)
    #[test]
    fn hides_the_session_when_the_configuration_is_printed() {
        let printed = format!(
            "{:?}",
            config("wss://liftoff.example.com/api/v1/tunnel", TOKEN, PROJECT)
        );

        assert!(!printed.contains(TOKEN), "{printed}");
        assert!(printed.contains(PROJECT), "{printed}");
    }
}

mod tls {
    use super::*;

    // TC: TUN-01 (`wss://`, the scheme `lof dev` uses against a daemon)
    #[tokio::test]
    async fn serves_requests_when_it_connects_over_tls() {
        let daemon = Daemon::start(patient()).await;
        let (front, _failed) = tls_front(daemon.addr).await;
        let app = LocalApp::start("app").await;
        let mut config = daemon.client_config(app.port, KEEPALIVE);
        config.url = format!("wss://{front}/api/v1/tunnel");
        config.extra_roots = vec![test_ca()];
        let client = start(config);
        wait_connected(&client).await;
        daemon.wait_tunnel().await;

        let (status, _headers, body) = daemon.get("/").await;

        assert_eq!((status, body), (StatusCode::OK, Bytes::from_static(b"app")));
    }

    // TC: TUN-01 (`wss://` checks the daemon's certificate)
    #[tokio::test]
    async fn does_not_connect_when_the_certificate_is_not_trusted() {
        let daemon = Daemon::start(patient()).await;
        let (front, mut failed) = tls_front(daemon.addr).await;
        let app = LocalApp::start("app").await;
        let mut config = daemon.client_config(app.port, KEEPALIVE);
        config.url = format!("wss://{front}/api/v1/tunnel");
        let client = start(config);

        // The TLS front relays to the route only after a handshake: two
        // failed handshakes, and no attempt reached the route.
        tokio::time::timeout(EXCHANGE, failed.wait_for(|n| *n >= 2))
            .await
            .expect("the client tries twice in time")
            .expect("the front runs");

        assert_eq!(daemon.attempts(), 0);
        assert_eq!(*client.state().borrow(), ClientState::Connecting);
    }
}

mod forward {
    use super::*;

    // TC: TUN-01
    #[tokio::test]
    async fn answers_the_local_response_byte_for_byte_when_the_app_answers() {
        let (daemon, _app, _client) = connected(patient()).await;

        let (status, headers, body) = daemon.get("/bytes").await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            headers.get("x-app").and_then(|v| v.to_str().ok()),
            Some("tunnel")
        );
        assert_eq!(body, bytes_body());
    }

    // TC: TUN-01
    #[tokio::test]
    async fn hands_the_request_to_the_app_unchanged_when_it_crosses() {
        let (daemon, app, _client) = connected(patient()).await;
        let request = Request::post("/submit?page=2&q=a%20b")
            .header(http::header::HOST, "shop-dev.apps.test")
            .header("x-forwarded-for", "203.0.113.7")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "shop-dev.apps.test")
            .header(http::header::CONTENT_TYPE, "application/octet-stream")
            .body(Full::new(Bytes::from_static(b"\x00\x01hello tunnel\xff")))
            .expect("request");

        let (status, _headers, _body) = daemon.send(request).await;

        assert_eq!(status, StatusCode::OK);
        let received = app.received();
        let got = received.first().expect("the app got the request");
        assert_eq!(got.method, http::Method::POST);
        assert_eq!(got.uri, "/submit?page=2&q=a%20b");
        let header = |name: &str| got.headers.get(name)?.to_str().ok();
        assert_eq!(header("host"), Some("shop-dev.apps.test"));
        assert_eq!(header("x-forwarded-for"), Some("203.0.113.7"));
        assert_eq!(header("x-forwarded-proto"), Some("https"));
        assert_eq!(header("x-forwarded-host"), Some("shop-dev.apps.test"));
        assert_eq!(header("content-type"), Some("application/octet-stream"));
        assert_eq!(got.body, Bytes::from_static(b"\x00\x01hello tunnel\xff"));
    }

    // TC: TUN-02
    #[tokio::test]
    async fn keeps_answers_apart_when_requests_run_at_once() {
        let (daemon, _app, _client) = connected(patient()).await;
        let mut requests = Vec::new();
        for n in 0..LocalApp::TOGETHER {
            let server = daemon.server.clone();
            requests.push(tokio::spawn(async move {
                let path = format!("/together/{n}");
                let request = Request::get(path.as_str())
                    .header(http::header::HOST, "shop-dev.apps.test")
                    .body(Full::new(Bytes::new()))
                    .expect("request");
                let (status, _headers, body) = send(&server, request).await;
                (path, status, body)
            }));
        }

        for request in requests {
            let (path, status, body) = request.await.expect("no request panics");
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(
                body,
                Bytes::from(path.clone()),
                "{path} gets its own answer"
            );
        }
    }

    // TC: TUN-09
    #[tokio::test]
    async fn answers_bad_gateway_when_the_local_port_refuses() {
        let daemon = Daemon::start(patient()).await;
        let port = closed_port().await;
        let client = start(daemon.client_config(port, KEEPALIVE));
        wait_connected(&client).await;
        daemon.wait_tunnel().await;

        let (status, _headers, _body) = daemon.get("/").await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    // TC: TUN-09 (no client at all: the listener's offline answer)
    #[tokio::test]
    async fn answers_service_unavailable_when_the_environment_has_no_tunnel() {
        let daemon = Daemon::start(patient()).await;

        let (status, _headers, _body) = daemon.get("/").await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
}

mod limits {
    use super::*;

    fn one_stream() -> ServerConfig {
        ServerConfig {
            max_streams: 1,
            idle_timeout: Duration::from_secs(3600),
            bandwidth: None,
        }
    }

    // TC: TUN-03
    #[tokio::test]
    async fn answers_service_unavailable_when_max_streams_are_open() {
        let (daemon, app, _client) = connected(one_stream()).await;
        let held = {
            let server = daemon.server.clone();
            tokio::spawn(async move {
                let request = Request::get("/hold")
                    .header(http::header::HOST, "shop-dev.apps.test")
                    .body(Full::new(Bytes::new()))
                    .expect("request");
                send(&server, request).await
            })
        };
        tokio::time::timeout(EXCHANGE, app.entered.notified())
            .await
            .expect("the first request reaches the app");

        let (extra, _headers, _body) = daemon.get("/").await;

        assert_eq!(extra, StatusCode::SERVICE_UNAVAILABLE);
        app.release.notify_one();
        let (first, _headers, body) = held.await.expect("the held request ends");
        assert_eq!((first, body), (StatusCode::OK, Bytes::from_static(b"held")));
    }

    // TC: TUN-03 (the bandwidth of an environment)
    #[tokio::test]
    async fn paces_the_answer_when_the_environment_has_a_bandwidth() {
        let limited = ServerConfig {
            bandwidth: Some(Bandwidth {
                bytes_per_second: 100_000,
                burst: 16 * 1024,
            }),
            ..patient()
        };
        let (daemon, _app, _client) = connected(limited).await;
        let started = tokio::time::Instant::now();

        let (status, _headers, body) = daemon.get("/bytes").await;

        assert_eq!((status, body), (StatusCode::CREATED, bytes_body()));
        // 100 000 bytes less the burst at 100 000 bytes a second: at least
        // 0.8 seconds; the bound leaves room below.
        assert!(
            started.elapsed() >= Duration::from_millis(600),
            "{:?}",
            started.elapsed()
        );
    }

    // TC: TUN-03 (one message never carries more than the crate writes)
    #[tokio::test]
    async fn drops_the_connection_when_the_client_sends_an_oversized_message() {
        let mut daemon = Daemon::start(patient()).await;
        let (mut socket, _hello) = raw_client(&daemon).await;
        let answer = Frame::Hello {
            environment: ENVIRONMENT.to_owned(),
            version: PROTOCOL_VERSION,
        };
        socket
            .send(hello_message(&answer))
            .await
            .expect("the answer goes out");
        daemon.wait_tunnel().await;

        // A text message the server would only log as a bad frame, were
        // it not too large.
        let oversized = " ".repeat(2 * 1024 * 1024);
        drop(socket.send(Message::text(oversized)).await);

        let ended = daemon.next_end().await;
        assert!(matches!(ended, Ok(Disconnect::Lost)), "{ended:?}");
    }

    // TC: TUN-03
    #[tokio::test]
    async fn answers_again_when_the_open_stream_ended() {
        let (daemon, app, _client) = connected(one_stream()).await;
        app.release.notify_one();
        let (first, _headers, _body) = daemon.get("/hold").await;
        assert_eq!(first, StatusCode::OK);

        let (next, _headers, body) = daemon.get("/").await;

        assert_eq!((next, body), (StatusCode::OK, Bytes::from_static(b"app")));
    }
}

mod reconnect {
    use super::*;

    // TC: TUN-04
    #[tokio::test]
    async fn reconnects_when_the_websocket_breaks() {
        let (daemon, _app, _client) = connected(patient()).await;

        daemon.break_connections();

        daemon.wait_upgrades(2).await;
        daemon.wait_tunnel().await;
        let (status, _headers, body) = daemon.get("/").await;
        assert_eq!((status, body), (StatusCode::OK, Bytes::from_static(b"app")));
    }

    // TC: TUN-04
    #[tokio::test]
    async fn connects_when_the_daemon_refused_the_first_attempts() {
        let daemon = Daemon::refusing(patient(), 3).await;
        let app = LocalApp::start("app").await;
        let client = start(daemon.client_config(app.port, KEEPALIVE));

        wait_connected(&client).await;

        assert_eq!(daemon.attempts(), 4, "three refusals, then the upgrade");
    }

    // TC: TUN-04
    #[tokio::test]
    async fn waits_longer_between_attempts_when_they_keep_failing() {
        let daemon = Daemon::refusing(patient(), 3).await;
        let app = LocalApp::start("app").await;
        let mut config = daemon.client_config(app.port, KEEPALIVE);
        config.backoff = Backoff {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(400),
        };
        let client = start(config);

        wait_connected(&client).await;

        let at = daemon.attempted_at();
        let gaps: Vec<Duration> = at.windows(2).map(|pair| pair[1] - pair[0]).collect();
        assert_eq!(gaps.len(), 3, "{gaps:?}");
        for (gap, least) in gaps.iter().zip([50, 100, 200]) {
            assert!(*gap >= Duration::from_millis(least), "{gaps:?}");
        }
    }

    // TC: TUN-04
    #[tokio::test]
    async fn waits_the_initial_delay_again_when_a_connection_that_worked_breaks() {
        // Eight refusals first: without the reset the next wait would be
        // 5 ms * 2^8 = 1.28 s, with it 5 ms.
        let daemon = Daemon::refusing(patient(), 8).await;
        let app = LocalApp::start("app").await;
        let mut config = daemon.client_config(app.port, KEEPALIVE);
        config.backoff = Backoff {
            initial: Duration::from_millis(5),
            max: Duration::from_secs(60),
        };
        let client = start(config);
        wait_connected(&client).await;
        daemon.wait_tunnel().await;
        let broken = tokio::time::Instant::now();

        daemon.break_connections();

        daemon.wait_upgrades(2).await;
        let again = *daemon.attempted_at().last().expect("the reconnect");
        assert!(
            again - broken < Duration::from_millis(640),
            "{:?}",
            again - broken
        );
    }

    // TC: TUN-04 (the client's own silence rule)
    #[tokio::test]
    async fn reconnects_when_the_server_goes_silent() {
        let daemon = Daemon::silent(patient()).await;
        let app = LocalApp::start("app").await;
        let client = start(daemon.client_config(app.port, Duration::from_millis(50)));
        wait_connected(&client).await;

        daemon.wait_upgrades(2).await;
    }

    // TC: TUN-04 (a refusal of the session does not heal by retrying)
    #[tokio::test]
    async fn stops_as_refused_when_the_daemon_refuses_the_session() {
        for refusal in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let daemon = Daemon::refusing_with(patient(), refusal).await;
            let client = start(daemon.client_config(closed_port().await, KEEPALIVE));

            let ended = wait_ended(&client).await;

            assert_eq!(ended, ClientState::Refused { status: refusal });
            assert_eq!(daemon.attempts(), 1, "{refusal}: no retry");
        }
    }

    // TC: TUN-04
    #[test]
    fn doubles_the_wait_up_to_60_seconds_when_attempts_fail_by_default() {
        let backoff = Backoff::default();

        let waits: Vec<u64> = (0..9)
            .map(|attempt| backoff.delay(attempt).as_secs())
            .collect();

        assert_eq!(waits, vec![1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    // TC: TUN-04
    #[test]
    fn stays_at_the_cap_when_failures_go_on_and_on() {
        let backoff = Backoff::default();

        assert_eq!(backoff.delay(u32::MAX), Duration::from_secs(60));
    }

    // TC: TUN-04
    #[test]
    fn doubles_from_the_injected_initial_wait_when_configured() {
        let backoff = Backoff {
            initial: Duration::from_millis(10),
            max: Duration::from_millis(50),
        };

        let waits: Vec<u128> = (0..5)
            .map(|attempt| backoff.delay(attempt).as_millis())
            .collect();

        assert_eq!(waits, vec![10, 20, 40, 50, 50]);
    }
}

mod eviction {
    use super::*;

    /// A daemon with a client of the app `first` and, connected after
    /// it, a second client of the same project for the app `second`.
    async fn two_clients() -> (Daemon, TunnelClient, TunnelClient, LocalApp, LocalApp) {
        let (daemon, first_app, first) = {
            let daemon = Daemon::start(patient()).await;
            let app = LocalApp::start("first").await;
            let client = start(daemon.client_config(app.port, KEEPALIVE));
            wait_connected(&client).await;
            daemon.wait_tunnel().await;
            (daemon, app, client)
        };
        let second_app = LocalApp::start("second").await;
        let second = start(daemon.client_config(second_app.port, KEEPALIVE));
        wait_connected(&second).await;
        (daemon, first, second, first_app, second_app)
    }

    // TC: TUN-05
    #[tokio::test]
    async fn notifies_the_first_client_when_a_second_connects() {
        let (_daemon, first, _second, _a, _b) = two_clients().await;

        wait_state(&first, |s| *s == ClientState::Evicted).await;
    }

    // TC: TUN-05 (the client's bytes still unread at the server when it
    // evicts the connection: the server reads them until the client
    // closes, or the reset of its socket takes the `Evicted` with it and
    // the client comes back)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ends_evicted_when_bytes_of_the_client_are_unread() {
        const ROUNDS: usize = 30;
        let daemon = Daemon::start(patient()).await;
        let app = LocalApp::start("app").await;
        let mut current = start(daemon.client_config(app.port, KEEPALIVE));
        wait_connected(&current).await;
        daemon.wait_tunnel().await;
        for round in 0..ROUNDS {
            let answers: Vec<_> = (0..4)
                .map(|_| {
                    let server = daemon.server.clone();
                    tokio::spawn(async move {
                        let request = Request::get("/bytes")
                            .header(http::header::HOST, "shop-dev.apps.test")
                            .body(Full::new(Bytes::new()))
                            .expect("request");
                        drop(server.forward(ENVIRONMENT, request).await);
                    })
                })
                .collect();
            tokio::task::yield_now().await;
            let next = start(daemon.client_config(app.port, KEEPALIVE));
            wait_connected(&next).await;

            let ended = wait_ended(&current).await;

            for answer in answers {
                answer.abort();
            }
            assert_eq!(ended, ClientState::Evicted, "round {round}");
            current = next;
        }
    }

    // TC: TUN-05 (no ping-pong: an evicted client does not reconnect)
    #[tokio::test]
    async fn stays_evicted_when_a_second_client_took_over() {
        let (daemon, first, _second, _a, _b) = two_clients().await;

        let ended = wait_ended(&first).await;

        assert_eq!(ended, ClientState::Evicted);
        assert_eq!(daemon.upgrades(), 2, "only the two clients' upgrades");
    }

    // TC: TUN-05
    #[tokio::test]
    async fn ends_the_first_connection_as_evicted_when_a_second_connects() {
        let (mut daemon, _first, _second, _a, _b) = two_clients().await;

        let ended = daemon.next_end().await;

        assert!(matches!(ended, Ok(Disconnect::Evicted)), "{ended:?}");
    }

    // TC: TUN-05
    #[tokio::test]
    async fn serves_requests_from_the_second_client_when_it_evicted_the_first() {
        let (daemon, first, _second, _a, _b) = two_clients().await;
        wait_state(&first, |s| *s == ClientState::Evicted).await;

        let (status, _headers, body) = daemon.get("/").await;

        assert_eq!(
            (status, body),
            (StatusCode::OK, Bytes::from_static(b"second"))
        );
    }
}

mod shutdown {
    use super::*;

    // TC: TUN-06
    #[tokio::test]
    async fn ends_the_connection_as_shutdown_at_once_when_the_client_sends_it() {
        let (mut daemon, _app, client) = connected(patient()).await;

        client.shutdown().await;

        let ended = daemon.next_end().await;
        assert!(matches!(ended, Ok(Disconnect::Shutdown)), "{ended:?}");
    }

    /// How many connections the races below run through.
    const ROUNDS: usize = 30;

    // TC: TUN-06 (the server's bytes still unread at the client when it
    // shuts down: the client reads them until the server closes, or the
    // reset of its socket takes the `Shutdown` with it)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ends_as_shutdown_when_the_client_shuts_down_with_bytes_unread() {
        let mut daemon = Daemon::start(patient()).await;
        let app = LocalApp::start("app").await;
        for round in 0..ROUNDS {
            let client = start(daemon.client_config(app.port, KEEPALIVE));
            wait_connected(&client).await;
            daemon.wait_tunnel().await;
            let server = daemon.server.clone();
            let flood = tokio::spawn(async move {
                let request = Request::post("/flood")
                    .header(http::header::HOST, "shop-dev.apps.test")
                    .body(Full::new(Bytes::from(vec![b'x'; 4 << 20])))
                    .expect("request");
                drop(server.forward(ENVIRONMENT, request).await);
            });
            tokio::task::yield_now().await;

            client.shutdown().await;

            let ended = daemon.next_end().await;
            flood.abort();
            assert!(
                matches!(ended, Ok(Disconnect::Shutdown)),
                "round {round}: {ended:?}"
            );
        }
    }

    // TC: TUN-06
    #[tokio::test]
    async fn forgets_the_tunnel_when_the_client_shut_down() {
        let (mut daemon, _app, client) = connected(patient()).await;

        client.shutdown().await;
        let _ended = daemon.next_end().await;

        assert!(!daemon.server.is_connected(ENVIRONMENT));
    }

    // TC: TUN-06
    #[tokio::test]
    async fn stops_the_client_when_it_shut_down() {
        let (daemon, _app, client) = connected(patient()).await;
        let mut state = client.state();

        client.shutdown().await;

        assert_eq!(*state.borrow_and_update(), ClientState::Stopped);
        assert_eq!(
            daemon.upgrades(),
            1,
            "a client that shut down does not reconnect"
        );
    }
}

mod keepalive {
    use super::*;

    /// A server that drops a connection silent for 300 ms.
    fn impatient() -> ServerConfig {
        ServerConfig {
            max_streams: 64,
            idle_timeout: Duration::from_millis(300),
            bandwidth: None,
        }
    }

    // TC: TUN-07
    #[test]
    fn pings_every_15_seconds_by_default() {
        let config = ClientConfig::new(
            "wss://liftoff.example.com/api/v1/tunnel".to_owned(),
            TOKEN.to_owned(),
            PROJECT.to_owned(),
            5173,
        );

        assert_eq!(config.keepalive, Duration::from_secs(15));
        assert_eq!(config.backoff, Backoff::default());
    }

    // TC: TUN-07
    #[tokio::test]
    async fn keeps_an_idle_connection_past_the_idle_timeout_when_it_pings() {
        let mut daemon = Daemon::start(impatient()).await;
        let app = LocalApp::start("app").await;
        let client = start(daemon.client_config(app.port, Duration::from_millis(50)));
        wait_connected(&client).await;
        daemon.wait_tunnel().await;

        // Three idle timeouts without a request.
        tokio::time::sleep(Duration::from_millis(900)).await;

        assert!(daemon.none_ended(), "the connection did not end");
        assert_eq!(daemon.upgrades(), 1, "the client did not reconnect");
        let (status, _headers, body) = daemon.get("/").await;
        assert_eq!((status, body), (StatusCode::OK, Bytes::from_static(b"app")));
    }

    // TC: TUN-07 (the idle timeout the keepalive outlives is real)
    #[tokio::test]
    async fn drops_a_silent_connection_as_lost_when_the_idle_timeout_passes() {
        let mut daemon = Daemon::start(impatient()).await;
        let app = LocalApp::start("app").await;
        let client = start(daemon.client_config(app.port, Duration::from_secs(3600)));
        wait_connected(&client).await;

        let ended = daemon.next_end().await;

        assert!(matches!(ended, Ok(Disconnect::Lost)), "{ended:?}");
    }
}

mod upgrades {
    use super::*;

    // TC: TUN-08
    #[tokio::test]
    async fn echoes_the_application_websocket_when_it_upgrades_through_the_tunnel() {
        let daemon = Daemon::start(patient()).await;
        let echo = websocket_echo().await;
        let client = start(daemon.client_config(echo, KEEPALIVE));
        wait_connected(&client).await;
        daemon.wait_tunnel().await;
        let listener = daemon.listener().await;

        let (mut socket, response) = tokio::time::timeout(
            EXCHANGE,
            tokio_tungstenite::connect_async(format!("ws://{listener}/socket")),
        )
        .await
        .expect("the upgrade ends in time")
        .expect("the browser's WebSocket opens through the tunnel");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        socket
            .send(Message::text("ping through the tunnel"))
            .await
            .expect("the browser sends");
        socket
            .send(Message::binary(vec![0_u8, 1, 2, 255]))
            .await
            .expect("the browser sends");

        let text = tokio::time::timeout(EXCHANGE, socket.next())
            .await
            .expect("in time");
        let binary = tokio::time::timeout(EXCHANGE, socket.next())
            .await
            .expect("in time");

        assert_eq!(
            text.and_then(Result::ok),
            Some(Message::text("ping through the tunnel"))
        );
        assert_eq!(
            binary.and_then(Result::ok),
            Some(Message::binary(vec![0_u8, 1, 2, 255]))
        );
    }
}
