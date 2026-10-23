//! `deploy`, `redeploy`, `deploys [ls|show|wait]`, `logs`, `token
//! rotate|snippet`.

use std::fmt::Write as _;
use std::time::Duration;

use liftoff_api::{DeployBody, DeployView, DeploysQueued, RedeployBody, SnippetView, TokenView};
use reqwest::{Method, Response};
use tokio_util::sync::CancellationToken;

use super::{Ctx, env_path, functions, parse, resolve, segment};
use crate::args::{DeployArgs, DeploysCommand, LogsArgs, TokenCommand};
use crate::client::{Client, Reply, describe};
use crate::error::CliError;
use crate::project::work_tree_root;
use crate::sse::Parser;
use crate::{Sleeper, WAIT_INTERVAL, render};

/// `deploy --tag T [--compose F] [--wait]`: a manual deploy of the
/// environment (with a session). In a function
/// project (`[function]` of `liftoff.toml`) a bare `deploy` builds,
/// checks and uploads the component and waits for it ([`functions`]).
pub(super) async fn deploy<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    args: DeployArgs,
) -> Result<(), CliError> {
    if args.tag.is_none() && args.compose.is_none() {
        let root = work_tree_root(&ctx.env.cwd);
        if let Some(function) = functions::function_of(&root)? {
            let project = ctx.want_project()?;
            ctx.finish()?;
            return functions::deploy(ctx, &root, function, project).await;
        }
    }
    let client = ctx.client()?;
    let tag = ctx.text(args.tag, "--tag", "Image tag")?;
    let project = ctx.want_project()?;
    ctx.finish()?;
    let compose = args
        .compose
        .as_deref()
        .map(|path| {
            std::fs::read_to_string(ctx.env.cwd.join(path))
                .map_err(|error| CliError::Usage(format!("cannot read {path}: {error}")))
        })
        .transpose()?;
    let project = resolve(&client, project).await?;
    let body = DeployBody {
        tag: Some(tag),
        compose,
        project: Some(project),
        environment: Some(ctx.env_name()),
        ..DeployBody::default()
    };
    let reply = client
        .send_json(Method::POST, "/deploys", &[], &body)
        .await?;
    queued(ctx, &client, &reply, args.wait).await
}

/// `redeploy [--to N] [--wait]`.
pub(super) async fn redeploy<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    to: Option<u32>,
    wait: bool,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    let project = ctx.want_project()?;
    ctx.finish()?;
    let project = resolve(&client, project).await?;
    let path = format!("{}/redeploy", env_path(&project, &ctx.env_name()));
    let reply = client
        .send_json(Method::POST, &path, &[], &RedeployBody { to })
        .await?;
    queued(ctx, &client, &reply, wait).await
}

/// Says what was queued and, with `--wait`, how it ended.
pub(super) async fn queued<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
    reply: &Reply,
    wait: bool,
) -> Result<(), CliError> {
    let queued: DeploysQueued = parse(&reply.body)?;
    ctx.show(reply, |_| {
        if queued.deploys.is_empty() {
            return Ok("nothing to deploy\n".to_owned());
        }
        let mut text = String::new();
        for deploy in &queued.deploys {
            writeln!(
                text,
                "queued deploy #{} ({}) of {}/{}",
                deploy.number, deploy.deploy_id, deploy.project, deploy.environment
            )
            .map_err(|error| CliError::Local(error.to_string()))?;
        }
        Ok(text)
    })?;
    if wait {
        for deploy in &queued.deploys {
            wait_for(ctx, client, &deploy.deploy_id).await?;
        }
    }
    Ok(())
}

/// `deploys`, `deploys ls`, `deploys show <id>`, `deploys wait <id>`.
pub(super) async fn history<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: Option<DeploysCommand>,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    match command {
        None | Some(DeploysCommand::Ls) => {
            let project = ctx.want_project()?;
            ctx.finish()?;
            let project = resolve(&client, project).await?;
            let path = format!("{}/deploys", env_path(&project, &ctx.env_name()));
            let reply = client.get(&path, &[]).await?;
            ctx.show(&reply, |body| {
                Ok(render::deploys(&parse::<Vec<DeployView>>(body)?))
            })
        }
        Some(DeploysCommand::Show { id }) => {
            let id = ctx.text(id, "<id>", "Deploy id")?;
            ctx.finish()?;
            let reply = client
                .get(&format!("/deploys/{}", segment(&id)), &[])
                .await?;
            ctx.show(&reply, |body| {
                Ok(render::deploy(&parse::<DeployView>(body)?))
            })
        }
        Some(DeploysCommand::Wait { id }) => {
            let id = ctx.text(id, "<id>", "Deploy id")?;
            ctx.finish()?;
            wait_for(ctx, &client, &id).await
        }
    }
}

