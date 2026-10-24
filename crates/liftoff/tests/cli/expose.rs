//! `lof expose <PORT>` (`EXP-09…12`, `DEVT-09`), `lof env
//! share|unshare` and the tunnel in `lof status` (`DEVT-10`).
//!
//! The server is the recording [`Stub`] whose `GET /tunnel` is the real
//! tunnel server of `liftoff-tunnel` ([`Stub::start_with_tunnel`]): it
//! names the tunnel after the port and answers the upgrade with
//! `X-Liftoff-Address` and, unless shared, [`LOGIN_URL`]. Batch mode runs
//! the binary (stdin is no terminal, Ctrl-C is a real `SIGINT`, which is
//! how every batch run here ends); interactive mode runs [`Cli::run`] in
//! process with a terminal (Ctrl-C is [`Cli::interrupt`]).

use std::io::Write as _;
use std::process::Stdio;
use std::time::Duration;

use liftoff_tunnel::Disconnect;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Child;
use tokio::task::JoinHandle;

use crate::run::{Ran, finish, spawn};
use crate::support::{
    Buf, Cli, LOGIN_URL, Outcome, REFUSAL, Stub, TunnelEvent, TunnelEvents, TunnelOptions, ZONE,
    json,
};

/// How long a run may take before the test gives up.
const PATIENCE: Duration = Duration::from_secs(60);

/// The project of the `env` and `status` runs here.
const PROJECT: &str = "web";

/// The base domain of the fake daemon (`GET /server`).
const BASE: &str = "west.test";

/// The port every run exposes but [`forward`]'s.
const PORT: &str = "3000";

/// The address of [`PORT`] (`https://<port>.localhost.<base>`).
const ADDRESS: &str = "https://3000.localhost.west.test";

/// `POST .../share` of the tunnel `web-dev`.
const SHARE_PATH: &str = "/projects/web/environments/web-dev/share";

const PROD: &str = r#"{"id":"01J000000000000000000000E1","project":"web","name":"prod","kind":"compose","branch":"main","pinned_by":"u1","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"shared","created_at":"2026-10-18T10:00:00Z"}"#;

/// The tunnel `web-dev` of `u1`, private.
const TUNNEL_PRIVATE: &str = r#"{"id":"01J000000000000000000000E3","project":"web","name":"web-dev","kind":"tunnel","branch":null,"pinned_by":null,"owner":"u1","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"private","created_at":"2026-10-18T10:00:00Z"}"#;

/// [`TUNNEL_PRIVATE`] shared.
const TUNNEL_SHARED: &str = r#"{"id":"01J000000000000000000000E3","project":"web","name":"web-dev","kind":"tunnel","branch":null,"pinned_by":null,"owner":"u1","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"shared","created_at":"2026-10-18T10:00:00Z"}"#;

/// A fake daemon with the tunnel and the base domain `lof expose` asks
/// for (`GET /server`).
async fn daemon(options: TunnelOptions) -> (Stub, TunnelEvents) {
    let (stub, events) = Stub::start_with_tunnel(options).await;
    let stub = stub.route(
        "GET",
        "/server",
        vec![json(
            200,
            &format!(r#"{{"base_domain":"{BASE}","public_ip":"203.0.113.7","checks":[]}}"#),
        )],
    );
    (stub, events)
}

/// Whether `done` holds within [`PATIENCE`]; looks every 10 ms.
async fn until(mut done: impl FnMut() -> bool) -> bool {
    let looked = async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, looked).await.is_ok()
}

/// Copies `from` into `into` to its end.
async fn copy(mut from: impl AsyncRead + Unpin, mut into: Buf) {
    let mut chunk = [0_u8; 4096];
    while let Ok(read) = from.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        into.write_all(&chunk[..read]).unwrap();
    }
}

/// A batch run of `lof expose` whose stdout and stderr the test reads
/// while it runs.
struct Running {
    child: Child,
    out: Buf,
    err: Buf,
    readers: Vec<JoinHandle<()>>,
}

