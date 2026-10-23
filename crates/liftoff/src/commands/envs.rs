//! `status`, `env add|ls|rm|stop|start|share|unshare`, `bind`, `unbind`.

use liftoff_api::{BindingCreate, BindingView, EnvironmentCreate, EnvironmentView};
use reqwest::Method;

use super::{Ctx, env_path, parse, resolve, segment};
use crate::args::EnvCommand;
use crate::error::CliError;
use crate::{Sleeper, render};

/// Runs `env …`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: EnvCommand,
) -> Result<(), CliError> {
    match command {
        EnvCommand::Add { name, branch, kind } => add(ctx, name, branch, kind).await,
        EnvCommand::Ls => list(ctx).await,
        EnvCommand::Rm { name, yes } => remove(ctx, name, yes).await,
        EnvCommand::Stop => lifecycle(ctx, "stop").await,
        EnvCommand::Start => lifecycle(ctx, "start").await,
        EnvCommand::Share => lifecycle(ctx, "share").await,
        EnvCommand::Unshare => lifecycle(ctx, "unshare").await,
    }
}

/// Shows the environment and, without `--json`, the state of each
/// tunnel of the project (`lof dev`'s).
pub(super) async fn status<S: Sleeper>(ctx: &mut Ctx<'_, S>) -> Result<(), CliError> {
    let client = ctx.client()?;
    let project = ctx.want_project()?;
    ctx.finish()?;
    let project = resolve(&client, project).await?;
    let reply = client
        .get(&env_path(&project, &ctx.env_name()), &[])
        .await?;
    if ctx.json() {
        return ctx.show(&reply, |_| Ok(String::new()));
    }
    let env = parse::<EnvironmentView>(&reply.body)?;
    let listed = client
        .get(
            &format!("/projects/{}/environments", segment(&project)),
            &[],
        )
        .await?;
    let listed = parse::<Vec<EnvironmentView>>(&listed.body)?;
    ctx.say(&render::status(&env, &listed))
}

async fn add<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    name: Option<String>,
    branch: Option<String>,
    kind: Option<String>,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    let name = ctx.text(name, "<name>", "Environment name")?;
    let branch = ctx.text(branch, "--branch", "Branch to deploy from")?;
    let project = ctx.want_project()?;
    ctx.finish()?;
    let project = resolve(&client, project).await?;
    let body = EnvironmentCreate { name, branch, kind };
    let reply = client
        .send_json(
            Method::POST,
            &format!("/projects/{}/environments", segment(&project)),
            &[],
            &body,
        )
        .await?;
    ctx.show(&reply, |text| {
        let env: EnvironmentView = parse(text)?;
        Ok(format!(
            "added environment {}/{} (branch {})\n",
            env.project,
            env.name,
            env.branch.unwrap_or_default()
        ))
    })
}

async fn list<S: Sleeper>(ctx: &mut Ctx<'_, S>) -> Result<(), CliError> {
    let client = ctx.client()?;
    let project = ctx.want_project()?;
    ctx.finish()?;
    let project = resolve(&client, project).await?;
    let reply = client
        .get(
            &format!("/projects/{}/environments", segment(&project)),
            &[],
        )
        .await?;
    ctx.show(&reply, |body| {
        Ok(render::environments(&parse::<Vec<EnvironmentView>>(body)?))
    })
}

async fn remove<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    name: Option<String>,
    yes: bool,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    let name = ctx.text(name, "<name>", "Environment to remove")?;
    let project = ctx.want_project()?;
    let question = format!("Remove environment {name} and everything it runs?");
    ctx.confirm(yes, "--yes", &question)?;
    ctx.finish()?;
    let project = resolve(&client, project).await?;
    let reply = client
        .delete(
            &env_path(&project, &name),
            &[("confirm", "true".to_owned())],
        )
        .await?;
    ctx.ack(&reply, &format!("removed environment {project}/{name}"))
}

async fn lifecycle<S: Sleeper>(ctx: &mut Ctx<'_, S>, action: &str) -> Result<(), CliError> {
    let client = ctx.client()?;
    let project = ctx.want_project()?;
    ctx.finish()?;
    let project = resolve(&client, project).await?;
    let path = format!("{}/{action}", env_path(&project, &ctx.env_name()));
    let reply = client.post(&path).await?;
    ctx.show(&reply, |body| {
        Ok(render::environment(&parse::<EnvironmentView>(body)?))
    })
}

/// `bind <service>:<port> <host>`, or `bind <host>` for a function
/// environment: a lone argument without `:` is the domain (a host has
/// no `:`, a `service:port` always does).
pub(super) async fn bind<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    service_port: Option<String>,
    host: Option<String>,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    let (service_port, host) = match (service_port, host) {
        (Some(lone), None) if !lone.contains(':') => (None, lone),
        (service_port, host) => {
            let service_port = ctx.text(
                service_port,
                "<service>:<port>",
                "Service and port (web:3000)",
            )?;
            (Some(service_port), ctx.text(host, "<host>", "Domain")?)
        }
    };
    let project = ctx.want_project()?;
    ctx.finish()?;
    let (service, port) = match service_port {
        None => (None, None),
        Some(service_port) => {
            let (service, port) = service_port
                .rsplit_once(':')
                .and_then(|(service, port)| Some((service, port.parse::<u16>().ok()?)))
                .filter(|(service, _)| !service.is_empty())
                .ok_or_else(|| {
                    CliError::Usage(format!("expected <service>:<port>, got {service_port:?}"))
                })?;
            (Some(service.to_owned()), Some(port))
        }
    };
    let body = BindingCreate {
        host,
        service,
        port,
    };
    let project = resolve(&client, project).await?;
    let path = format!("{}/bindings", env_path(&project, &ctx.env_name()));
    let reply = client.send_json(Method::POST, &path, &[], &body).await?;
    ctx.show(&reply, |text| {
        Ok(render::binding(&parse::<BindingView>(text)?))
    })
}

/// `unbind <host>`.
pub(super) async fn unbind<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    host: Option<String>,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    let host = ctx.text(host, "<host>", "Domain to unbind")?;
    ctx.finish()?;
    let reply = client
        .delete(&format!("/bindings/{}", segment(&host)), &[])
        .await?;
    ctx.ack(&reply, &format!("unbound {host}"))
}
