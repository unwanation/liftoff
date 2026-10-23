//! `lof dev` with the tunnel, `--share`, `--local`, `lof env
//! share|unshare` and the tunnel in `lof status` (`DEVT-09…12`).
//!
//! The server is the recording [`Stub`] whose `GET /tunnel` is the real
//! tunnel server of `liftoff-tunnel` ([`Stub::start_with_tunnel`]): it
//! names the tunnel `<project>-dev` and answers the upgrade of a private
//! one with [`LOGIN_URL`]. Batch mode runs the binary (stdin is no
//! terminal, Ctrl-C is a real `SIGINT`); interactive mode runs
//! [`Cli::run`] in process with a terminal (Ctrl-C is
//! [`Cli::interrupt`]). The dev command waits until the test writes
//! `stop` next to it, so the tunnel stays up as long as the test needs.

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use liftoff_tunnel::Disconnect;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Child;
use tokio::task::JoinHandle;

use crate::dev::{Ran, spawn};
use crate::support::{
    Answer, Buf, Cli, LOGIN_URL, Outcome, Stub, TunnelEvent, TunnelEvents, TunnelOptions, json,
    problem,
};

/// How long a run may take before the test gives up.
const PATIENCE: Duration = Duration::from_secs(60);

/// The project of every run here.
const PROJECT: &str = "web";

/// The base domain of the fake daemon (`GET /server`).
const BASE: &str = "west.test";

/// The dev address of `web-dev` on [`BASE`] (`<env>.<base>`).
const DEV_ADDRESS: &str = "https://web-dev.west.test";

/// The environment `web-dev` of `web`.
const DEV_ENV_PATH: &str = "/projects/web/environments/web-dev";

/// `POST .../share` of the tunnel `web-dev`.
const SHARE_PATH: &str = "/projects/web/environments/web-dev/share";

/// The user of single-user mode, an admin.
const ME: &str =
    r#"{"id":"u1","login":"owner","email":"owner@example.com","role":"admin","source":"local"}"#;

/// No module: the tunnel is `<project>-dev`.
const NO_MODULES: &str = "[]";

const PROD: &str = r#"{"id":"01J000000000000000000000E1","project":"web","name":"prod","kind":"compose","branch":"main","pinned_by":"u1","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"shared","created_at":"2026-10-18T10:00:00Z"}"#;

/// The tunnel `web-dev` of [`ME`], private.
const TUNNEL_PRIVATE: &str = r#"{"id":"01J000000000000000000000E3","project":"web","name":"web-dev","kind":"tunnel","branch":null,"pinned_by":null,"owner":"u1","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"private","created_at":"2026-10-18T10:00:00Z"}"#;

/// [`TUNNEL_PRIVATE`] shared.
const TUNNEL_SHARED: &str = r#"{"id":"01J000000000000000000000E3","project":"web","name":"web-dev","kind":"tunnel","branch":null,"pinned_by":null,"owner":"u1","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"shared","created_at":"2026-10-18T10:00:00Z"}"#;

/// A tunnel `web-dev` of another user, private.
const FOREIGN_TUNNEL: &str = r#"{"id":"01J000000000000000000000E4","project":"web","name":"web-dev","kind":"tunnel","branch":null,"pinned_by":null,"owner":"u9","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"private","created_at":"2026-10-18T10:00:00Z"}"#;

/// A compose environment that happens to be named `web-dev`, private.
const COMPOSE_WEB_DEV: &str = r#"{"id":"01J000000000000000000000E5","project":"web","name":"web-dev","kind":"compose","branch":"dev","pinned_by":"u1","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"private","created_at":"2026-10-18T10:00:00Z"}"#;