impl Running {
    /// `lof expose <args>` of `cli`.
    fn start(cli: &Cli, args: &[&str]) -> Self {
        let mut words = vec!["expose"];
        words.extend_from_slice(args);
        let mut child = spawn(cli, &words);
        let (out, err) = (Buf::default(), Buf::default());
        let readers = vec![
            tokio::spawn(copy(child.stdout.take().unwrap(), out.clone())),
            tokio::spawn(copy(child.stderr.take().unwrap(), err.clone())),
        ];
        Self {
            child,
            out,
            err,
            readers,
        }
    }

    /// Whether the run printed `text` on stdout within [`PATIENCE`].
    async fn printed(&self, text: &str) -> bool {
        until(|| self.out.text().contains(text)).await
    }

    /// Ctrl-C of the CLI, then the run to its end.
    async fn interrupt(self) -> Ran {
        if let Some(pid) = self.child.id() {
            send("INT", pid);
        }
        self.finish().await
    }

    /// The run to its end.
    async fn finish(mut self) -> Ran {
        let status = tokio::time::timeout(PATIENCE, self.child.wait())
            .await
            .expect("liftoff ends")
            .unwrap();
        for reader in self.readers {
            reader.await.unwrap();
        }
        Ran {
            code: status.code(),
            out: self.out.text(),
            err: self.err.text(),
        }
    }
}

/// Batch mode: `lof expose <args>` until the tunnel connected
/// `connections` times and the CLI printed `printed`, then Ctrl-C; what
/// the run printed, and whether every connection came.
async fn batch_until_connected(
    cli: &Cli,
    events: &mut TunnelEvents,
    args: &[&str],
    connections: usize,
    printed: &str,
) -> (Ran, bool) {
    let running = Running::start(cli, args);
    let mut came = true;
    for _ in 0..connections {
        if !events.connected().await {
            came = false;
            break;
        }
    }
    let came = came && running.printed(printed).await;
    (running.interrupt().await, came)
}

/// Interactive mode: [`batch_until_connected`] in process with a terminal.
async fn interactive_until_connected(
    cli: &Cli,
    events: &mut TunnelEvents,
    args: &[&str],
    printed: &str,
) -> (Outcome, bool) {
    let mut words = vec!["expose"];
    words.extend_from_slice(args);
    let drive = async {
        let came = events.connected().await && until(|| cli.printed().0.contains(printed)).await;
        cli.interrupt();
        came
    };
    tokio::join!(cli.run(&words), drive)
}

/// How often `text` is in `haystack`.
fn count(haystack: &str, text: &str) -> usize {
    haystack.matches(text).count()
}

