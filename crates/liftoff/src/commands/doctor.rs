//! `doctor`: every check with its status, red ones ending the run with
//! exit code 1. It checks the config (and that it is private), the server (`/healthz`), the session (`/me`) and readiness
//! (`/readyz`: docker, `SQLite` and Caddy admin), then the rows
//! of `GET /server`: `docker`, `sqlite` and `caddy`
//! apart, and `dns`, whether a fresh label under the base domain
//! resolves to the server's public IP (a server older than that route is
//! a warning). Once local mode keeps a store in the config home,
//! `secrets` checks that its key is `0600` and the config home `0700`
//! (LCL-02): wider is a warning.

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt as _;

use liftoff_api::{ServerView, UserView};
use tokio_util::sync::CancellationToken;

use super::{Ctx, parse};
use crate::Sleeper;
use crate::client::Client;
use crate::config::{Chosen, Config};
use crate::error::CliError;
use crate::local::Home;
use crate::project::Dotfile;

/// How a check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    const fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone)]
struct Check {
    name: String,
    status: Status,
    detail: String,
}

fn check(name: impl Into<String>, status: Status, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status,
        detail: detail.into(),
    }
}

/// Runs `doctor`.
pub(super) async fn run<S: Sleeper>(ctx: &mut Ctx<'_, S>) -> Result<(), CliError> {
    let (config_check, chosen) = config(ctx);
    let mut checks = vec![config_check];
    match chosen {
        Ok(chosen) => checks.extend(server(&chosen, ctx.env.cancel.clone()).await),
        Err(reason) => checks.extend(all_failed(&reason)),
    }
    if let Some(local) = local_store(&ctx.home()) {
        checks.push(local);
    }
    report(ctx, &checks)?;
    let failed = checks
        .iter()
        .filter(|check| check.status == Status::Fail)
        .count();
    if failed == 0 {
        Ok(())
    } else {
        Err(CliError::ChecksFailed(format!(
            "{failed} of {} checks failed",
            checks.len()
        )))
    }
}

/// The `config` check and the server it picks (or why there is none).
fn config<S: Sleeper>(ctx: &Ctx<'_, S>) -> (Check, Result<Chosen, String>) {
    let path = &ctx.env.config_path;
    let dotfile = match Dotfile::find(&ctx.env.cwd) {
        Ok(dotfile) => dotfile,
        Err(error) => {
            let reason = error.to_string();
            return (check("config", Status::Fail, reason.clone()), Err(reason));
        }
    };
    let config = match Config::read(path) {
        Ok(Some(config)) => config,
        Ok(None) => {
            let reason = format!(
                "no config at {}; run `liftoff login <url> --token <t>`",
                path.display()
            );
            return (
                check("config", Status::Fail, reason),
                Err("no server".to_owned()),
            );
        }
        Err(error) => {
            return (
                check("config", Status::Fail, error.to_string()),
                Err("no server".to_owned()),
            );
        }
    };
    let wanted = ctx.flags.server.clone().or(dotfile.server);
    let chosen = match config.choose(wanted.as_deref()) {
        Ok(Some(chosen)) => Ok(chosen),
        Ok(None) => Err("the config names no server".to_owned()),
        Err(error) => Err(error.to_string()),
    };
    let mode = std::fs::metadata(path).map(|meta| meta.permissions().mode() & 0o777);
    let line = match mode {
        Ok(mode) if mode & 0o077 != 0 => check(
            "config",
            Status::Warn,
            format!(
                "{} is readable by others (mode {mode:o}); run chmod 600 on it",
                path.display()
            ),
        ),
        Ok(_) => check("config", Status::Ok, path.display().to_string()),
        Err(error) => check("config", Status::Fail, error.to_string()),
    };
    (line, chosen)
}

/// The `secrets` check of local mode: none before local mode made its
/// key.
fn local_store(home: &Home) -> Option<Check> {
    let key = home.key();
    let key_mode = match home.key_mode() {
        Ok(Some(mode)) => mode,
        Ok(None) => return None,
        Err(error) => {
            return Some(check(
                "secrets",
                Status::Fail,
                format!("{}: {error}", key.display()),
            ));
        }
    };
    if key_mode & 0o077 != 0 {
        return Some(check(
            "secrets",
            Status::Warn,
            format!(
                "{} is readable by others (mode {key_mode:o}); run chmod 600 on it",
                key.display()
            ),
        ));
    }
    match home.dir_mode() {
        Ok(Some(mode)) if mode & 0o077 != 0 => Some(check(
            "secrets",
            Status::Warn,
            format!(
                "{} is open to others (mode {mode:o}); run chmod 700 on it",
                home.dir().display()
            ),
        )),
        Ok(_) => Some(check("secrets", Status::Ok, key.display().to_string())),
        Err(error) => Some(check(
            "secrets",
            Status::Fail,
            format!("{}: {error}", home.dir().display()),
        )),
    }
}