/// The routes `lof dev` with a server may call besides `GET /tunnel`:
/// the secrets, the base domain, who the user is (`me`), the modules
/// (`modules`) and the environments of the project.
fn dev_routes(stub: Stub, me: &str, modules: &str) -> Stub {
    stub.route(
        "GET",
        "/secrets/resolve",
        vec![json(200, r#"{"vars": []}"#)],
    )
    .route(
        "GET",
        "/server",
        vec![json(
            200,
            &format!(r#"{{"base_domain":"{BASE}","public_ip":"203.0.113.7","checks":[]}}"#),
        )],
    )
    .route("GET", "/me", vec![json(200, me)])
    .route("GET", "/modules", vec![json(200, modules)])
    .route(
        "GET",
        "/projects/web/environments",
        vec![json(200, &format!("[{PROD},{TUNNEL_PRIVATE}]"))],
    )
}

/// A fake daemon with the tunnel and the routes of `lof dev` in
/// single-user mode.
async fn daemon(options: TunnelOptions) -> (Stub, TunnelEvents) {
    let (stub, events) = Stub::start_with_tunnel(options).await;
    (dev_routes(stub, ME, NO_MODULES), events)
}

/// [`daemon`] whose `GET .../web-dev` answers `environment` and whose
/// `POST .../web-dev/share` answers `shares` in order.
async fn sharing_daemon(environment: Vec<Answer>, shares: Vec<Answer>) -> (Stub, TunnelEvents) {
    let (stub, events) = daemon(TunnelOptions::default()).await;
    let stub = stub
        .route("GET", DEV_ENV_PATH, environment)
        .route("POST", SHARE_PATH, shares);
    (stub, events)
}

/// A project in the working tree of a [`Cli`] logged in to `stub` whose
/// dev command waits for `stop` (at most a minute) and records that it
/// ran in `ran`.
fn waiting_project(stub: &Stub) -> Cli {
    waiting(Cli::logged_in(&stub.url))
}

/// `cli` with the dev command of [`waiting_project`].
fn waiting(cli: Cli) -> Cli {
    let root = cli.root.display().to_string();
    let script = format!(
        "touch {root}/ran\ni=0\nwhile [ ! -e {root}/stop ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done\n"
    );
    write_dev_script(&cli, &script);
    cli
}

/// A project of a [`Cli`] in local mode (no server) whose dev command
/// records that it ran and ends.
fn local_project() -> Cli {
    let cli = Cli::new();
    let root = cli.root.display().to_string();
    write_dev_script(&cli, &format!("touch {root}/ran\n"));
    cli
}

/// `dev.sh` holds `script`; `liftoff.toml` runs it as the `[dev] command`.
fn write_dev_script(cli: &Cli, script: &str) {
    let path = cli.root.join("dev.sh");
    std::fs::write(&path, script).unwrap();
    cli.write_root(
        "liftoff.toml",
        &format!(
            "[service]\ncompose = \"deploy/compose.yaml\"\nexpose = \"web:3000\"\n\n[dev]\ncommand = \"sh {}\"\n",
            path.display()
        ),
    );
}

/// Lets the dev command of `cli` end.
fn stop(cli: &Cli) {
    std::fs::write(cli.root.join("stop"), "").unwrap();
}

/// Whether the dev command of `cli` ran.
fn dev_command_ran(root: &Path) -> bool {
    root.join("ran").exists()
}

/// `lof --project web dev <extra>` of `cli` as a process (batch mode).
fn spawn_dev(cli: &Cli, extra: &[&str]) -> Child {
    let mut args = vec!["--project", PROJECT, "dev"];
    args.extend_from_slice(extra);
    spawn(cli, &args)
}

/// The run of `child` to its end.
async fn finish(child: Child) -> Ran {
    tokio::time::timeout(PATIENCE, child.wait_with_output())
        .await
        .expect("liftoff ends")
        .unwrap()
        .into()
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

/// A batch run of `lof dev` whose stdout and stderr the test reads while
/// it runs.
struct Running {
    child: Child,
    out: Buf,
    err: Buf,
    readers: Vec<JoinHandle<()>>,
}

impl Running {
    /// `lof --project web dev <extra>` of `cli`.
    fn start(cli: &Cli, extra: &[&str]) -> Self {
        let mut child = spawn_dev(cli, extra);
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

    /// Whether the run told `text` on stderr within [`PATIENCE`].
    async fn told(&self, text: &str) -> bool {
        until(|| self.err.text().contains(text)).await
    }

    /// The pid of the CLI.
    fn pid(&self) -> u32 {
        self.child.id().unwrap()
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

/// Batch mode: `lof dev <extra>` until the tunnel connected `connections`
/// times and the CLI printed `printed` (it handled the first connection),
/// then the dev command ends; what the run printed, and whether every
/// connection came.
async fn batch_until_connected(
    cli: &Cli,
    events: &mut TunnelEvents,
    extra: &[&str],
    connections: usize,
    printed: &str,
) -> (Ran, bool) {
    let running = Running::start(cli, extra);
    let mut came = true;
    for _ in 0..connections {
        if !events.connected().await {
            came = false;
            break;
        }
    }
    let came = came && running.printed(printed).await;
    stop(cli);
    (running.finish().await, came)
}

/// Interactive mode: [`batch_until_connected`] in process with a terminal.
async fn interactive_until_connected(
    cli: &Cli,
    events: &mut TunnelEvents,
    extra: &[&str],
    connections: usize,
    printed: &str,
) -> (Outcome, bool) {
    let mut args = vec!["--project", PROJECT, "dev"];
    args.extend_from_slice(extra);
    let drive = async {
        let mut came = true;
        for _ in 0..connections {
            if !events.connected().await {
                came = false;
                break;
            }
        }
        let came = came && until(|| cli.printed().0.contains(printed)).await;
        stop(cli);
        came
    };
    let (outcome, came) = tokio::join!(cli.run(&args), drive);
    (outcome, came)
}

/// Batch mode: the run of `lof dev <extra>` that needs no tunnel to end
/// (the dev command waits for `stop`, written at once).
async fn batch_without_tunnel(cli: &Cli, extra: &[&str]) -> Ran {
    stop(cli);
    finish(spawn_dev(cli, extra)).await
}

/// Interactive mode: [`batch_without_tunnel`] in process.
async fn interactive_without_tunnel(cli: &Cli, extra: &[&str]) -> Outcome {
    stop(cli);
    let mut args = vec!["--project", PROJECT, "dev"];
    args.extend_from_slice(extra);
    cli.run(&args).await
}

/// How often `text` is in `haystack`.
fn count(haystack: &str, text: &str) -> usize {
    haystack.matches(text).count()
}

/// The positions of `(method, path)` in the trail of `stub`.
fn positions(stub: &Stub, method: &str, path: &str) -> Vec<usize> {
    stub.trail()
        .iter()
        .enumerate()
        .filter(|(_, (seen_method, seen_path))| seen_method == method && seen_path == path)
        .map(|(index, _)| index)
        .collect()
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

    // TC: DEVT-09 (`https://<env>.<base>`, printed once)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_dev_address_once_when_tunnel_connects_in_batch_mode() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub);
        let (ran, came) = batch_until_connected(&cli, &mut events, &[], 1, DEV_ADDRESS).await;
        assert!(came, "no tunnel connected: {:?}, {ran:?}", stub.trail());
        assert_eq!(count(&ran.out, DEV_ADDRESS), 1, "{ran:?}");
    }

    // TC: DEVT-09
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_dev_address_once_when_tunnel_connects_in_interactive_mode() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub).terminal(vec![]);
        let (outcome, came) =
            interactive_until_connected(&cli, &mut events, &[], 1, DEV_ADDRESS).await;
        assert!(came, "no tunnel connected: {:?}, {outcome:?}", stub.trail());
        assert_eq!(count(&outcome.out, DEV_ADDRESS), 1, "{outcome:?}");
    }

    // TC: DEVT-09 (DEVT-07: the link of `X-Liftoff-Login-Url`, printed once)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_login_link_once_when_tunnel_is_private_in_batch_mode() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub);
        let (ran, came) = batch_until_connected(&cli, &mut events, &[], 1, DEV_ADDRESS).await;
        assert!(came, "no tunnel connected: {:?}, {ran:?}", stub.trail());
        assert_eq!(count(&ran.out, LOGIN_URL), 1, "{ran:?}");
    }

    // TC: DEVT-09
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_login_link_once_when_tunnel_is_private_in_interactive_mode() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub).terminal(vec![]);
        let (outcome, came) =
            interactive_until_connected(&cli, &mut events, &[], 1, DEV_ADDRESS).await;
        assert!(came, "no tunnel connected: {:?}, {outcome:?}", stub.trail());
        assert_eq!(count(&outcome.out, LOGIN_URL), 1, "{outcome:?}");
    }

    // TC: DEVT-09 (`Authorization: Bearer <session>` and `X-Liftoff-Project`)
    #[tokio::test(flavor = "multi_thread")]
    async fn opens_tunnel_with_session_and_project_when_server_is_configured() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub);
        let (ran, _came) = batch_until_connected(&cli, &mut events, &[], 1, DEV_ADDRESS).await;
        let upgrades: Vec<(Option<String>, Option<String>)> = stub
            .seen_at("GET", "/tunnel")
            .into_iter()
            .map(|seen| (seen.authorization, seen.project))
            .collect();
        assert_eq!(
            upgrades,
            vec![(
                Some(format!("Bearer {}", crate::support::SESSION)),
                Some(PROJECT.to_owned())
            )],
            "{ran:?}"
        );
    }

    // TC: DEVT-09 (the note is gone once the tunnel runs)
    #[tokio::test(flavor = "multi_thread")]
    async fn says_nothing_of_unavailable_tunnel_when_tunnel_connects() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub);
        let (ran, came) = batch_until_connected(&cli, &mut events, &[], 1, DEV_ADDRESS).await;
        assert!(
            came && !ran.err.contains("not available yet"),
            "connected {came}: {ran:?}"
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
        let cli = waiting_project(&stub);
        let (ran, came) = batch_until_connected(&cli, &mut events, &[], 2, DEV_ADDRESS).await;
        assert!(came, "no second connection: {:?}, {ran:?}", stub.trail());
    }

    // TC: DEVT-09 (the address is printed once, not per connection)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_dev_address_once_when_tunnel_reconnects() {
        let (stub, mut events) = flaky().await;
        let cli = waiting_project(&stub);
        let (ran, came) = batch_until_connected(&cli, &mut events, &[], 2, DEV_ADDRESS).await;
        assert!(came, "no second connection: {:?}, {ran:?}", stub.trail());
        assert_eq!(count(&ran.out, DEV_ADDRESS), 1, "{ran:?}");
    }

    // TC: DEVT-09 (each upgrade answers a link; the CLI prints the first)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_login_link_once_when_tunnel_reconnects() {
        let (stub, mut events) = flaky().await;
        let cli = waiting_project(&stub);
        let (ran, came) = batch_until_connected(&cli, &mut events, &[], 2, DEV_ADDRESS).await;
        assert!(came, "no second connection: {:?}, {ran:?}", stub.trail());
        assert_eq!(count(&ran.out, LOGIN_URL), 1, "{ran:?}");
    }
}