/// Sends the signal `name` to `pid` only.
fn send(name: &str, pid: u32) {
    let status = std::process::Command::new("kill")
        .args([&format!("-{name}"), &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "kill -{name} {pid}");
}

mod address {
    use super::*;

    // TC: EXP-09 (the server's address, printed once)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_address_once_when_tunnel_connects_in_batch_mode() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url);
        let (ran, came) = batch_until_connected(&cli, &mut events, &[PORT], 1, ADDRESS).await;
        assert!(came, "no tunnel connected: {:?}, {ran:?}", stub.trail());
        assert_eq!(count(&ran.out, ADDRESS), 1, "{ran:?}");
    }

    // TC: EXP-09
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_address_once_when_tunnel_connects_in_interactive_mode() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![]);
        let (outcome, came) =
            interactive_until_connected(&cli, &mut events, &[PORT], ADDRESS).await;
        assert!(came, "no tunnel connected: {:?}, {outcome:?}", stub.trail());
        assert_eq!(count(&outcome.out, ADDRESS), 1, "{outcome:?}");
    }

    // TC: EXP-09 (the server names the address: any name under its zone)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_address_server_names_when_exposed_from_elsewhere() {
        let (stub, mut events) = daemon(TunnelOptions {
            environment: Some("owner-3f9a1c2b"),
            ..TunnelOptions::default()
        })
        .await;
        let cli = Cli::logged_in(&stub.url);
        let address = format!("https://owner-3f9a1c2b.{ZONE}");
        let (ran, came) = batch_until_connected(&cli, &mut events, &[PORT], 1, &address).await;
        assert!(came, "no tunnel connected: {:?}, {ran:?}", stub.trail());
    }

    // TC: EXP-09 (DEVT-07: the link of `X-Liftoff-Login-Url`, printed once)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_login_link_once_when_tunnel_is_private() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url);
        let (ran, came) = batch_until_connected(&cli, &mut events, &[PORT], 1, ADDRESS).await;
        assert!(came, "no tunnel connected: {:?}, {ran:?}", stub.trail());
        assert_eq!(count(&ran.out, LOGIN_URL), 1, "{ran:?}");
    }

    // TC: EXP-09 (`Authorization: Bearer <session>` and `X-Liftoff-Expose`;
    // no project is needed)
    #[tokio::test(flavor = "multi_thread")]
    async fn opens_tunnel_with_session_and_port_when_server_is_configured() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url);
        let (ran, _came) = batch_until_connected(&cli, &mut events, &[PORT], 1, ADDRESS).await;
        let upgrades: Vec<(Option<String>, Option<String>, bool)> = stub
            .seen_at("GET", "/tunnel")
            .into_iter()
            .map(|seen| (seen.authorization, seen.expose, seen.share))
            .collect();
        assert_eq!(
            upgrades,
            vec![(
                Some(format!("Bearer {}", crate::support::SESSION)),
                Some(PORT.to_owned()),
                false
            )],
            "{ran:?}"
        );
    }

    // TC: EXP-10 (`--share` travels with the upgrade, and no link is printed)
    #[tokio::test(flavor = "multi_thread")]
    async fn shares_without_login_link_when_share() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url);
        let (ran, came) =
            batch_until_connected(&cli, &mut events, &[PORT, "--share"], 1, ADDRESS).await;
        let shared: Vec<bool> = stub
            .seen_at("GET", "/tunnel")
            .into_iter()
            .map(|seen| seen.share)
            .collect();
        assert_eq!(
            (came, shared, ran.out.contains(LOGIN_URL)),
            (true, vec![true], false),
            "{ran:?}"
        );
    }
}

mod reconnect {
    use super::*;

    /// A daemon that drops the first connection as soon as it is the tunnel.
    async fn flaky() -> (Stub, TunnelEvents) {
        daemon(TunnelOptions {
            drop_first: true,
            ..TunnelOptions::default()
        })
        .await
    }

    // TC: DEVT-09 (the CLI reconnects with backoff)
    #[tokio::test(flavor = "multi_thread")]
    async fn connects_again_when_connection_drops() {
        let (stub, mut events) = flaky().await;
        let cli = Cli::logged_in(&stub.url);
        let (ran, came) = batch_until_connected(&cli, &mut events, &[PORT], 2, ADDRESS).await;
        assert!(came, "no second connection: {:?}, {ran:?}", stub.trail());
    }

    // TC: DEVT-09 (the address and the link are printed once, not per
    // connection)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_address_and_link_once_when_tunnel_reconnects() {
        let (stub, mut events) = flaky().await;
        let cli = Cli::logged_in(&stub.url);
        let (ran, came) = batch_until_connected(&cli, &mut events, &[PORT], 2, ADDRESS).await;
        assert!(came, "no second connection: {:?}, {ran:?}", stub.trail());
        assert_eq!(
            (count(&ran.out, ADDRESS), count(&ran.out, LOGIN_URL)),
            (1, 1),
            "{ran:?}"
        );
    }
}

mod ctrl_c {
    use super::*;

