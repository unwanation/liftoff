//! `admin token|users|sessions revoke|reconcile|env set`, and the commands
//! of modules below `admin` (`admin tenant set`).

use liftoff_api::{EnvironmentPatch, SetupTokenGranted, UserView};
use reqwest::Method;

use super::{Ctx, env_path, parse};
use crate::args::{AdminCommand, AdminEnvCommand, SessionsCommand};
use crate::error::CliError;
use crate::{Sleeper, render};

/// The words of a module's command below `admin` (`tenant set` →
/// `admin tenant set`).
pub(super) fn below_admin(words: Vec<String>) -> Vec<String> {
    core::iter::once("admin".to_owned()).chain(words).collect()
}

/// Runs `admin …`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: AdminCommand,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    match command {
        AdminCommand::External(words) => crate::dynamic::run(ctx, below_admin(words)).await,
        AdminCommand::Token => {
            ctx.finish()?;
            let reply = client.post("/admin/token").await?;
            ctx.show(&reply, |body| {
                let granted: SetupTokenGranted = parse(body)?;
                Ok(format!(
                    "{}\nsingle use: run `lof login <url> --token <t>` with it on the new machine\n",
                    granted.token
                ))
            })
        }
        AdminCommand::Users => {
            ctx.finish()?;
            let reply = client.get("/admin/users", &[]).await?;
            ctx.show(&reply, |body| {
                Ok(render::users(&parse::<Vec<UserView>>(body)?))
            })
        }
        AdminCommand::Sessions {
            command: SessionsCommand::Revoke { login },
        } => {
            let login = ctx.text(login, "<login>", "Whose sessions end")?;
            ctx.finish()?;
            let reply = client
                .delete("/admin/sessions", &[("login", login.clone())])
                .await?;
            ctx.ack(&reply, &format!("revoked every session of {login}"))
        }
        AdminCommand::Reconcile => {
            ctx.finish()?;
            let reply = client.post("/admin/reconcile").await?;
            ctx.ack(&reply, "reconciled")
        }
        AdminCommand::Env {
            command:
                AdminEnvCommand::Set {
                    target,
                    cpus,
                    memory,
                },
        } => {
            let target = ctx.text(target, "<project/env>", "Environment (project/env)")?;
            ctx.finish()?;
            let (project, env) = target
                .split_once('/')
                .filter(|(project, env)| !project.is_empty() && !env.is_empty())
                .ok_or_else(|| {
                    CliError::Usage(format!("expected <project/env>, got {target:?}"))
                })?;
            // Only what is given: an absent field keeps its value.
            let body = EnvironmentPatch {
                branch: None,
                cpus,
                memory_mb: memory,
            };
            let reply = client
                .send_json(Method::PATCH, &env_path(project, env), &[], &body)
                .await?;
            ctx.ack(&reply, &format!("updated {project}/{env}"))
        }
    }
}
