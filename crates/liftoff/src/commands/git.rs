//! `git connect|ls|disconnect`: Forgejo by token; GitHub (and
//! GitLab) through their `git.*` module, by the forge's OAuth device flow
//! when the module has an OAuth application, else by token.

use std::time::Duration;

use liftoff_api::{
    ConnectionCreate, ConnectionView, GitDevicePending, GitDevicePoll, GitDeviceStart,
    GitDeviceView,
};
use reqwest::{Method, StatusCode};

use std::sync::Arc;

use liftoff_core::domain::{GitConnection, GitProvider, Host, ModuleName, SecretValue};
use liftoff_core::ports::{DeviceStart, Ports as _, Store as _};
use liftoff_core::service::{DevicePolled, GitService, InstallRequest};

use super::deploys::pause;
use super::{Ctx, parse, segment};
use crate::args::{GitCommand, Provider};
use crate::client::{Client, refusal};
use crate::error::CliError;
use crate::local::{LocalMode, LocalPorts, view};
use crate::{Sleeper, render};

/// The host of a provider that has one public forge.
const fn default_host(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Github => Some("github.com"),
        // A self-hosted GitLab is named with `--host`.
        Provider::Gitlab => Some("gitlab.com"),
        Provider::Forgejo => None,
    }
}

/// What a token of `provider` must be allowed, said before it is asked.
const fn token_hint(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Github => Some(
            "Create a GitHub token with the scopes repo and read:packages (https://github.com/settings/tokens).\n",
        ),
        Provider::Gitlab => Some(
            "Create a GitLab personal access token with the scope api (User settings, Access tokens).\n",
        ),
        Provider::Forgejo => None,
    }
}

/// Runs `git …`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: GitCommand,
) -> Result<(), CliError> {
    if ctx.is_local() {
        return local(ctx, command).await;
    }
    let client = ctx.client()?;
    match command {
        GitCommand::Connect {
            provider,
            host,
            token,
        } => connect(ctx, &client, provider, host, token).await,
        GitCommand::Ls => {
            ctx.finish()?;
            let reply = client.get("/git/connections", &[]).await?;
            ctx.show(&reply, |body| {
                Ok(render::connections(&parse::<Vec<ConnectionView>>(body)?))
            })
        }
        GitCommand::Disconnect { host } => {
            let host = ctx.text(host, "<host>", "Forge host")?;
            ctx.finish()?;
            let reply = client
                .delete(&format!("/git/connections/{}", segment(&host)), &[])
                .await?;
            ctx.ack(&reply, &format!("disconnected {host}"))
        }
    }
}

/// The forge and host of `git connect`: given, or asked (or missing in
/// batch mode); a provider with one public forge has its host by default.
fn forge<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    provider: Option<Provider>,
    host: Option<String>,
) -> Result<(Provider, String), CliError> {
    let provider = ctx.pick(
        provider,
        "<github|gitlab|forgejo>",
        "Forge",
        &[
            ("forgejo", Provider::Forgejo),
            ("github", Provider::Github),
            ("gitlab", Provider::Gitlab),
        ],
    )?;
    let host = match (host, default_host(provider)) {
        (Some(host), _) => host,
        (None, Some(host)) => host.to_owned(),
        (None, None) => ctx.text(None, "--host", "Forge host")?,
    };
    Ok((provider, host))
}

/// `git connect <provider> [--host H] [--token T]`.
///
/// A token given is sent as is. Without one, for a forge served by a
/// module (GitHub, GitLab), the device flow is started (`POST
/// /git/connections/{host}/device`, which `git.connect` admits, as it
/// does `POST /git/connections`): a module without an OAuth application
/// answers `400` (a server without the route `404`), and then the token
/// is asked (or missing in batch mode). Forgejo is always asked for a
/// token, with no request before it.
async fn connect<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
    provider: Option<Provider>,
    host: Option<String>,
    token: Option<String>,
) -> Result<(), CliError> {
    let (provider, host) = forge(ctx, provider, host)?;
    let served_by_module = provider != Provider::Forgejo;
    if token.is_none()
        && served_by_module
        && ctx.missing.is_empty()
        && let Some(device) = device_start(client, provider, &host).await?
    {
        ctx.finish()?;
        let connection = device_flow(ctx, client, provider, &host, device).await?;
        return ctx.show(&connection, |text| {
            let connection: ConnectionView = parse(text)?;
            Ok(connected(&connection))
        });
    }
    if token.is_none()
        && let Some(hint) = token_hint(provider)
    {
        ctx.tell(hint)?;
    }
    let token = ctx.secret(token, "--token", "Your access token on that forge")?;
    ctx.finish()?;
    let body = ConnectionCreate {
        provider: provider.name().to_owned(),
        host,
        token,
    };
    let reply = client
        .send_json(Method::POST, "/git/connections", &[], &body)
        .await?;
    ctx.show(&reply, |text| {
        let connection: ConnectionView = parse(text)?;
        Ok(connected(&connection))
    })
}