    // TC: DEVT-09 (Ctrl-C sends `Shutdown`, the route goes at once)
    #[tokio::test(flavor = "multi_thread")]
    async fn sends_shutdown_when_cli_gets_sigint_in_batch_mode() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url);
        let running = Running::start(&cli, &[PORT]);
        let came = events.connected().await;
        let ran = running.interrupt().await;
        let ended = if came { events.ended().await } else { None };
        assert!(came, "no tunnel connected: {:?}, {ran:?}", stub.trail());
        assert_eq!(ended, Some(TunnelEvent::Ended(Ok(Disconnect::Shutdown))));
    }

    // TC: DEVT-09
    #[tokio::test(flavor = "multi_thread")]
    async fn sends_shutdown_when_interrupted_in_interactive_mode() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![]);
        let drive = async {
            let came = events.connected().await;
            cli.interrupt();
            let ended = if came { events.ended().await } else { None };
            (came, ended)
        };
        let (outcome, (came, ended)) = tokio::join!(cli.run(&["expose", PORT]), drive);
        assert!(came, "no tunnel connected: {:?}, {outcome:?}", stub.trail());
        assert_eq!(
            ended,
            Some(TunnelEvent::Ended(Ok(Disconnect::Shutdown))),
            "{outcome:?}"
        );
    }

    // TC: DEVT-09
    #[tokio::test(flavor = "multi_thread")]
    async fn exits_130_when_cli_gets_sigint() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url);
        let running = Running::start(&cli, &[PORT]);
        let came = events.connected().await;
        let ran = running.interrupt().await;
        assert!(came, "no tunnel connected: {:?}", stub.trail());
        assert_eq!(ran.code, Some(130), "{ran:?}");
    }
}

mod refused {
    use super::*;

    // TC: EXP-11 (DEVT-06: the daemon refuses the session; the run ends,
    // there is nothing left to do)
    #[tokio::test(flavor = "multi_thread")]
    async fn exits_1_and_says_log_in_when_session_is_refused() {
        let (stub, _events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in_as(&stub.url, "lfs_expired");
        let ran = Running::start(&cli, &[PORT]).finish().await;
        assert!(
            ran.code == Some(1) && ran.err.contains("lof login"),
            "{ran:?}"
        );
    }

    // TC: EXP-11 (EXP-06: a port another user exposes on the server)
    #[tokio::test(flavor = "multi_thread")]
    async fn exits_1_with_servers_reason_when_port_is_another_users() {
        let (stub, _events) = daemon(TunnelOptions {
            refuse: Some(409),
            ..TunnelOptions::default()
        })
        .await;
        let cli = Cli::logged_in(&stub.url);
        let ran = Running::start(&cli, &[PORT]).finish().await;
        assert!(ran.code == Some(1) && ran.err.contains(REFUSAL), "{ran:?}");
    }

    // TC: EXP-11 (a second `lof expose` of the port evicts the first,
    // which ends)
    #[tokio::test(flavor = "multi_thread")]
    async fn exits_1_and_tells_takeover_when_another_expose_takes_tunnel() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let (first, second) = (Cli::logged_in(&stub.url), Cli::logged_in(&stub.url));
        let older = Running::start(&first, &[PORT]);
        let up = events.connected().await && older.printed(ADDRESS).await;
        let newer = Running::start(&second, &[PORT]);
        let older = older.finish().await;
        let newer = newer.interrupt().await;
        assert_eq!(
            (up, older.code, count(&older.err, "took over")),
            (true, Some(1), 1),
            "{older:?}, {newer:?}"
        );
    }
}

mod forward {
    use super::*;

    /// What the app answers.
    const ANSWER: &str = "hello from the app";

    // TC: EXP-09 (a request through the tunnel reaches the exposed port)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_through_tunnel_from_exposed_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let app = axum::Router::new().fallback(async || ANSWER);
        drop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url);
        let running = Running::start(&cli, &[&port]);
        let address = format!("https://{port}.{ZONE}");
        let up = events.connected().await && running.printed(&address).await;
        let answer = if up {
            Some(stub.through_tunnel(&port, "/hello").await)
        } else {
            None
        };
        let ran = running.interrupt().await;
        assert_eq!(answer, Some((200, ANSWER.to_owned())), "{ran:?}");
    }

    // TC: EXP-09 (nothing on the port yet: said once, the tunnel opens
    // all the same)
    #[tokio::test(flavor = "multi_thread")]
    async fn tells_nothing_listens_when_port_is_free() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url);
        let free = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port().to_string()
        };
        let running = Running::start(&cli, &[&free]);
        let up = events.connected().await;
        let ran = running.interrupt().await;
        assert!(up && ran.err.contains("nothing listens on port"), "{ran:?}");
    }
}

