//! `dev` run locally:
//! `[dev] command` of `liftoff.toml`, else what stack detection proposes,
//! run with `sh -c` in the top of the working tree, with `PORT` (a free
//! port of 4000–4999 by `liftoff_core::dev::free_port`, or `--port`) and
//! the secrets (`GET /secrets/resolve` with a server, else the local
//! store) in its environment, never in its arguments; `--port N` is
//! appended for the frameworks that ignore `PORT`
//! (`liftoff_core::dev::with_port`). A command that names `--port N`
//! itself runs on N: `PORT` and the URL follow it
//! (`liftoff_core::dev::named_port`), and a `--port` of the CLI that
//! differs is a usage error. In local mode the system variables
//! `LIFTOFF_PROJECT`, `LIFTOFF_ENV` and an empty `LIFTOFF_DOMAIN` go on
//! top of the secrets, as the server adds them. The CLI prints
//! `http://localhost:<PORT>`; the child's stdout and stderr pass through.
//! Without a command the run ends with 64 and the hint to write `[dev]`.
//!
//! The command is the one string a shell runs: it comes from the person's
//! own `liftoff.toml` (or their `package.json`), never from a server, and
//! is written as a shell command (`npm run dev`, `cd web && bun dev`).
//! Every other process the CLI starts gets an argument vector.
//!
//! The child leads its own process group, so Ctrl-C (the CLI's `SIGINT`),
//! and `SIGTERM` or `SIGHUP` of the CLI (`kill`, the terminal closed),
//! end the whole group: `SIGTERM`, then `SIGKILL` for what of the group is
//! left after a grace period; the run ends with 130. Its stdin is closed: a process
//! outside the terminal's foreground group that read the terminal would
//! be stopped.
//!
//! With a server and no `--local`, the tunnel (steps 2–4; [`tunnel`]) opens once the command runs: the CLI prints the
//! dev address and the login link of a private tunnel once, reconnects
//! after a drop, and on Ctrl-C sends `Shutdown` before it ends the
//! command's group. `--share` shares the tunnel first; `--local --share`
//! is a usage error. Without a server the CLI says once that there is no
//! tunnel (`--share` changes nothing) and runs the command locally.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::signal::unix::{Signal as Watch, SignalKind, signal as watch};

use liftoff_api::{EnvVar, ResolvedEnv};
use liftoff_core::detect::{DirTree, StackDetector};
use liftoff_core::dev::{DEV_PORTS, Framework, free_port, named_port, with_port};
use liftoff_core::manifest::ProjectManifest;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use tokio::process::{Child, Command};

use self::tunnel::Tunnel;
use super::{Ctx, ProjectRef, parse, resolve, secrets};
use crate::Sleeper;
use crate::args::DevArgs;
use crate::client::Client;
use crate::error::CliError;
use crate::project::work_tree_root;

mod tunnel;

/// The manifest of a project.
const MANIFEST: &str = "liftoff.toml";

/// Neither `[dev] command` nor stack detection tells what to run (DEV-05).
const NO_COMMAND: &str = "nothing to run: liftoff.toml has no `[dev] command` and the files here do not tell how the project runs locally; write it into liftoff.toml, e.g.\n\n[dev]\ncommand = \"npm run dev\"\n\nand run `lof dev` again";

/// How long the dev command's group has to end after `SIGTERM` before
/// `SIGKILL`.
const GRACE: Duration = Duration::from_secs(5);

/// How often [`stop`] looks whether the group has ended.
const POLL: Duration = Duration::from_millis(50);

/// The variables the server adds on top of the secrets.
const SYSTEM: [&str; 3] = ["LIFTOFF_PROJECT", "LIFTOFF_ENV", "LIFTOFF_DOMAIN"];