/// What `git connect` prints.
fn connected(connection: &ConnectionView) -> String {
    format!(
        "connected {} as {}\n",
        connection.host, connection.account_login
    )
}

/// The path of the device flow of `host`.
fn device_path(host: &str) -> String {
    format!("/git/connections/{}/device", segment(host))
}

/// Starts the device flow of the module serving `host` for `provider`:
/// `None` when it has no OAuth application (`400`) or the server knows
/// no device flow (`404`), so that the token is asked instead.
async fn device_start(
    client: &Client,
    provider: Provider,
    host: &str,
) -> Result<Option<GitDeviceView>, CliError> {
    let start = GitDeviceStart {
        provider: provider.name().to_owned(),
    };
    let (status, body) = client
        .send_json_any(Method::POST, &device_path(host), &start)
        .await?;
    if status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(refusal(status, body));
    }
    parse(&body).map(Some)
}

/// The pause after a `202` of the poll: `interval` unless the forge asked
/// to slow down (RFC 8628 3.5), then the interval it named, and at least
/// 5 seconds more than `interval`.
fn next_interval(interval: Duration, body: &str) -> Duration {
    let Ok(pending) = serde_json::from_str::<GitDevicePending>(body) else {
        return interval;
    };
    if pending.status != "slow_down" {
        return interval;
    }
    let raised = interval.saturating_add(SLOW_DOWN);
    pending
        .interval
        .map_or(raised, |asked| raised.max(Duration::from_secs(asked)))
}

/// What RFC 8628 3.5 adds to the interval on each `slow_down`.
const SLOW_DOWN: Duration = Duration::from_secs(5);

/// The device flow (RFC 8628) `device` started through the server's forge
/// module: the link and the code go to stderr, then the poll runs every
/// `interval` (raised on `slow_down`) until it is no longer `202`. The
/// answer of the last poll, the stored connection.
async fn device_flow<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
    provider: Provider,
    host: &str,
    device: GitDeviceView,
) -> Result<crate::client::Reply, CliError> {
    ctx.tell(&format!(
        "To connect {host}, open {} and enter the code {}\n",
        device.verification_uri, device.user_code
    ))?;
    let poll = GitDevicePoll {
        provider: provider.name().to_owned(),
        device_code: device.device_code,
    };
    let poll_path = format!("{}/poll", device_path(host));
    let mut interval = Duration::from_secs(device.interval.max(1));
    let expires_in = Duration::from_secs(device.expires_in);
    let mut waited = Duration::ZERO;
    while waited < expires_in {
        pause(&ctx.env.sleeper, &ctx.env.cancel, interval).await?;
        waited = waited.saturating_add(interval);
        let (status, body) = client
            .send_json_any(Method::POST, &poll_path, &poll)
            .await?;
        if status == StatusCode::ACCEPTED {
            interval = next_interval(interval, &body);
            continue;
        }
        if !status.is_success() {
            return Err(refusal(status, body));
        }
        return Ok(crate::client::Reply { body });
    }
    Err(CliError::Api {
        message: format!(
            "the device code expired: run `lof git connect {}` again",
            provider.name()
        ),
        body: String::new(),
    })
}

// ---- local mode -----------------------------------------------------------------