mod env_share {
    use super::*;

    /// A server whose `share` and `unshare` of `env` answer `view`.
    async fn visibility(env: &str, view: &str) -> Stub {
        let base = format!("/projects/web/environments/{env}");
        Stub::start()
            .await
            .route("POST", &format!("{base}/share"), vec![json(200, view)])
            .route("POST", &format!("{base}/unshare"), vec![json(200, view)])
    }

    /// `lof --project web env <words>` against `stub`, batch or
    /// interactive.
    async fn env(stub: &Stub, interactive: bool, words: &[&str]) -> Outcome {
        let cli = Cli::logged_in(&stub.url);
        let cli = if interactive {
            cli.terminal(vec![])
        } else {
            cli
        };
        let mut args = vec!["--project", PROJECT, "env"];
        args.extend_from_slice(words);
        cli.run(&args).await
    }

    // TC: DEVT-10 (`lof env share [--env E]`)
    #[tokio::test]
    async fn posts_share_of_env_flag_in_batch_mode() {
        let stub = visibility("web-dev", TUNNEL_SHARED).await;
        let outcome = env(&stub, false, &["share", "--env", "web-dev"]).await;
        assert_eq!(stub.seen_at("POST", SHARE_PATH).len(), 1, "{outcome:?}");
    }

    // TC: DEVT-10
    #[tokio::test]
    async fn posts_share_of_env_flag_in_interactive_mode() {
        let stub = visibility("web-dev", TUNNEL_SHARED).await;
        let outcome = env(&stub, true, &["share", "--env", "web-dev"]).await;
        assert_eq!(stub.seen_at("POST", SHARE_PATH).len(), 1, "{outcome:?}");
    }

    // TC: DEVT-10
    #[tokio::test]
    async fn posts_unshare_of_env_flag_when_env_unshare() {
        let stub = visibility("web-dev", TUNNEL_PRIVATE).await;
        let outcome = env(&stub, false, &["unshare", "--env", "web-dev"]).await;
        assert_eq!(
            stub.seen_at("POST", "/projects/web/environments/web-dev/unshare")
                .len(),
            1,
            "{outcome:?}"
        );
    }

    // TC: DEVT-10 (the environment of `.liftoff`, else `prod`)
    #[tokio::test]
    async fn posts_share_of_prod_when_no_env_flag() {
        let stub = visibility("prod", PROD).await;
        let outcome = env(&stub, false, &["share"]).await;
        assert_eq!(
            stub.seen_at("POST", "/projects/web/environments/prod/share")
                .len(),
            1,
            "{outcome:?}"
        );
    }