/// Looks at the deploy every [`WAIT_INTERVAL`] until it succeeded or failed.
async fn wait_for<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
    id: &str,
) -> Result<(), CliError> {
    let path = format!("/deploys/{}", segment(id));
    loop {
        let reply = client.get(&path, &[]).await?;
        let deploy: DeployView = parse(&reply.body)?;
        match deploy.status.as_str() {
            "succeeded" => {
                return ctx.show(&reply, |_| {
                    Ok(format!("deploy #{} succeeded\n", deploy.number))
                });
            }
            "failed" => {
                let mut text = format!("deploy #{} failed", deploy.number);
                if let Some(error) = &deploy.error {
                    text.push_str(": ");
                    text.push_str(error);
                }
                if ctx.json() {
                    ctx.say(&super::raw(&reply.body))?;
                }
                return Err(CliError::DeployFailed(text));
            }
            _ => pause(&ctx.env.sleeper, &ctx.env.cancel, WAIT_INTERVAL).await?,
        }
    }
}

pub(super) async fn pause<S: Sleeper>(
    sleeper: &S,
    cancel: &CancellationToken,
    duration: Duration,
) -> Result<(), CliError> {
    tokio::select! {
        () = sleeper.sleep(duration) => Ok(()),
        () = cancel.cancelled() => Err(CliError::Interrupted),
    }
}

/// `logs [service] [-f] [--tail N] [--deploy N]`.
pub(super) async fn logs<S: Sleeper>(ctx: &mut Ctx<'_, S>, args: LogsArgs) -> Result<(), CliError> {
    let client = ctx.client()?;
    let project = ctx.want_project()?;
    ctx.finish()?;
    let project = resolve(&client, project).await?;
    let path = env_path(&project, &ctx.env_name());
    if let Some(number) = args.deploy {
        return deploy_log(ctx, &client, &path, number).await;
    }
    let mut query = vec![("follow", args.follow.to_string())];
    if let Some(service) = args.service {
        query.push(("service", service));
    }
    if let Some(tail) = args.tail {
        query.push(("tail", tail.to_string()));
    }
    let response = match client.stream(&format!("{path}/logs"), &query).await {
        Ok(response) => response,
        // Ctrl-C before the first byte ends a log view quietly, too.
        Err(CliError::Interrupted) => return Ok(()),
        Err(error) => return Err(error),
    };
    follow(ctx, response).await
}

/// Prints the stored log of deploy `number` of the environment at `path`.
async fn deploy_log<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
    path: &str,
    number: u32,
) -> Result<(), CliError> {
    let history = client.get(&format!("{path}/deploys"), &[]).await?;
    let deploys: Vec<DeployView> = parse(&history.body)?;
    let deploy = deploys
        .iter()
        .find(|deploy| deploy.number == number)
        .ok_or_else(|| CliError::Usage(format!("no deploy #{number} in this environment")))?;
    let reply = client
        .get(&format!("/deploys/{}/log", segment(&deploy.id)), &[])
        .await?;
    ctx.say(&reply.body)
}

/// Prints each event's data as it arrives. Ends when the server does, or
/// quietly on Ctrl-C.
async fn follow<S: Sleeper>(ctx: &mut Ctx<'_, S>, mut response: Response) -> Result<(), CliError> {
    let mut parser = Parser::default();
    loop {
        let chunk = tokio::select! {
            biased;
            () = ctx.env.cancel.cancelled() => return Ok(()),
            chunk = response.chunk() => chunk,
        };
        let Some(chunk) = chunk.map_err(|error| CliError::Network(describe(&error)))? else {
            return Ok(());
        };
        for event in parser.feed(&chunk) {
            if event.name == "error" {
                return Err(CliError::Api {
                    message: event.data,
                    body: String::new(),
                });
            }
            ctx.say(&format!("{}\n", event.data))?;
        }
    }
}

/// `token rotate`, `token snippet`.
pub(super) async fn token<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: TokenCommand,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    let project = ctx.want_project()?;
    ctx.finish()?;
    let project = resolve(&client, project).await?;
    match command {
        TokenCommand::Rotate => {
            let reply = client
                .post(&format!("/projects/{}/token/rotate", segment(&project)))
                .await?;
            ctx.show(&reply, |body| {
                let token: TokenView = parse(body)?;
                Ok(format!(
                    "new deploy token {} for {} (stored as the project secret LIFTOFF_TOKEN)\n",
                    token.id, token.project
                ))
            })
        }
        TokenCommand::Snippet => {
            let reply = client
                .get(&format!("/projects/{}/snippet", segment(&project)), &[])
                .await?;
            ctx.show(&reply, |body| {
                // A YAML comment names the file, so `lof token snippet >
                // <path>` still writes a valid workflow.
                let view = parse::<SnippetView>(body)?;
                Ok(format!("# {}\n{}\n", view.path, view.snippet.trim_end()))
            })
        }
    }
}
