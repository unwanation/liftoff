//! `expose <PORT>`: a local port on the network, through the server.
//!
//! The CLI opens `wss://<api>/api/v1/tunnel` with its `cli` session, the
//! port in `X-Liftoff-Expose` and, with `--share`, `X-Liftoff-Share: 1`;
//! the daemon finds or creates the caller's tunnel of the port and
//! answers its address (`X-Liftoff-Address`): `<port>.localhost.<base>`
//! when the CLI runs on the daemon's own machine, else
//! `<login>-<id>.localhost.<base>`. No project is needed; the app on the
//! port runs however the person runs it (`npm run dev`), and its own
//! WebSocket (HMR) crosses the tunnel as it is.
//!
//! The CLI prints the address once, on the first connection, and for a
//! private tunnel the one-time login link (the server's single sign-on
//! with the `oidc` module); a dropped connection and its return go to
//! stderr, and the client reconnects by itself. A refusal of the server
//! (no session, not allowed, a port another user exposes) or an eviction
//! (another `lof expose` of the port) ends the run with 1; Ctrl-C,
//! `SIGTERM` and `SIGHUP` send `Shutdown`, so the daemon takes the
//! address down at once, and end it with 130.
//!
//! Without a server (local mode) there is nothing to expose through:
//! the run ends with 78 and the hint to connect one.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener};

use liftoff_api::ServerView;
use liftoff_tunnel::{ClientConfig, ClientState, TunnelClient};
use reqwest::StatusCode;
use rustix::io::Errno;
use tokio::signal::unix::SignalKind;
use tokio::sync::watch;

use super::group::watching;
use super::{Ctx, parse};
use crate::Sleeper;
use crate::args::ExposeArgs;
use crate::error::CliError;

/// Local mode has no server to expose a port through.
const NO_SERVER: &str = "no server: `lof expose` reaches the network through a liftoff server; connect one with `lof login <url>`";

