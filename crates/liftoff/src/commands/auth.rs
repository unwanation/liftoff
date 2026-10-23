//! `login`, `logout`, `whoami`.

use std::time::Duration;

use liftoff_api::{DeviceLogin, DevicePoll, SessionGranted, TokenExchange, UserView};
use reqwest::{Method, StatusCode};

use super::deploys::pause;
use super::{Ctx, parse};
use crate::client::{Client, refusal};
use crate::config::{Config, Server};
use crate::error::CliError;
use crate::{Sleeper, render};

/// The `oidc` module's device flow.
const DEVICE_START: &str = "/m/oidc/device";
/// Its poll.
const DEVICE_POLL: &str = "/m/oidc/device/poll";

/// `login <url> [--token <t>]`: with `--token`, trades the
/// single-use token for a session; without it, runs the device flow of
/// the server's `oidc` module (whose manifest replaces `login`), and
/// asks for the token when the server has none (`404`). The session is
/// saved under the server's name, the first server becoming the
/// default. Nothing is written unless the server agrees.
pub(super) async fn login<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    url: Option<String>,
    token: Option<String>,
) -> Result<(), CliError> {
    let url = ctx.text(url, "<url>", "Server URL")?;
    let url = url.trim().trim_end_matches('/').to_owned();
    let device = if token.is_none() && !url.is_empty() {
        check_url(&url)?;
        // A broken config fails before the person confirms anything.
        let config = Config::load(&ctx.env.config_path)?;
        let client = Client::new(&url, None, ctx.env.cancel.clone())?;
        device_login(ctx, &client)
            .await?
            .map(|granted| (config, granted))
    } else {
        None
    };
    let (mut config, granted) = if let Some(found) = device {
        ctx.finish()?;
        found
    } else {
        let token = ctx.secret(token, "--token", "Login token")?;
        ctx.finish()?;
        check_url(&url)?;
        // A broken config fails before the single-use token is spent.
        let config = Config::load(&ctx.env.config_path)?;
        let client = Client::new(&url, None, ctx.env.cancel.clone())?;
        let body = TokenExchange {
            token,
            name: ctx.env.machine.clone(),
        };
        let reply = client
            .send_json(Method::POST, "/auth/token", &[], &body)
            .await?;
        (config, parse::<SessionGranted>(&reply.body)?)
    };
    let name = config
        .name_of(&url)
        .unwrap_or_else(|| config.new_name(&url));
    // A new session keeps what `lof tenant use` wrote for the server.
    let tenant = config
        .servers
        .get(&name)
        .and_then(|server| server.tenant.clone());
    let _previous: Option<Server> = config.servers.insert(
        name.clone(),
        Server {
            url: url.clone(),
            token: Some(granted.token),
            tenant,
        },
    );
    let has_default = config
        .default_server
        .as_ref()
        .is_some_and(|default| config.servers.contains_key(default));
    if !has_default {
        config.default_server = Some(name.clone());
    }
    ctx.save_config(&config)?;
    if ctx.json() {
        // The session token is not printed, not even with `--json`.
        let user = serde_json::to_string(&granted.user)
            .map_err(|error| CliError::Unexpected(error.to_string()))?;
        return ctx.say(&format!("{user}\n"));
    }
    ctx.say(&format!(
        "logged in to {url} as {} (server {name})\n",
        granted.user.login
    ))
}

/// `<url>` must be an `http(s)` URL.
fn check_url(url: &str) -> Result<(), CliError> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        Err(CliError::Usage(format!(
            "<url> must start with https:// or http://, got {url:?}"
        )))
    }
}

/// The device flow (RFC 8628) through the server's `oidc` module: the
/// link and the code go to stderr, then the poll runs every `interval`
/// until it is no longer `202`. `None` when the server has no such module.
async fn device_login<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
) -> Result<Option<SessionGranted>, CliError> {
    let (status, body) = client
        .send_json_any(Method::POST, DEVICE_START, &serde_json::json!({}))
        .await?;
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(refusal(status, body));
    }
    let device: DeviceLogin = parse(&body)?;
    ctx.tell(&format!(
        "To log in, open {} and confirm the code {}\n",
        device.verification_uri_complete, device.user_code
    ))?;
    let poll = DevicePoll {
        device_code: device.device_code,
        name: ctx.env.machine.clone(),
    };
    let interval = Duration::from_secs(device.interval.max(1));
    let expires_in = Duration::from_secs(device.expires_in);
    let mut waited = Duration::ZERO;
    while waited < expires_in {
        pause(&ctx.env.sleeper, &ctx.env.cancel, interval).await?;
        waited = waited.saturating_add(interval);
        let (status, body) = client
            .send_json_any(Method::POST, DEVICE_POLL, &poll)
            .await?;
        if status == StatusCode::ACCEPTED {
            continue;
        }
        if !status.is_success() {
            return Err(refusal(status, body));
        }
        return parse::<SessionGranted>(&body).map(Some);
    }
    Err(CliError::Api {
        message: "device code expired".to_owned(),
        body: String::new(),
    })
}

/// Ends the session on the server and forgets the token.
pub(super) async fn logout<S: Sleeper>(ctx: &mut Ctx<'_, S>) -> Result<(), CliError> {
    let client = ctx.client()?;
    let result = client.delete("/auth/session", &[]).await;
    if let (Some(mut config), Some(chosen)) = (ctx.config.clone(), ctx.chosen.as_ref()) {
        if let Some(server) = config.servers.get_mut(&chosen.name) {
            server.token = None;
        }
        ctx.save_config(&config)?;
    }
    let reply = match result {
        Ok(reply) => reply,
        // An expired or revoked session is already logged out.
        Err(CliError::Unauthorized { .. }) => crate::client::Reply {
            body: String::new(),
        },
        Err(error) => return Err(error),
    };
    ctx.ack(&reply, "logged out")
}

/// Shows the caller.
pub(super) async fn whoami<S: Sleeper>(ctx: &mut Ctx<'_, S>) -> Result<(), CliError> {
    let client = ctx.client()?;
    let reply = client.get("/me", &[]).await?;
    ctx.show(&reply, |body| Ok(render::user(&parse::<UserView>(body)?)))
}
