//! The tunnel of `lof dev`.
//!
//! Before the dev command starts, [`prepare`] asks the server for its base
//! domain and, with `--share`, shares the tunnel environment: the CLI names
//! it as the daemon does (`liftoff_core::dev::tunnel_name`: `<project>-dev`,
//! `<project>-<login>` with the `tenants` module) and shares it only when
//! it is the caller's own tunnel (`kind` and `owner` of its `GET`). Before
//! the first `lof dev` of a project it does not exist (`404`); when it is
//! no tunnel of the caller's (the daemon will refuse the upgrade), or when
//! the server's `Hello` names another environment than the guess, the
//! share follows the `Hello` and goes to the environment it names. After the command started, [`Tunnel::start`] runs the
//! client of `liftoff-tunnel` (it reconnects with backoff by itself), and
//! [`Tunnel::on`] tells what happens to it: the dev address
//! `https://<env>.<base>` once, on the first connection, and the login
//! link of a private tunnel (`X-Liftoff-Login-Url` of that upgrade) once
//! too; a dropped connection, a refusal and an eviction go to stderr, and
//! the dev command runs on locally either way. [`Tunnel::shutdown`] sends
//! `Shutdown` (Ctrl-C), so the daemon takes the route down at once.

use liftoff_api::{EnvironmentView, ModuleView, ServerView, UserView};
use liftoff_core::dev::tunnel_name;
use liftoff_tunnel::{ClientConfig, ClientState, TunnelClient};
use reqwest::StatusCode;
use tokio::sync::watch;

use crate::Sleeper;
use crate::client::{Client, refusal};
use crate::commands::{Ctx, env_path, parse};
use crate::error::CliError;

/// The module whose users each get a tunnel of their own.
const TENANTS: &str = "tenants";

/// The `kind` of a tunnel environment.
const TUNNEL_KIND: &str = "tunnel";

/// Where `--share` is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Share {
    /// No `--share`: a private tunnel's login link is printed.
    No,
    /// The caller's tunnel of this name was shared before the upgrade.
    Done(String),
    /// The tunnel did not exist yet, or the guessed name is no tunnel
    /// of the caller's: shared once the server named it.
    AfterHello,
}

/// What the tunnel needs, gathered before the dev command starts.
#[derive(Debug)]
pub(super) struct Plan {
    config: ClientConfig,
    base: String,
    project: String,
    share: Share,
}

/// Asks the server for its base domain and, with `share`, shares the
/// tunnel environment of `project` before the upgrade; the client's
/// configuration for `server` (its URL) with the session `token` and the
/// dev command's `port`.
pub(super) async fn prepare(
    client: &Client,
    server: &str,
    token: &str,
    project: String,
    port: u16,
    share: bool,
) -> Result<Plan, CliError> {
    let url = tunnel_url(server).ok_or_else(|| {
        CliError::Config(format!(
            "the server URL {server} is no http:// or https:// URL: `lof dev` cannot open its tunnel"
        ))
    })?;
    let share = if share {
        share_before_upgrade(client, &project).await?
    } else {
        Share::No
    };
    let base = parse::<ServerView>(&client.get("/server", &[]).await?.body)?.base_domain;
    let config = ClientConfig::new(url, token.to_owned(), project.clone(), port);
    Ok(Plan {
        config,
        base,
        project,
        share,
    })
}

/// `ws://` or `wss://` of the server's `GET /api/v1/tunnel`.
fn tunnel_url(server: &str) -> Option<String> {
    let server = server.trim_end_matches('/');
    let (scheme, rest) = server
        .strip_prefix("https://")
        .map(|rest| ("wss", rest))
        .or_else(|| server.strip_prefix("http://").map(|rest| ("ws", rest)))?;
    Some(format!("{scheme}://{rest}/api/v1/tunnel"))
}

/// `POST .../share` of the tunnel `project` would have, if it is the
/// caller's own: shared; not there yet (`404`, the first `lof dev`) or
/// no tunnel of the caller's, left to the server's `Hello`.
async fn share_before_upgrade(client: &Client, project: &str) -> Result<Share, CliError> {
    let me = parse::<UserView>(&client.get("/me", &[]).await?.body)?;
    let modules = parse::<Vec<ModuleView>>(&client.get("/modules", &[]).await?.body)?;
    let tenants = modules
        .iter()
        .any(|module| module.name == TENANTS && module.enabled);
    let name = tunnel_name(project, tenants.then_some(me.login.as_str()));
    let path = env_path(project, &name);
    let (status, body) = client.get_any(&path).await?;
    if status == StatusCode::NOT_FOUND {
        return Ok(Share::AfterHello);
    }
    if !status.is_success() {
        return Err(refusal(status, body));
    }
    let found = parse::<EnvironmentView>(&body)?;
    if found.kind != TUNNEL_KIND || found.owner.as_deref() != Some(me.id.as_str()) {
        tracing::info!(environment = %name, "not the caller's tunnel: sharing after the server's Hello");
        return Ok(Share::AfterHello);
    }
    let (status, body) = client.post_any(&format!("{path}/share")).await?;
    if status.is_success() {
        Ok(Share::Done(name))
    } else if status == StatusCode::NOT_FOUND {
        Ok(Share::AfterHello)
    } else {
        Err(refusal(status, body))
    }
}