mod ctrl_c {
    use super::*;

    // TC: DEVT-09 (Ctrl-C sends `Shutdown`, the route goes at once)
    #[tokio::test(flavor = "multi_thread")]
    async fn sends_shutdown_when_cli_gets_sigint_in_batch_mode() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub);
        let mut child = spawn_dev(&cli, &[]);
        let came = events.connected().await;
        if let Some(pid) = child.id() {
            let status = std::process::Command::new("kill")
                .args(["-INT", &pid.to_string()])
                .status()
                .unwrap();
            assert!(status.success(), "kill -INT {pid}");
        }
        let ended = if came { events.ended().await } else { None };
        stop(&cli);
        let _ended = tokio::time::timeout(PATIENCE, child.wait()).await;
        assert!(came, "no tunnel connected: {:?}", stub.trail());
        assert_eq!(ended, Some(TunnelEvent::Ended(Ok(Disconnect::Shutdown))));
    }

    // TC: DEVT-09
    #[tokio::test(flavor = "multi_thread")]
    async fn sends_shutdown_when_interrupted_in_interactive_mode() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub).terminal(vec![]);
        let args = ["--project", PROJECT, "dev"];
        let drive = async {
            let came = events.connected().await;
            cli.interrupt();
            let ended = if came { events.ended().await } else { None };
            stop(&cli);
            (came, ended)
        };
        let (outcome, (came, ended)) = tokio::join!(cli.run(&args), drive);
        assert!(came, "no tunnel connected: {:?}, {outcome:?}", stub.trail());
        assert_eq!(
            ended,
            Some(TunnelEvent::Ended(Ok(Disconnect::Shutdown))),
            "{outcome:?}"
        );
    }

    // TC: DEVT-09 (Ctrl-C ends the dev command, also with a tunnel)
    #[tokio::test(flavor = "multi_thread")]
    async fn exits_130_when_cli_gets_sigint_with_tunnel() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub);
        let child = spawn_dev(&cli, &[]);
        let came = events.connected().await;
        if let Some(pid) = child.id() {
            send("INT", pid);
        }
        let ran = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output()).await;
        stop(&cli);
        assert!(came, "no tunnel connected: {:?}", stub.trail());
        let ran: Ran = ran.expect("liftoff ends after Ctrl-C").unwrap().into();
        assert_eq!(ran.code, Some(130), "{ran:?}");
    }

    /// Kills the grandchild whatever the test's outcome.
    struct Reaper(u32);

    impl Drop for Reaper {
        fn drop(&mut self) {
            drop(
                std::process::Command::new("kill")
                    .args(["-KILL", &self.0.to_string()])
                    .stderr(Stdio::null())
                    .status(),
            );
        }
    }

    /// Whether `pid` is gone (or a zombie nobody waits for).
    fn gone(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
    }

    // TC: DEVT-09 (DEV-04: Ctrl-C ends the dev command's process group also when a tunnel is open; nobody writes `stop`)
    #[tokio::test(flavor = "multi_thread")]
    async fn ends_grandchild_when_cli_gets_sigint_with_tunnel() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = Cli::logged_in(&stub.url);
        let record = cli.root.join("grandchild.pid");
        write_dev_script(
            &cli,
            &format!("sleep 100 &\necho $! > {}\nwait\n", record.display()),
        );
        let running = Running::start(&cli, &[]);
        let up = events.connected().await && running.printed(DEV_ADDRESS).await;
        let mut pid = None;
        assert!(
            until(|| {
                pid = std::fs::read_to_string(&record)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok());
                pid.is_some()
            })
            .await,
            "the dev command records its grandchild"
        );
        let pid = pid.unwrap();
        let _reaper = Reaper(pid);
        send("INT", running.pid());
        let ran = running.finish().await;
        let ended = until(|| gone(pid)).await;
        assert_eq!((up, ended, ran.code), (true, true, Some(130)), "{ran:?}");
    }
}