/// Runs `dev`.
pub(super) async fn run<S: Sleeper>(ctx: &mut Ctx<'_, S>, args: DevArgs) -> Result<(), CliError> {
    let root = work_tree_root(&ctx.env.cwd);
    let command = dev_command(&root)?;
    let project = ctx
        .flags
        .project
        .clone()
        .map(ProjectRef::Named)
        .or_else(|| ctx.repository().map(ProjectRef::Repo));
    if !args.local && ctx.is_local() {
        ctx.tell("no server: `lof dev` runs locally, without a tunnel (`lof server connect` gives it one)\n")?;
    }
    let (vars, server) = secrets_of(ctx, project, !args.local).await?;
    let port = port_for(&command, args.port)?;
    let line = with_port(&command, Framework::of(&DirTree::new(&root)), port);
    let plan = match &server {
        Some(Server {
            client,
            url,
            token,
            project: Some(project),
        }) => Some(tunnel::prepare(client, url, token, project.clone(), port, args.share).await?),
        _ => None,
    };
    ctx.say(&format!("http://localhost:{port}\n"))?;
    // The Ctrl-C watcher of the binary is a task of the same thread: let it
    // install its handler before there is a child to leave behind.
    tokio::task::yield_now().await;
    // `SIGTERM` and `SIGHUP` would end the CLI and leave the group, which
    // the terminal's hangup no longer reaches, running: watched from here
    // on, before there is a child.
    let mut term = watching(SignalKind::terminate())?;
    let mut hangup = watching(SignalKind::hangup())?;
    let mut child = spawn(&line, &root, &vars, port)?;
    let mut tunnel = plan.map(Tunnel::start).transpose()?;
    let cancel = ctx.env.cancel.clone();
    let status = loop {
        let state = async {
            match tunnel.as_mut() {
                Some(tunnel) => tunnel.changed().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            status = child.wait() => break Ok(Some(status)),
            () = cancel.cancelled() => break Ok(None),
            _ = term.recv() => break Ok(None),
            _ = hangup.recv() => break Ok(None),
            state = state => {
                if let (Some(running), Some(server)) = (tunnel.as_mut(), &server)
                    && let Err(error) = running.on(state, ctx, &server.client).await
                {
                    break Err(error);
                }
            }
        }
    };
    // `Shutdown` first: the daemon takes the dev address down at once,
    // then the command ends.
    if let Some(tunnel) = tunnel {
        tunnel.shutdown().await;
    }
    let status = match status {
        Ok(Some(status)) => status,
        Ok(None) => {
            stop(&mut child).await;
            return Err(CliError::Interrupted);
        }
        Err(error) => {
            stop(&mut child).await;
            return Err(error);
        }
    };
    let status =
        status.map_err(|error| CliError::Local(format!("cannot wait for `{line}`: {error}")))?;
    ended(status, &line)
}

/// Starts `line` with `sh -c` in `root`, in a process group of its own,
/// with `vars` and `PORT` in its environment and its stdin closed.
fn spawn(line: &str, root: &Path, vars: &[EnvVar], port: u16) -> Result<Child, CliError> {
    Command::new("sh")
        .arg("-c")
        .arg(line)
        .current_dir(root)
        .envs(vars.iter().map(|var| (&var.key, &var.value)))
        .env("PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| CliError::Local(format!("cannot start `{line}`: {error}")))
}

/// The port `command` runs on: the one it names, else `--port`, else a
/// free one of [`DEV_PORTS`]; a `--port` other than the named one is a
/// usage error.
fn port_for(command: &str, flag: Option<u16>) -> Result<u16, CliError> {
    match (flag, named_port(command)) {
        (Some(flag), Some(named)) if flag != named => Err(CliError::Usage(format!(
            "the dev command `{command}` runs on port {named}, not on --port {flag}: drop one of them"
        ))),
        (_, Some(port)) | (Some(port), None) => Ok(port),
        (None, None) => free_port(is_free).ok_or_else(|| {
            CliError::Local(format!(
                "every port of {}–{} is busy: name one with `--port N`",
                DEV_PORTS.start(),
                DEV_PORTS.end()
            ))
        }),
    }
}

/// `[dev] command` of the manifest at `root`, else the one stack detection
/// proposes; neither is a usage error with the hint to write `[dev]`.
fn dev_command(root: &Path) -> Result<String, CliError> {
    let committed = match std::fs::read_to_string(root.join(MANIFEST)) {
        Ok(text) => Some(
            ProjectManifest::parse(&text)
                .map_err(|error| CliError::Usage(format!("{MANIFEST}: {error}")))?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(CliError::Local(format!("cannot read {MANIFEST}: {error}"))),
    };
    committed
        .and_then(|manifest| manifest.dev)
        .or_else(|| StackDetector::detect(&DirTree::new(root)).dev)
        .map(|dev| dev.command)
        .filter(|command| !command.trim().is_empty())
        .ok_or_else(|| CliError::Usage(NO_COMMAND.to_owned()))
}

/// The server of a `lof dev` with one: its client, URL and session, and
/// the project as it resolved, if there is one.
struct Server {
    client: Client,
    url: String,
    token: String,
    project: Option<String>,
}

/// The secrets the project's environment runs with: as the server resolves
/// them, else from the local store; none without a project. With a
/// server, the server too: the tunnel's when `tunnel` (no `--local`).
async fn secrets_of<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    project: Option<ProjectRef>,
    tunnel: bool,
) -> Result<(Vec<EnvVar>, Option<Server>), CliError> {
    let Some(project) = project else {
        let what = if tunnel && !ctx.is_local() {
            "without secrets and without a tunnel"
        } else {
            "without secrets"
        };
        ctx.tell(&format!("no project here (no `--project`, no `origin` remote on a forge): the dev command runs {what}\n"))?;
        return Ok((Vec::new(), None));
    };
    let env = ctx.env_name();
    let Some(chosen) = ctx.chosen.as_ref() else {
        let mut vars = secrets::local_resolve(&ctx.home(), &project, &env)?.vars;
        vars.retain(|var| !SYSTEM.contains(&var.key.as_str()));
        let name = secrets::local_project_name(&project)?;
        for (key, value) in SYSTEM
            .into_iter()
            .zip([name.to_string(), env, String::new()])
        {
            vars.push(EnvVar {
                key: key.to_owned(),
                value,
            });
        }
        return Ok((vars, None));
    };
    let url = chosen.server.url.clone();
    let token = chosen.server.token.clone().ok_or(CliError::NotLoggedIn)?;
    let client = ctx.client()?;
    let project = resolve(&client, project).await?;
    let query = [("project", project.clone()), ("env", env)];
    let reply = client.get("/secrets/resolve", &query).await?;
    let vars = parse::<ResolvedEnv>(&reply.body)?.vars;
    let server = Server {
        client,
        url,
        token,
        project: tunnel.then_some(project),
    };
    Ok((vars, Some(server)))
}

/// Starts watching the signal `kind`.
pub(super) fn watching(kind: SignalKind) -> Result<Watch, CliError> {
    watch(kind).map_err(|error| CliError::Local(format!("cannot watch signals: {error}")))
}

/// Whether nothing listens on `port`, on loopback or on every address, of
/// IPv4 and of IPv6 (a dev server on `localhost` may take `[::1]` only); an
/// IPv6 address the host does not have counts as free.
fn is_free(port: u16) -> bool {
    let v4 = [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED].map(IpAddr::V4);
    let v6 = [Ipv6Addr::LOCALHOST, Ipv6Addr::UNSPECIFIED].map(IpAddr::V6);
    let absent = |error: &std::io::Error| {
        error.raw_os_error().is_some_and(|code| {
            [Errno::ADDRNOTAVAIL, Errno::AFNOSUPPORT]
                .iter()
                .any(|absent| absent.raw_os_error() == code)
        })
    };
    v4.into_iter()
        .all(|address| TcpListener::bind((address, port)).is_ok())
        && v6.into_iter().all(|address| {
            TcpListener::bind((address, port)).map_or_else(|error| absent(&error), |_| true)
        })
}

/// Ends the dev command (or a function's build) and its process group:
/// `SIGTERM`, then `SIGKILL` for what of the group outlives [`GRACE`] (a
/// grandchild that ignores `SIGTERM`, or one still cleaning up). The grace period is the group's:
/// `sh` itself ends at once, the dev server it started may not.
pub(super) async fn stop(child: &mut Child) {
    stop_group(group_of(child), child).await;
}

/// The process group `child` leads (it was spawned with
/// `process_group(0)`); `None` once it has been waited for, so a caller
/// that waits for it first takes the group before.
pub(super) fn group_of(child: &Child) -> Option<Pid> {
    child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
}

/// [`stop`] for the group `group` that `child` led, also after `child`
/// itself has ended.
pub(super) async fn stop_group(group: Option<Pid>, child: &mut Child) {
    if let Some(group) = group {
        signal(group, Signal::TERM);
        let ended = async {
            while group_runs(group, child) {
                tokio::time::sleep(POLL).await;
            }
        };
        let outlived = tokio::time::timeout(GRACE, ended).await.is_err();
        if outlived && group_runs(group, child) {
            signal(group, Signal::KILL);
        }
    }
    if let Err(error) = child.kill().await {
        tracing::debug!(%error, "the dev command is already gone");
    }
}

/// Whether a process of the group `group` is left. Reaps `child` first:
/// the zombie of the shell would count as one.
fn group_runs(group: Pid, child: &mut Child) -> bool {
    if let Err(error) = child.try_wait() {
        tracing::debug!(%error, "cannot reap the dev command");
    }
    test_kill_process_group(group).is_ok()
}

/// Sends `signal` to the process group `group`; a group that is already
/// gone is fine.
fn signal(group: Pid, signal: Signal) {
    if let Err(error) = kill_process_group(group, signal) {
        tracing::debug!(%error, ?signal, "the dev command's group is gone");
    }
}

/// The outcome of the dev command: a failure unless it exited with 0.
fn ended(status: ExitStatus, line: &str) -> Result<(), CliError> {
    if status.success() {
        return Ok(());
    }
    Err(CliError::Local(status.code().map_or_else(
        || format!("`{line}` ended with {status}"),
        |code| format!("`{line}` ended with exit code {code}"),
    )))
}