/// The rows `GET /server` answers, in its order.
const SERVER_ROWS: [&str; 4] = ["docker", "sqlite", "caddy", "dns"];

/// Every server-side check failed for `reason`.
fn all_failed(reason: &str) -> Vec<Check> {
    ["server", "session", "ready"]
        .into_iter()
        .chain(SERVER_ROWS)
        .map(|name| check(name, Status::Fail, reason))
        .collect()
}

/// The `server`, `session` and `ready` checks against `chosen`, then the
/// rows of `GET /server`.
async fn server(chosen: &Chosen, cancel: CancellationToken) -> Vec<Check> {
    let url = &chosen.server.url;
    let open = match Client::new(url, None, cancel.clone()) {
        Ok(client) => client,
        Err(error) => return all_failed(&error.to_string()),
    };
    let server = match open.get("/healthz", &[]).await {
        Ok(_) => check("server", Status::Ok, format!("{} ({url})", chosen.name)),
        Err(error) => check("server", Status::Fail, format!("{url}: {error}")),
    };
    let session = chosen.server.token.as_ref().map_or_else(
        || Err(CliError::NotLoggedIn),
        |token| Client::new(url, Some(token.clone()), cancel),
    );
    let me = match &session {
        Err(error) => check("session", Status::Fail, error.to_string()),
        Ok(client) => match client.get("/me", &[]).await {
            Ok(reply) => match parse::<UserView>(&reply.body) {
                Ok(user) => check(
                    "session",
                    Status::Ok,
                    format!("{} ({})", user.login, user.role),
                ),
                Err(error) => check("session", Status::Fail, error.to_string()),
            },
            Err(error) => check("session", Status::Fail, error.to_string()),
        },
    };
    let ready = match open.get("/readyz", &[]).await {
        Ok(_) => check("ready", Status::Ok, "docker, sqlite and caddy admin answer"),
        Err(error) => check("ready", Status::Fail, error.to_string()),
    };
    let mut checks = vec![server, me, ready];
    checks.extend(match &session {
        Err(error) => rows_failed(&error.to_string()),
        Ok(client) => server_rows(client).await,
    });
    checks
}

/// Every row of `GET /server` with one status and detail.
fn rows_failed(reason: &str) -> Vec<Check> {
    SERVER_ROWS
        .into_iter()
        .map(|name| check(name, Status::Fail, reason))
        .collect()
}

/// The rows of `GET /server` as the server reports them; a warning on
/// each when the server predates the route (`404`).
async fn server_rows(client: &Client) -> Vec<Check> {
    let view = match client.get("/server", &[]).await {
        Ok(reply) => parse::<ServerView>(&reply.body),
        Err(error) => Err(error),
    };
    match view {
        Ok(view) => view
            .checks
            .into_iter()
            .map(|row| {
                let status = match row.status.as_str() {
                    "ok" => Status::Ok,
                    "warn" => Status::Warn,
                    _ => Status::Fail,
                };
                let detail = row.detail.unwrap_or_else(|| match row.name.as_str() {
                    "dns" => format!("*.{}", view.base_domain),
                    _ if status == Status::Ok => "answers".to_owned(),
                    _ => "does not answer; see the server's log".to_owned(),
                });
                check(row.name, status, detail)
            })
            .collect(),
        Err(error) if is_not_found(&error) => SERVER_ROWS
            .into_iter()
            .map(|name| {
                check(
                    name,
                    Status::Warn,
                    "the server does not report this check apart; upgrade it (`lof server upgrade`)",
                )
            })
            .collect(),
        Err(error) => rows_failed(&error.to_string()),
    }
}

/// Whether the API answered `404` (a route this server does not have).
fn is_not_found(error: &CliError) -> bool {
    match error {
        CliError::Api { body, .. } => serde_json::from_str::<serde_json::Value>(body)
            .is_ok_and(|problem| problem.get("status") == Some(&serde_json::json!(404))),
        _ => false,
    }
}

/// One line per check, `<check> <status> <detail>`; a JSON array with
/// `--json`.
fn report<S: Sleeper>(ctx: &mut Ctx<'_, S>, checks: &[Check]) -> Result<(), CliError> {
    let text = if ctx.json() {
        let rows: Vec<serde_json::Value> = checks
            .iter()
            .map(|check| {
                serde_json::json!({
                    "check": check.name,
                    "status": check.status.name(),
                    "detail": check.detail,
                })
            })
            .collect();
        let text = serde_json::to_string(&rows)
            .map_err(|error| CliError::Unexpected(error.to_string()))?;
        format!("{text}\n")
    } else {
        let mut text = String::new();
        for check in checks {
            writeln!(
                text,
                "{:<8}{:<6}{}",
                check.name,
                check.status.name(),
                check.detail
            )
            .map_err(|error| CliError::Local(error.to_string()))?;
        }
        text
    };
    ctx.say(&text)
}