mod share {
    use super::*;

    // TC: DEVT-10 (`POST.../share` before the upgrade)
    #[tokio::test(flavor = "multi_thread")]
    async fn posts_share_before_tunnel_upgrade_in_batch_mode() {
        let (stub, mut events) = sharing_daemon(
            vec![json(200, TUNNEL_PRIVATE)],
            vec![json(200, TUNNEL_SHARED)],
        )
        .await;
        let cli = waiting_project(&stub);
        let (ran, _came) =
            batch_until_connected(&cli, &mut events, &["--share"], 1, DEV_ADDRESS).await;
        let shares = positions(&stub, "POST", SHARE_PATH);
        let upgrades = positions(&stub, "GET", "/tunnel");
        assert!(
            !shares.is_empty() && !upgrades.is_empty() && shares.first() < upgrades.first(),
            "trail {:?}: {ran:?}",
            stub.trail()
        );
    }

    // TC: DEVT-10
    #[tokio::test(flavor = "multi_thread")]
    async fn posts_share_before_tunnel_upgrade_in_interactive_mode() {
        let (stub, mut events) = sharing_daemon(
            vec![json(200, TUNNEL_PRIVATE)],
            vec![json(200, TUNNEL_SHARED)],
        )
        .await;
        let cli = waiting_project(&stub).terminal(vec![]);
        let (outcome, _came) =
            interactive_until_connected(&cli, &mut events, &["--share"], 1, DEV_ADDRESS).await;
        let shares = positions(&stub, "POST", SHARE_PATH);
        let upgrades = positions(&stub, "GET", "/tunnel");
        assert!(
            !shares.is_empty() && !upgrades.is_empty() && shares.first() < upgrades.first(),
            "trail {:?}: {outcome:?}",
            stub.trail()
        );
    }