/// The running tunnel of a `lof dev`.
#[derive(Debug)]
pub(super) struct Tunnel {
    client: TunnelClient,
    /// The client's state; `None` once it ended (refused, evicted).
    states: Option<watch::Receiver<ClientState>>,
    base: String,
    project: String,
    share: Share,
    /// Whether the dev address was printed.
    announced: bool,
    /// Whether the client is connected.
    up: bool,
}

impl Tunnel {
    /// Starts the client of `plan`.
    pub(super) fn start(plan: Plan) -> Result<Self, CliError> {
        let client = TunnelClient::start(plan.config)
            .map_err(|error| CliError::Config(error.to_string()))?;
        let states = Some(client.state());
        Ok(Self {
            client,
            states,
            base: plan.base,
            project: plan.project,
            share: plan.share,
            announced: false,
            up: false,
        })
    }

    /// The next state of the client; pending for good once it ended.
    pub(super) async fn changed(&mut self) -> ClientState {
        loop {
            let Some(states) = self.states.as_mut() else {
                return std::future::pending().await;
            };
            if states.changed().await.is_ok() {
                return states.borrow_and_update().clone();
            }
            self.states = None;
        }
    }

    /// Tells what `state` means to the person: the dev address and the
    /// login link on the first connection, a drop, a return, a refusal
    /// or an eviction on stderr. `client` shares a tunnel that did not
    /// exist before its upgrade.
    pub(super) async fn on<S: Sleeper>(
        &mut self,
        state: ClientState,
        ctx: &mut Ctx<'_, S>,
        client: &Client,
    ) -> Result<(), CliError> {
        match state {
            ClientState::Connected {
                environment,
                login_url,
            } => {
                if self.announced && !self.up {
                    ctx.tell("the tunnel is back\n")?;
                }
                self.up = true;
                if self.announced {
                    return Ok(());
                }
                self.announced = true;
                ctx.say(&format!("https://{environment}.{}\n", self.base))?;
                let shared = match &self.share {
                    Share::No => false,
                    Share::Done(name) if *name == environment => true,
                    Share::Done(_) | Share::AfterHello => {
                        self.share_now(ctx, client, &environment).await?
                    }
                };
                if let (false, Some(url)) = (shared, login_url) {
                    ctx.say(&format!("{url}\n"))?;
                    ctx.tell(&format!(
                        "the dev address is private: the link above logs one browser in to it within 10 minutes (`lof env share --env {environment}` opens it to everyone)\n"
                    ))?;
                }
            }
            ClientState::Connecting => {
                if self.up {
                    ctx.tell("the tunnel connection broke; reconnecting\n")?;
                }
                self.up = false;
            }
            ClientState::Evicted => {
                self.up = false;
                ctx.tell("another `lof dev` of this project took over its tunnel: this one runs on locally\n")?;
            }
            ClientState::Refused { status } => {
                self.up = false;
                ctx.tell(&format!(
                    "the server refused the tunnel ({status}): log in again with `lof login`; the dev command runs on locally\n"
                ))?;
            }
            ClientState::Stopped => self.up = false,
        }
        Ok(())
    }

    /// `POST .../share` of `environment`, which the server just named;
    /// whether it is shared. A failure is told, and leaves it private.
    async fn share_now<S: Sleeper>(
        &mut self,
        ctx: &mut Ctx<'_, S>,
        client: &Client,
        environment: &str,
    ) -> Result<bool, CliError> {
        self.share = Share::Done(environment.to_owned());
        let path = format!("{}/share", env_path(&self.project, environment));
        match client.post(&path).await {
            Ok(_reply) => Ok(true),
            Err(CliError::Interrupted) => Err(CliError::Interrupted),
            Err(error) => {
                ctx.tell(&format!(
                    "cannot share {environment}: {error}; it stays private\n"
                ))?;
                Ok(false)
            }
        }
    }

    /// Sends `Shutdown` and stops the client (Ctrl-C, or the dev command
    /// ended).
    pub(super) async fn shutdown(self) {
        self.client.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod tunnel_url {
        use super::*;

        // TC: DEVT-09 (`wss://<base>/api/v1/tunnel`)
        #[test]
        fn is_wss_when_server_is_https() {
            assert_eq!(
                tunnel_url("https://api.example.com/"),
                Some("wss://api.example.com/api/v1/tunnel".to_owned())
            );
        }

        // TC: DEVT-09
        #[test]
        fn is_ws_when_server_is_http() {
            assert_eq!(
                tunnel_url("http://127.0.0.1:8080"),
                Some("ws://127.0.0.1:8080/api/v1/tunnel".to_owned())
            );
        }

        // TC: DEVT-09
        #[test]
        fn is_none_when_server_is_no_http_url() {
            assert_eq!(tunnel_url("ftp://example.com"), None);
        }
    }
}
