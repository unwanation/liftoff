//! MCP over HTTP in the daemon (8.3).
//!
//! `liftoff-mcp`'s Streamable HTTP transport is mounted at
//! `POST|GET|DELETE /mcp` of the API host, beside the REST router (which
//! serves `GET /.well-known/oauth-protected-resource`).
//!
//! Its [`CoreBackend`] asks the core's
//! [`McpAuthService::verify_oauth`] for the `mcp` session of the
//! request's bearer, and the transport's tools then call the REST API at
//! [`McpMount::api_url`] (the daemon's own loopback listener) with that
//! session, so the token's user is the actor of every tool. Composed here
//! because it joins two adapters (`liftoff-rest` and `liftoff-mcp`).

use core::time::Duration;

use liftoff_core::Error;
use liftoff_core::ports::Ports;
use liftoff_core::service::McpAuthService;
use liftoff_mcp::Session;
use liftoff_mcp::http::{Backend, Grant, HttpConfig, Refusal, StreamableHttp};
use tokio_util::sync::CancellationToken;

/// The most transport sessions `/mcp` holds at once.
const MAX_SESSIONS: usize = 1024;

/// The most transport sessions `/mcp` holds at once for one user.
const MAX_SESSIONS_PER_USER: usize = 16;

/// How long a transport session nobody uses is kept.
const SESSION_IDLE: Duration = Duration::from_mins(5);

/// Where the transport is mounted and whom its tools call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpMount {
    /// The API the tools call, without `/api/v1`: the daemon's REST
    /// listener on loopback (`http://127.0.0.1:7000`).
    pub api_url: String,
    /// `[api] public_url` (`https://liftoff.example.com`): the
    /// `resource_metadata` of a `401` is
    /// `<public_url>/.well-known/oauth-protected-resource`, and its host is
    /// the one `Host` the transport answers.
    pub public_url: String,
}

/// The transport's [`Backend`] over the core.
#[derive(Debug)]
pub struct CoreBackend<P> {
    auth: McpAuthService<P>,
    api_url: String,
}

impl<P: Ports> CoreBackend<P> {
    /// The backend verifying bearers with `auth`, its tools calling
    /// `api_url`.
    #[must_use]
    pub const fn new(auth: McpAuthService<P>, api_url: String) -> Self {
        Self { auth, api_url }
    }
}

impl<P: Ports + 'static> Backend for CoreBackend<P> {
    async fn session(&self, bearer: Option<&str>) -> Result<Grant, Refusal> {
        match self.auth.verify_oauth(bearer).await {
            Ok(grant) => Ok(Grant {
                session: Session {
                    url: self.api_url.clone(),
                    token: grant.token.expose().to_owned(),
                },
                principal: grant.user.id.to_string(),
                valid_for: Duration::try_from(grant.valid_for).unwrap_or_default(),
            }),
            Err(error) => Err(refusal(error)),
        }
    }
}

/// The core's refusal as the transport answers it; an internal cause
/// goes to the log alone.
fn refusal(error: Error) -> Refusal {
    match error {
        Error::ModuleRequired(module) => Refusal::ModuleRequired(module),
        Error::Unauthenticated(why) => Refusal::Unauthenticated(why),
        refused @ (Error::Conflict(_) | Error::Forbidden(_) | Error::Validation(_)) => {
            Refusal::Forbidden(refused.to_string())
        }
        Error::Unavailable(why) => Refusal::Unavailable(why),
        other => {
            tracing::error!(error = %other, "checking an access token of /mcp failed");
            Refusal::Unavailable("internal error".to_owned())
        }
    }
}

/// The host of `public_url` (`liftoff.example.com`, with its port when
/// it names one).
fn authority(public_url: &str) -> String {
    let rest = public_url
        .split_once("://")
        .map_or(public_url, |(_scheme, rest)| rest);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// `app` with the transport at `POST|GET|DELETE /mcp`, its bearers
/// verified by `auth` (see the [module documentation](self)); its open
/// event streams end when `stop` is cancelled.
pub fn mount<P: Ports + 'static>(
    app: axum::Router,
    auth: McpAuthService<P>,
    mount: &McpMount,
    stop: &CancellationToken,
) -> axum::Router {
    let public_url = mount.public_url.trim_end_matches('/');
    let transport = StreamableHttp::new(
        CoreBackend::new(auth, mount.api_url.clone()),
        HttpConfig {
            resource_metadata: format!("{public_url}/.well-known/oauth-protected-resource"),
            allowed_hosts: vec![authority(public_url)],
            max_sessions: MAX_SESSIONS,
            max_sessions_per_principal: MAX_SESSIONS_PER_USER,
            session_idle: SESSION_IDLE,
            shutdown: stop.child_token(),
        },
    );
    app.route_service("/mcp", transport)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod refusal {
        use super::*;

        // TC: MCPH-03 (an internal cause never reaches the client: the
        // transport answers 503 with a fixed reason)
        #[test]
        fn unavailable_without_cause_when_internal_error() {
            assert_eq!(
                refusal(Error::internal_msg("disk /var/lib/liftoff is full")),
                Refusal::Unavailable("internal error".to_owned())
            );
        }

        // TC: MCPH-04 (another user holds the login: 403 with the core's
        // reason, as the REST API answers a conflict)
        #[test]
        fn forbidden_when_login_conflicts() {
            assert!(matches!(
                refusal(Error::conflict("login bob is taken")),
                Refusal::Forbidden(_)
            ));
        }

        // TC: MCPH-03
        #[test]
        fn unauthenticated_when_token_refused() {
            assert_eq!(
                refusal(Error::unauthenticated("expired")),
                Refusal::Unauthenticated("expired".to_owned())
            );
        }
    }

    mod authority {
        use super::*;

        #[test]
        fn host_when_public_url_has_no_port() {
            assert_eq!(
                authority("https://liftoff.example.com/"),
                "liftoff.example.com"
            );
        }

        #[test]
        fn host_and_port_when_public_url_names_port() {
            assert_eq!(authority("http://127.0.0.1:7000"), "127.0.0.1:7000");
        }
    }
}