    // TC: DEVT-10 (a shared tunnel needs no login: no link to print)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_dev_address_without_login_link_when_share() {
        let (stub, mut events) = sharing_daemon(
            vec![json(200, TUNNEL_PRIVATE)],
            vec![json(200, TUNNEL_SHARED)],
        )
        .await;
        let cli = waiting_project(&stub);
        let (ran, came) =
            batch_until_connected(&cli, &mut events, &["--share"], 1, DEV_ADDRESS).await;
        assert!(came, "no tunnel connected: {:?}, {ran:?}", stub.trail());
        assert_eq!(
            (count(&ran.out, DEV_ADDRESS), count(&ran.out, LOGIN_URL)),
            (1, 0),
            "{ran:?}"
        );
    }

    // TC: DEVT-10 (the first `lof dev`: the tunnel does not exist before
    // its upgrade, so `share` follows the server's `Hello`)
    #[tokio::test(flavor = "multi_thread")]
    async fn posts_share_after_upgrade_when_tunnel_does_not_exist_yet() {
        let (stub, mut events) = sharing_daemon(
            vec![problem(404, "environment web-dev of web does not exist")],
            vec![json(200, TUNNEL_SHARED)],
        )
        .await;
        let cli = waiting_project(&stub);
        let (ran, _came) =
            batch_until_connected(&cli, &mut events, &["--share"], 1, DEV_ADDRESS).await;
        let shares = positions(&stub, "POST", SHARE_PATH);
        let upgrades = positions(&stub, "GET", "/tunnel");
        assert!(
            shares.len() == 1 && !upgrades.is_empty() && shares.first() > upgrades.first(),
            "trail {:?}: {ran:?}",
            stub.trail()
        );
    }

    // TC: DEVT-10 (the address once, and no link: the share follows it)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_dev_address_without_login_link_when_share_follows_upgrade() {
        let (stub, mut events) = sharing_daemon(
            vec![problem(404, "environment web-dev of web does not exist")],
            vec![json(200, TUNNEL_SHARED)],
        )
        .await;
        let cli = waiting_project(&stub);
        let (ran, came) =
            batch_until_connected(&cli, &mut events, &["--share"], 1, DEV_ADDRESS).await;
        assert!(came, "no tunnel connected: {:?}, {ran:?}", stub.trail());
        assert_eq!(
            (count(&ran.out, DEV_ADDRESS), count(&ran.out, LOGIN_URL)),
            (1, 0),
            "{ran:?}"
        );
    }

    // TC: DEVT-10 (amendment of: `<project>-<login>` with `tenants`, the login of `GET /me`)
    #[tokio::test(flavor = "multi_thread")]
    async fn posts_share_of_login_tunnel_before_upgrade_when_tenants() {
        const ME_ANN: &str = r#"{"id":"u2","login":"Ann.Lee","email":"ann@example.com","role":"user","source":"local"}"#;
        const TENANTS: &str = r#"[{"name":"tenants","version":"0.1.0","enabled":true,"error":null,"installed_at":"2026-10-18T10:00:00Z","commands":[],"routes":[],"tools":[],"config_schema":null}]"#;
        const ANN_TUNNEL: &str = r#"{"id":"01J000000000000000000000E6","project":"web","name":"web-ann-lee","kind":"tunnel","branch":null,"pinned_by":null,"owner":"u2","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"private","created_at":"2026-10-18T10:00:00Z"}"#;
        const ANN_SHARE: &str = "/projects/web/environments/web-ann-lee/share";
        let (stub, mut events) = Stub::start_with_tunnel(TunnelOptions {
            environment: Some("web-ann-lee"),
            ..TunnelOptions::default()
        })
        .await;
        let stub = dev_routes(stub, ME_ANN, TENANTS)
            .route(
                "GET",
                "/projects/web/environments/web-ann-lee",
                vec![json(200, ANN_TUNNEL)],
            )
            .route("POST", ANN_SHARE, vec![json(200, ANN_TUNNEL)]);
        let cli = waiting_project(&stub);
        let (ran, came) = batch_until_connected(
            &cli,
            &mut events,
            &["--share"],
            1,
            "https://web-ann-lee.west.test",
        )
        .await;
        let shares = positions(&stub, "POST", ANN_SHARE);
        let upgrades = positions(&stub, "GET", "/tunnel");
        assert!(
            came && !shares.is_empty() && !upgrades.is_empty() && shares.first() < upgrades.first(),
            "trail {:?}: {ran:?}",
            stub.trail()
        );
    }

    /// Whether `lof dev --share`, the environment `web-dev` answering
    /// `environment`, posted no `share` before its first upgrade.
    async fn shares_nothing_before_upgrade_when_environment_is(environment: &str) -> bool {
        let (stub, mut events) =
            sharing_daemon(vec![json(200, environment)], vec![json(200, TUNNEL_SHARED)]).await;
        let cli = waiting_project(&stub);
        let (ran, came) =
            batch_until_connected(&cli, &mut events, &["--share"], 1, DEV_ADDRESS).await;
        let shares = positions(&stub, "POST", SHARE_PATH);
        let upgrades = positions(&stub, "GET", "/tunnel");
        let first = upgrades.first().copied();
        let none_before = shares.iter().all(|share| Some(*share) > first);
        assert!(came, "no tunnel connected: {:?}, {ran:?}", stub.trail());
        none_before
    }

    // TC: DEVT-10 (the guessed name is another user's tunnel: shared only
    // once the server named the caller's own)
    #[tokio::test(flavor = "multi_thread")]
    async fn shares_nothing_before_upgrade_when_tunnel_is_another_users() {
        assert!(shares_nothing_before_upgrade_when_environment_is(FOREIGN_TUNNEL).await);
    }

    // TC: DEVT-10 (the guessed name is no tunnel at all)
    #[tokio::test(flavor = "multi_thread")]
    async fn shares_nothing_before_upgrade_when_environment_is_no_tunnel() {
        assert!(shares_nothing_before_upgrade_when_environment_is(COMPOSE_WEB_DEV).await);
    }

    // TC: DEVT-10 (the server's `Hello` wins over the CLI's guess)
    #[tokio::test(flavor = "multi_thread")]
    async fn posts_share_of_environment_hello_names_when_guess_differs() {
        const OTHER_SHARE: &str = "/projects/web/environments/web-other/share";
        let (stub, mut events) = Stub::start_with_tunnel(TunnelOptions {
            environment: Some("web-other"),
            ..TunnelOptions::default()
        })
        .await;
        let stub = dev_routes(stub, ME, NO_MODULES)
            .route("GET", DEV_ENV_PATH, vec![json(200, TUNNEL_PRIVATE)])
            .route("POST", SHARE_PATH, vec![json(200, TUNNEL_SHARED)])
            .route("POST", OTHER_SHARE, vec![json(200, TUNNEL_SHARED)]);
        let cli = waiting_project(&stub);
        let (ran, came) = batch_until_connected(
            &cli,
            &mut events,
            &["--share"],
            1,
            "https://web-other.west.test",
        )
        .await;
        let shares = positions(&stub, "POST", OTHER_SHARE);
        let upgrades = positions(&stub, "GET", "/tunnel");
        assert!(
            came && shares.len() == 1 && shares.first() > upgrades.first(),
            "trail {:?}: {ran:?}",
            stub.trail()
        );
    }

    // TC: DEVT-10 (a refused share before the upgrade fails the run: no
    // tunnel, no dev command)
    #[tokio::test(flavor = "multi_thread")]
    async fn exits_1_without_tunnel_when_share_is_forbidden() {
        let (stub, _events) = sharing_daemon(
            vec![json(200, TUNNEL_PRIVATE)],
            vec![problem(403, "not allowed to share web-dev")],
        )
        .await;
        let cli = waiting_project(&stub);
        let ran = batch_without_tunnel(&cli, &["--share"]).await;
        assert_eq!(
            (
                ran.code,
                positions(&stub, "GET", "/tunnel").len(),
                dev_command_ran(&cli.root)
            ),
            (Some(1), 0, false),
            "trail {:?}: {ran:?}",
            stub.trail()
        );
    }

    // TC: DEVT-10 (a share after `Hello` that fails leaves the tunnel
    // private: its link is printed, and why)
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_login_link_and_stays_private_when_share_after_upgrade_fails() {
        let (stub, mut events) = sharing_daemon(
            vec![problem(404, "environment web-dev of web does not exist")],
            vec![problem(500, "internal error")],
        )
        .await;
        let cli = waiting_project(&stub);
        let (ran, came) =
            batch_until_connected(&cli, &mut events, &["--share"], 1, DEV_ADDRESS).await;
        assert!(came, "no tunnel connected: {:?}, {ran:?}", stub.trail());
        assert_eq!(
            (
                ran.code,
                count(&ran.out, LOGIN_URL),
                count(&ran.err, "stays private")
            ),
            (Some(0), 1, 1),
            "{ran:?}"
        );
    }
}