    // TC: DEVT-10
    #[tokio::test]
    async fn exits_0_when_env_share_succeeds() {
        let stub = visibility("web-dev", TUNNEL_SHARED).await;
        let outcome = env(&stub, false, &["share", "--env", "web-dev"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
    }

    // TC: DEVT-10 (the answer is the environment, as `status` shows it)
    #[tokio::test]
    async fn prints_shared_visibility_when_env_share_succeeds() {
        let stub = visibility("web-dev", TUNNEL_SHARED).await;
        let outcome = env(&stub, false, &["share", "--env", "web-dev"]).await;
        let visibility = outcome
            .out
            .lines()
            .find(|line| line.split_whitespace().next() == Some("visibility"))
            .and_then(|line| line.split_whitespace().nth(1));
        assert_eq!(visibility, Some("shared"), "{outcome:?}");
    }

    // TC: DEVT-10
    #[tokio::test]
    async fn prints_private_visibility_when_env_unshare_succeeds() {
        let stub = visibility("web-dev", TUNNEL_PRIVATE).await;
        let outcome = env(&stub, false, &["unshare", "--env", "web-dev"]).await;
        let visibility = outcome
            .out
            .lines()
            .find(|line| line.split_whitespace().next() == Some("visibility"))
            .and_then(|line| line.split_whitespace().nth(1));
        assert_eq!(visibility, Some("private"), "{outcome:?}");
    }
}

mod status {
    use super::*;

    /// A server whose project `web` has `prod` and the environments
    /// `listed`.
    async fn listing(listed: &str) -> Stub {
        Stub::start()
            .await
            .route(
                "GET",
                "/projects/web/environments/prod",
                vec![json(200, PROD)],
            )
            .route("GET", "/projects/web/environments", vec![json(200, listed)])
    }

    /// The words of the `tunnel` line of `out`, if there is one.
    fn tunnel_line(out: &str) -> Option<Vec<String>> {
        out.lines()
            .find(|line| line.split_whitespace().next() == Some("tunnel"))
            .map(|line| line.split_whitespace().skip(1).map(str::to_owned).collect())
    }

    // TC: DEVT-09 (the tunnel's state in `lof status`)
    #[tokio::test]
    async fn shows_tunnel_name_and_state_when_project_has_tunnel() {
        let stub = listing(&format!("[{PROD},{TUNNEL_PRIVATE}]")).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--project", PROJECT, "status"]).await;
        assert_eq!(
            tunnel_line(&outcome.out),
            Some(vec!["web-dev".to_owned(), "running".to_owned()]),
            "{outcome:?}"
        );
    }

    // TC: DEVT-09
    #[tokio::test]
    async fn shows_tunnel_name_and_state_in_interactive_mode() {
        let stub = listing(&format!("[{PROD},{TUNNEL_PRIVATE}]")).await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![]);
        let outcome = cli.run(&["--project", PROJECT, "status"]).await;
        assert_eq!(
            tunnel_line(&outcome.out),
            Some(vec!["web-dev".to_owned(), "running".to_owned()]),
            "{outcome:?}"
        );
    }

    // TC: DEVT-09
    #[tokio::test]
    async fn shows_no_tunnel_line_when_project_has_no_tunnel() {
        let stub = listing(&format!("[{PROD}]")).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--project", PROJECT, "status"]).await;
        assert_eq!(
            (outcome.code, tunnel_line(&outcome.out)),
            (0, None),
            "{outcome:?}"
        );
    }
}

mod no_server {
    use super::*;

    // TC: EXP-12 (local mode has nothing to expose through: 78, nothing
    // sent)
    #[tokio::test]
    async fn exits_78_with_hint_when_no_server_in_batch_mode() {
        let cli = Cli::new();
        let ran = finish(spawn(&cli, &["expose", PORT])).await;
        assert!(
            ran.code == Some(78) && ran.err.contains("lof login"),
            "{ran:?}"
        );
    }

    // TC: EXP-12
    #[tokio::test]
    async fn exits_78_when_no_server_in_interactive_mode() {
        let cli = Cli::new().terminal(vec![]);
        let outcome = cli.run(&["expose", PORT]).await;
        assert_eq!(outcome.code, 78, "{outcome:?}");
    }
}

mod usage {
    use super::*;

    // TC: EXP-12 (a port is 1–65535)
    #[rstest::rstest]
    #[case::zero("0")]
    #[case::too_big("65536")]
    #[case::no_number("web")]
    #[tokio::test]
    async fn exits_64_when_port_is_no_port(#[case] port: &str) {
        let (stub, _events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url);
        let ran = finish(spawn(&cli, &["expose", port])).await;
        assert!(
            ran.code == Some(64) && stub.seen_at("GET", "/tunnel").is_empty(),
            "{ran:?}"
        );
    }

    // TC: EXP-12 (`lof dev` is gone)
    #[tokio::test]
    async fn exits_64_when_dev_is_asked() {
        let cli = Cli::new();
        let ran = finish(spawn(&cli, &["dev"])).await;
        assert_eq!(ran.code, Some(64), "{ran:?}");
    }
}