/// Runs `expose`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    args: ExposeArgs,
) -> Result<(), CliError> {
    let Some(chosen) = ctx.chosen.as_ref() else {
        return Err(CliError::Config(NO_SERVER.to_owned()));
    };
    if args.port == 0 {
        return Err(CliError::Usage("the port is 1–65535".to_owned()));
    }
    let server = chosen.server.url.clone();
    let token = chosen.server.token.clone().ok_or(CliError::NotLoggedIn)?;
    let url = tunnel_url(&server).ok_or_else(|| {
        CliError::Config(format!(
            "the server URL {server} is no http:// or https:// URL: `lof expose` cannot open its tunnel"
        ))
    })?;
    // Asked first: a server that does not answer, or a session it does
    // not know, fails here and not in the tunnel's retries.
    let client = ctx.client()?;
    let base = parse::<ServerView>(&client.get("/server", &[]).await?.body)?.base_domain;
    if is_free(args.port) {
        ctx.tell(&format!(
            "nothing listens on port {} yet: the address answers 502 until something does\n",
            args.port
        ))?;
    }
    let mut config = ClientConfig::new(url, token, args.port);
    config.share = args.share;
    // The Ctrl-C watcher of the binary is a task of the same thread: let it
    // install its handler first.
    tokio::task::yield_now().await;
    let mut term = watching(SignalKind::terminate())?;
    let mut hangup = watching(SignalKind::hangup())?;
    let mut tunnel = Tunnel::start(config, base, args.share)?;
    let cancel = ctx.env.cancel.clone();
    let ended = loop {
        tokio::select! {
            () = cancel.cancelled() => break Err(CliError::Interrupted),
            _ = term.recv() => break Err(CliError::Interrupted),
            _ = hangup.recv() => break Err(CliError::Interrupted),
            state = tunnel.changed() => match tunnel.on(state, ctx) {
                Ok(None) => {}
                Ok(Some(end)) => break end,
                Err(error) => break Err(error),
            },
        }
    };
    tunnel.shutdown().await;
    ended
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

/// The running tunnel of a `lof expose`.
#[derive(Debug)]
struct Tunnel {
    client: TunnelClient,
    /// The client's state; `None` once it ended.
    states: Option<watch::Receiver<ClientState>>,
    base: String,
    share: bool,
    /// Whether the address was printed.
    announced: bool,
    /// Whether the client is connected.
    up: bool,
}

impl Tunnel {
    /// Starts the client of `config`; `base` names the address when the
    /// server's answer does not.
    fn start(config: ClientConfig, base: String, share: bool) -> Result<Self, CliError> {
        let client =
            TunnelClient::start(config).map_err(|error| CliError::Config(error.to_string()))?;
        let states = Some(client.state());
        Ok(Self {
            client,
            states,
            base,
            share,
            announced: false,
            up: false,
        })
    }

    /// The next state of the client; pending for good once it ended.
    async fn changed(&mut self) -> ClientState {
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

    /// Tells what `state` means to the person: the address and the login
    /// link on the first connection, a drop and a return on stderr. A
    /// refusal or an eviction ends the run: `Some` with its outcome.
    fn on<S: Sleeper>(
        &mut self,
        state: ClientState,
        ctx: &mut Ctx<'_, S>,
    ) -> Result<Option<Result<(), CliError>>, CliError> {
        match state {
            ClientState::Connected {
                environment,
                address,
                login_url,
            } => {
                if self.announced && !self.up {
                    ctx.tell("the tunnel is back\n")?;
                }
                self.up = true;
                if self.announced {
                    return Ok(None);
                }
                self.announced = true;
                let address = address
                    .unwrap_or_else(|| format!("https://{environment}.localhost.{}", self.base));
                ctx.say(&format!("{address}\n"))?;
                match login_url {
                    Some(url) if !self.share => {
                        ctx.say(&format!("{url}\n"))?;
                        ctx.tell(
                            "the address is private: the link above logs one browser in to it within 10 minutes (`--share` opens it to everyone)\n",
                        )?;
                    }
                    _ => {}
                }
                Ok(None)
            }
            ClientState::Connecting => {
                if self.up {
                    ctx.tell("the tunnel connection broke; reconnecting\n")?;
                }
                self.up = false;
                Ok(None)
            }
            ClientState::Evicted => Ok(Some(Err(CliError::Local(
                "another `lof expose` of this port took over its tunnel".to_owned(),
            )))),
            ClientState::Refused { status, detail } => Ok(Some(Err(refused(status, detail)))),
            ClientState::Stopped => Ok(Some(Ok(()))),
        }
    }

    /// Sends `Shutdown` and stops the client.
    async fn shutdown(self) {
        self.client.shutdown().await;
    }
}

/// The failure of a refused upgrade: a session to renew (`401`), else
/// what the server said.
fn refused(status: StatusCode, detail: Option<String>) -> CliError {
    let said = detail.as_deref().and_then(|body| {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()?
            .get("detail")?
            .as_str()
            .map(str::to_owned)
    });
    match (status, said) {
        (StatusCode::UNAUTHORIZED, _) => CliError::Unauthorized {
            message: "the server refused the session: log in again with `lof login`".to_owned(),
            body: detail.unwrap_or_default(),
        },
        (_, Some(said)) => CliError::Api {
            message: format!("the server refused the tunnel: {said}"),
            body: detail.unwrap_or_default(),
        },
        (_, None) => CliError::Api {
            message: format!("the server refused the tunnel ({status})"),
            body: detail.unwrap_or_default(),
        },
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

    mod refused {
        use super::*;

        // TC: DEVT-06
        #[test]
        fn log_in_again_when_unauthorized() {
            let error = refused(StatusCode::UNAUTHORIZED, None);
            assert!(error.to_string().contains("lof login"), "{error}");
        }

        // TC: EXP-06 (the server's reason, a port of another user)
        #[test]
        fn servers_detail_when_conflict() {
            let error = refused(
                StatusCode::CONFLICT,
                Some(
                    r#"{"title":"Conflict","detail":"3000 is exposed by another user"}"#.to_owned(),
                ),
            );
            assert_eq!(
                error.to_string(),
                "the server refused the tunnel: 3000 is exposed by another user"
            );
        }

        #[test]
        fn status_when_answer_has_no_detail() {
            let error = refused(StatusCode::FORBIDDEN, Some("nope".to_owned()));
            assert_eq!(
                error.to_string(),
                "the server refused the tunnel (403 Forbidden)"
            );
        }
    }
}