mod refused {
    use super::*;

    // TC: DEVT-09 (DEVT-06: the daemon refuses the session; the dev
    // command runs on locally, the refusal is told once)
    #[tokio::test(flavor = "multi_thread")]
    async fn tells_refusal_once_and_runs_on_when_session_is_refused() {
        let (stub, _events) = daemon(TunnelOptions::default()).await;
        let cli = waiting(Cli::logged_in_as(&stub.url, "lfs_expired"));
        let running = Running::start(&cli, &[]);
        let told = running.told("refused the tunnel").await;
        stop(&cli);
        let ran = running.finish().await;
        assert_eq!(
            (
                told,
                ran.code,
                count(&ran.err, "refused the tunnel"),
                dev_command_ran(&cli.root)
            ),
            (true, Some(0), 1, true),
            "{ran:?}"
        );
    }

    // TC: DEVT-09 (a second `lof dev` of the project evicts the first, which runs on locally)
    #[tokio::test(flavor = "multi_thread")]
    async fn tells_takeover_and_runs_on_when_another_lof_dev_takes_tunnel() {
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let (first, second) = (waiting_project(&stub), waiting_project(&stub));
        let older = Running::start(&first, &[]);
        let up = events.connected().await && older.printed(DEV_ADDRESS).await;
        let newer = Running::start(&second, &[]);
        let told = up && older.told("took over").await;
        stop(&first);
        stop(&second);
        let (older, newer) = (older.finish().await, newer.finish().await);
        assert_eq!(
            (up, told, older.code, count(&older.err, "took over")),
            (true, true, Some(0), 1),
            "{older:?}, {newer:?}"
        );
    }
}