/// `git …` without a server (LCL-01): the core's
/// `GitService` over the local ports, the forge reached by its `git.*`
/// module in the CLI's own host, the connection kept in the local store
/// with its token sealed.
async fn local<S: Sleeper>(ctx: &mut Ctx<'_, S>, command: GitCommand) -> Result<(), CliError> {
    match command {
        GitCommand::Connect {
            provider,
            host,
            token,
        } => local_connect(ctx, provider, host, token).await,
        GitCommand::Ls => {
            ctx.finish()?;
            let local = ctx.local_mode().await?;
            let connections = GitService::new(Arc::clone(&local.ports))
                .list(&local.actor)
                .await?;
            let views: Vec<ConnectionView> = connections.iter().map(view::connection).collect();
            ctx.say_json_or(&views, |views| render::connections(views))
        }
        GitCommand::Disconnect { host } => {
            let host = ctx.text(host, "<host>", "Forge host")?;
            ctx.finish()?;
            let host = Host::new(&host).map_err(|error| CliError::Usage(error.to_string()))?;
            let local = ctx.local_mode().await?;
            GitService::new(Arc::clone(&local.ports))
                .disconnect(&local.actor, &host)
                .await?;
            ctx.say_json_or(&serde_json::json!({"host": host.as_str()}), |_| {
                format!("disconnected {host}\n")
            })
        }
    }
}

/// `git connect` without a server: the forge's `git.<provider>` module is
/// installed into the config home first when it is not, then
/// the device flow when the module has an OAuth application, else the
/// token, checked with one call to the forge.
async fn local_connect<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    provider: Option<Provider>,
    host: Option<String>,
    token: Option<String>,
) -> Result<(), CliError> {
    let (provider, host) = forge(ctx, provider, host)?;
    if !ctx.missing.is_empty() {
        let _token = ctx.secret(token, "--token", "Your access token on that forge")?;
        return ctx.finish();
    }
    let git_provider =
        GitProvider::parse(provider.name()).map_err(|error| CliError::Usage(error.to_string()))?;
    let host = Host::new(&host).map_err(|error| CliError::Usage(error.to_string()))?;
    let local = ctx.local_mode().await?;
    let module = ModuleName::new(&format!("git.{}", provider.name()))
        .map_err(|error| CliError::Local(error.to_string()))?;
    let modules = local.modules();
    if local.ports.store().module(&module).await?.is_none() {
        let installed = modules
            .install(
                &local.actor,
                InstallRequest {
                    name: module.clone(),
                    version: None,
                    config: serde_json::Map::new(),
                },
            )
            .await?;
        ctx.tell(&format!(
            "installed {} {} into {}\n",
            installed.installed.name,
            installed.installed.version,
            ctx.home().dir().join("modules").display()
        ))?;
    }
    let git = GitService::new(Arc::clone(&local.ports));
    if token.is_none() {
        match git
            .device_start(&local.actor, git_provider, host.clone())
            .await
        {
            Ok(device) => {
                let connection =
                    local_device_flow(ctx, &git, &local, git_provider, &host, device).await?;
                return ctx.say_json_or(&view::connection(&connection), connected);
            }
            Err(liftoff_core::Error::Validation(_)) => {}
            Err(other) => return Err(other.into()),
        }
    }
    if token.is_none()
        && let Some(hint) = token_hint(provider)
    {
        ctx.tell(hint)?;
    }
    let token = ctx.secret(token, "--token", "Your access token on that forge")?;
    ctx.finish()?;
    let connection = git
        .connect(&local.actor, git_provider, host, SecretValue::new(token))
        .await?;
    ctx.say_json_or(&view::connection(&connection), connected)
}

/// The device flow of local mode: the link and the code on stderr, then a
/// poll every interval (raised on `slow_down`) until the person confirms.
async fn local_device_flow<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    git: &GitService<LocalPorts>,
    local: &LocalMode,
    provider: GitProvider,
    host: &Host,
    device: DeviceStart,
) -> Result<GitConnection, CliError> {
    ctx.tell(&format!(
        "To connect {host}, open {} and enter the code {}\n",
        device.verification_uri, device.user_code
    ))?;
    let mut interval = Duration::from_secs(device.interval.max(1));
    let expires_in = Duration::from_secs(device.expires_in);
    let mut waited = Duration::ZERO;
    while waited < expires_in {
        pause(&ctx.env.sleeper, &ctx.env.cancel, interval).await?;
        waited = waited.saturating_add(interval);
        match git
            .device_poll(&local.actor, provider, host.clone(), &device.device_code)
            .await?
        {
            DevicePolled::Pending => {}
            DevicePolled::SlowDown { interval: asked } => {
                let raised = interval.saturating_add(SLOW_DOWN);
                interval = asked.map_or(raised, |asked| raised.max(Duration::from_secs(asked)));
            }
            DevicePolled::Connected(connection) => return Ok(connection),
        }
    }
    Err(CliError::Local(format!(
        "the device code expired: run `lof git connect {}` again",
        provider.as_str()
    )))
}