mod forward {
    use super::*;

    /// What the dev command answers.
    const ANSWER: &str = "hello from the dev command";

    // TC: DEVT-09 (a request through the tunnel reaches the dev command's port)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_through_tunnel_from_dev_command_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let app = axum::Router::new().fallback(async || ANSWER);
        drop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let (stub, mut events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub);
        let running = Running::start(&cli, &["--port", &port]);
        let up = events.connected().await && running.printed(DEV_ADDRESS).await;
        let answer = if up {
            Some(stub.through_tunnel("web-dev", "/hello").await)
        } else {
            None
        };
        stop(&cli);
        let ran = running.finish().await;
        assert_eq!(answer, Some((200, ANSWER.to_owned())), "{ran:?}");
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

mod local {
    use super::*;

    // TC: DEVT-11 (`--local` opens no tunnel)
    #[tokio::test(flavor = "multi_thread")]
    async fn opens_no_tunnel_when_local_in_batch_mode() {
        let (stub, _events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub);
        let ran = batch_without_tunnel(&cli, &["--local"]).await;
        assert!(
            ran.code == Some(0) && positions(&stub, "GET", "/tunnel").is_empty(),
            "trail {:?}: {ran:?}",
            stub.trail()
        );
    }

    // TC: DEVT-11
    #[tokio::test(flavor = "multi_thread")]
    async fn opens_no_tunnel_when_local_in_interactive_mode() {
        let (stub, _events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub).terminal(vec![]);
        let outcome = interactive_without_tunnel(&cli, &["--local"]).await;
        assert!(
            outcome.code == 0 && positions(&stub, "GET", "/tunnel").is_empty(),
            "trail {:?}: {outcome:?}",
            stub.trail()
        );
    }

    // TC: DEVT-11
    #[tokio::test(flavor = "multi_thread")]
    async fn prints_no_dev_address_when_local() {
        let (stub, _events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub);
        let ran = batch_without_tunnel(&cli, &["--local"]).await;
        assert!(
            ran.code == Some(0) && !ran.out.contains(DEV_ADDRESS) && !ran.out.contains(LOGIN_URL),
            "{ran:?}"
        );
    }

    // TC: DEVT-11 (`--share` needs the tunnel `--local` turns off)
    #[tokio::test(flavor = "multi_thread")]
    async fn exits_64_when_local_and_share() {
        let (stub, _events) = daemon(TunnelOptions::default()).await;
        let cli = waiting_project(&stub);
        let ran = batch_without_tunnel(&cli, &["--local", "--share"]).await;
        assert!(
            ran.code == Some(64) && positions(&stub, "POST", SHARE_PATH).is_empty(),
            "trail {:?}: {ran:?}",
            stub.trail()
        );
    }
}

mod no_server {
    use super::*;

    /// The note of a run without a server (the earlier wording).
    const NOTE: &str = "without a tunnel";

    // TC: DEVT-12 (one warning, then the local run)
    #[tokio::test]
    async fn warns_once_when_no_server_in_batch_mode() {
        let cli = local_project();
        let ran = finish(spawn(&cli, &["--project", "demo", "dev"])).await;
        assert_eq!((ran.code, count(&ran.err, NOTE)), (Some(0), 1), "{ran:?}");
    }

    // TC: DEVT-12
    #[tokio::test]
    async fn warns_once_when_no_server_in_interactive_mode() {
        let cli = local_project().terminal(vec![]);
        let outcome = cli.run(&["--project", "demo", "dev"]).await;
        assert_eq!(
            (outcome.code, count(&outcome.err, NOTE)),
            (0, 1),
            "{outcome:?}"
        );
    }

    // TC: DEVT-12
    #[tokio::test]
    async fn runs_dev_command_when_no_server_in_batch_mode() {
        let cli = local_project();
        let ran = finish(spawn(&cli, &["--project", "demo", "dev"])).await;
        assert!(dev_command_ran(&cli.root), "{ran:?}");
    }

    // TC: DEVT-12 (`--share` without a server: the same single warning)
    #[tokio::test]
    async fn warns_once_when_no_server_and_share_in_batch_mode() {
        let cli = local_project();
        let ran = finish(spawn(&cli, &["--project", "demo", "dev", "--share"])).await;
        assert_eq!((ran.code, count(&ran.err, NOTE)), (Some(0), 1), "{ran:?}");
    }

    // TC: DEVT-12
    #[tokio::test]
    async fn warns_once_when_no_server_and_share_in_interactive_mode() {
        let cli = local_project().terminal(vec![]);
        let outcome = cli.run(&["--project", "demo", "dev", "--share"]).await;
        assert_eq!(
            (outcome.code, count(&outcome.err, NOTE)),
            (0, 1),
            "{outcome:?}"
        );
    }

    // TC: DEVT-12
    #[tokio::test]
    async fn runs_dev_command_when_no_server_and_share() {
        let cli = local_project();
        let ran = finish(spawn(&cli, &["--project", "demo", "dev", "--share"])).await;
        assert!(dev_command_ran(&cli.root), "{ran:?}");
    }
}
