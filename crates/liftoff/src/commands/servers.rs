//! `server connect|ls|use|upgrade|status`: the installations of the config.

use std::collections::BTreeMap;
use std::path::Path;

use liftoff_api::{CreatePlanRequest, CreatePlanView, ExistingRepoBody};
use liftoff_core::domain::{Host, ProjectName, ScopeRef};
use liftoff_core::manifest::{ProjectManifest, RepoPath};
use liftoff_core::service::Substitution;
use reqwest::Method;

use super::{Ctx, parse, plan};
use crate::args::ServerCommand;
use crate::client::Client;
use crate::error::CliError;
use crate::local::{ForgeAuth, LocalSecretStore, inside, view};
use crate::project::{DOTFILE, work_tree_root};
use crate::{Sleeper, render};

/// Runs `server …`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: ServerCommand,
) -> Result<(), CliError> {
    match command {
        ServerCommand::Connect { url } => connect(ctx, url).await,
        ServerCommand::Ls => list(ctx),
        ServerCommand::Use { name } => use_server(ctx, name),
        ServerCommand::Upgrade => upgrade(ctx),
        ServerCommand::Status => status(ctx).await,
    }
}

/// The unit `server upgrade` restarts.
const UNIT: &str = "liftoffd.service";

/// Runs on the server's host as root: the release installer (the
/// latest release from the forge, its signed `SHA256SUMS` checked, replaces
/// `liftoffd`, `liftoff` and `lof`), then `systemctl restart`. A failed
/// install leaves the running daemon alone.
pub(super) fn upgrade<S: Sleeper>(ctx: &mut Ctx<'_, S>) -> Result<(), CliError> {
    ctx.finish()?;
    ctx.env
        .host
        .run("sh", &["-s"], Some(crate::host::INSTALL_SCRIPT))
        .map_err(CliError::Local)?;
    ctx.env
        .host
        .run("systemctl", &["restart", UNIT], None)
        .map_err(CliError::Local)?;
    ctx.say(&format!("liftoff upgraded; {UNIT} restarted\n"))
}

/// `server connect <url>` (LCL-04): the project of
/// the working tree, made in local mode, becomes a project of the server
/// at `url`, which the person logged in to first (`lof login`).
///
/// The server builds the plan of the existing repository from `git
/// remote` and the committed `liftoff.toml` (`POST /git/create-plan`,
/// which checks it against its store); the CLI runs its steps: the local
/// ones in the work tree (the CI workflow, its commit and push), the
/// server ones through their endpoints, then writes `.liftoff` naming the
/// server. The values the plan asks for come from the local store first
/// (what local `create` generated); the other local secrets move with
/// `lof secret push`.
async fn connect<S: Sleeper>(ctx: &mut Ctx<'_, S>, url: Option<String>) -> Result<(), CliError> {
    let url = ctx.text(url, "<url>", "Server URL")?;
    let repo = ctx.repository();
    let project = ctx.project_flag();
    ctx.finish()?;
    let url = url.trim_end_matches('/').to_owned();
    let config = ctx.config.clone().unwrap_or_default();
    let name = config
        .name_of(&url)
        .or_else(|| config.name_of(&format!("{url}/")))
        .ok_or_else(|| {
            CliError::Local(format!(
                "no server at {url} in the config: log in to it first with `lof login {url}`"
            ))
        })?;
    let chosen = config.choose(Some(&name))?.ok_or(CliError::NotLoggedIn)?;
    let token = chosen.server.token.clone().ok_or(CliError::NotLoggedIn)?;
    let repo = repo.ok_or_else(|| {
        CliError::Usage(
            "run `lof server connect` in the repository of the project, whose `origin` is on a forge"
                .to_owned(),
        )
    })?;
    let root = work_tree_root(&ctx.env.cwd);
    let manifest = std::fs::read_to_string(root.join("liftoff.toml")).map_err(|error| {
        CliError::Usage(format!(
            "{}: {error}; `lof create` writes liftoff.toml",
            root.join("liftoff.toml").display()
        ))
    })?;
    let project = project.unwrap_or_else(|| repo.name().to_ascii_lowercase());
    let local_name = ProjectName::new(&project)
        .map_err(|error| CliError::Usage(format!("--project: {error}")))?;
    let client = Client::new(&chosen.server.url, Some(token), ctx.env.cancel.clone())?;
    let request = CreatePlanRequest {
        name: project.clone(),
        existing: Some(ExistingRepoBody {
            git_host: repo.host.clone(),
            repo: repo.repo.clone(),
            manifest: manifest.clone(),
            detected: false,
        }),
        ..CreatePlanRequest::default()
    };
    let reply = client
        .send_json(Method::POST, "/git/create-plan", &[], &request)
        .await?;
    let plan: CreatePlanView = parse(&reply.body)?;
    let known = known_secrets(ctx, &local_name)?;
    let auth = Box::pin(local_forge_auth(ctx, &plan.git_host)?).await?;
    let branch = ready_work_tree(ctx, &root, auth.as_ref(), "lof server connect")?;
    let (substituted, changed) = substitute_domain(&root, &manifest, &plan)?;
    let target = plan::Target {
        root: &root,
        server: &name,
        known,
        auth,
        branch,
        changed,
    };
    let mut done = substituted;
    done.push_str(&plan::run(ctx, &client, &plan, &target).await?);
    if ctx.json() {
        return ctx.say(&super::raw(&reply.body));
    }
    ctx.say(&format!(
        "{done}{} is a project of {name} now; it deploys {} after the next `git push`. \
         Move its other local secrets with `lof secret push --move`.\n",
        plan.project, plan.domain
    ))
}

/// The default branch of the forge's repository, once the work tree
/// `root` is ready for the plan's commit: on that branch (`prod` deploys it, and the CI workflow goes there) and without
/// uncommitted changes of tracked files (the plan commits files the
/// person may be editing, `liftoff.toml` and the substituted ones, and
/// pushes them). Untracked files may stay: the commit holds what the plan
/// wrote only.
///
/// `command` (`lof server connect`, `lof create`) is what the person runs
/// again once the work tree is ready.
pub(super) fn ready_work_tree<S: Sleeper>(
    ctx: &Ctx<'_, S>,
    root: &Path,
    auth: Option<&ForgeAuth>,
    command: &str,
) -> Result<String, CliError> {
    let git = ctx.env.local.git();
    let default = git.default_branch(root, auth)?;
    let current = git.current_branch(root)?;
    if current != default {
        return Err(CliError::Local(format!(
            "the work tree is on {current}, and `prod` deploys {default}, where the plan commits \
             the CI workflow: switch to it (`git switch {default}`) and run `{command}` \
             again; nothing was changed"
        )));
    }
    for name in [DOTFILE, ".gitignore"] {
        let path = RepoPath::new(name).map_err(|error| CliError::Unexpected(error.to_string()))?;
        if inside(root, &path)?.is_none() {
            return Err(CliError::Local(format!(
                "{name} of the work tree is a symlink, and `{command}` writes it: \
                 make it a file; nothing was changed"
            )));
        }
    }
    if git.has_uncommitted_changes(root)? {
        return Err(CliError::Local(format!(
            "tracked files of the work tree have uncommitted changes, and the plan commits and \
             pushes: commit or stash them first (`git stash`), then run `{command}` \
             again; nothing was changed"
        )));
    }
    Ok(default)
}

/// The credential of the local connection on `host` (what local `create`
/// cloned and pushed with), for the plan's push to a private repository;
/// `None` when local mode has no store or no connection there.
pub(super) fn local_forge_auth<S: Sleeper>(
    ctx: &Ctx<'_, S>,
    host: &str,
) -> Result<impl Future<Output = Result<Option<ForgeAuth>, CliError>> + Send + 'static, CliError> {
    let host = Host::new(host).map_err(|error| CliError::Unexpected(error.to_string()))?;
    let open = ctx.home().secrets().exists().then(|| ctx.local_mode());
    Ok(async move {
        let Some(open) = open else {
            return Ok(None);
        };
        let local = open.await?;
        Ok(local
            .forge_access(&host)
            .await?
            .map(|access| ForgeAuth::of(&access)))
    })
}

/// The substitutions for a project local `create` made: it replaced
/// `{{project}}` only, as the domain is the server's; now that the plan
/// names it, `{{domain}}` too, in the files of the work tree's
/// `[substitute]` (a symlink is not followed), before the plan's commit.
/// One line when it changed a file, and the files it changed.
fn substitute_domain(
    root: &Path,
    manifest: &str,
    plan: &CreatePlanView,
) -> Result<(String, Vec<String>), CliError> {
    let manifest = ProjectManifest::parse(manifest)
        .map_err(|error| CliError::Usage(format!("liftoff.toml: {error}")))?;
    let substitution = Substitution {
        files: manifest.substitute,
        project: ProjectName::new(&plan.project)
            .map_err(|error| CliError::Unexpected(error.to_string()))?,
        domain: Host::new(&plan.domain).map_err(|error| CliError::Unexpected(error.to_string()))?,
    };
    let changed = plan::substitute(root, &substitution)?;
    let line = if changed.is_empty() {
        String::new()
    } else {
        format!("substituted {} in {}\n", plan.domain, changed.join(", "))
    };
    Ok((line, changed))
}

/// The values of the local store for `project`, by `(scope, key)`, as a
/// plan's `ask_secrets` names them (the environment is `prod`'s).
fn known_secrets<S: Sleeper>(
    ctx: &Ctx<'_, S>,
    project: &ProjectName,
) -> Result<BTreeMap<(String, String), String>, CliError> {
    let home = ctx.home();
    if !home.secrets().exists() {
        return Ok(BTreeMap::new());
    }
    let store = LocalSecretStore::open(&home)?;
    Ok(store
        .of_project(project)?
        .into_iter()
        .filter(|(secret, _)| match &secret.scope {
            ScopeRef::Environment(_, env) => env.as_str() == "prod",
            _ => true,
        })
        .map(|(secret, value)| {
            (
                (
                    view::scope_kind(&secret.scope).to_owned(),
                    secret.key.to_string(),
                ),
                value.expose().to_owned(),
            )
        })
        .collect())
}

/// Names and URLs, never tokens; the default is marked.
fn list<S: Sleeper>(ctx: &mut Ctx<'_, S>) -> Result<(), CliError> {
    let config = ctx.config.clone().unwrap_or_default();
    let default = config.default_server.clone().unwrap_or_default();
    if ctx.json() {
        let servers: Vec<serde_json::Value> = config
            .servers
            .iter()
            .map(|(name, server)| {
                serde_json::json!({
                    "name": name,
                    "url": server.url,
                    "default": *name == default,
                    "logged_in": server.token.is_some(),
                })
            })
            .collect();
        let text = serde_json::to_string(&servers)
            .map_err(|error| CliError::Unexpected(error.to_string()))?;
        return ctx.say(&format!("{text}\n"));
    }
    let rows: Vec<Vec<String>> = config
        .servers
        .iter()
        .map(|(name, server)| {
            vec![
                if *name == default { "*" } else { "" }.to_owned(),
                name.clone(),
                server.url.clone(),
                if server.token.is_some() { "yes" } else { "no" }.to_owned(),
            ]
        })
        .collect();
    let text = render::table(&["DEFAULT", "NAME", "URL", "LOGGED IN"], &rows);
    ctx.say(&text)
}

/// Makes `name` the default server.
fn use_server<S: Sleeper>(ctx: &mut Ctx<'_, S>, name: Option<String>) -> Result<(), CliError> {
    let mut config = ctx.config.clone().unwrap_or_default();
    let name = if name.is_none() && ctx.mode == crate::Mode::Interactive {
        let names: Vec<String> = config.servers.keys().cloned().collect();
        if names.is_empty() {
            return Err(CliError::NotLoggedIn);
        }
        let index = ctx.env.prompter.select("Default server", &names)?;
        names.get(index).cloned()
    } else {
        name
    };
    let name = ctx.text(name, "<name>", "Default server")?;
    ctx.finish()?;
    if !config.servers.contains_key(&name) {
        return Err(CliError::Config(format!(
            "no server named {name:?} in the config; see `liftoff server ls`"
        )));
    }
    config.default_server = Some(name.clone());
    ctx.save_config(&config)?;
    ctx.say(&format!("default server is now {name}\n"))
}

/// Asks the chosen server whether it is alive and ready.
async fn status<S: Sleeper>(ctx: &mut Ctx<'_, S>) -> Result<(), CliError> {
    let chosen = ctx.chosen.clone().ok_or(CliError::NotLoggedIn)?;
    let client = Client::new(
        &chosen.server.url,
        chosen.server.token.clone(),
        ctx.env.cancel.clone(),
    )?;
    let health = client.get("/healthz", &[]).await?;
    let ready = client.get("/readyz", &[]).await;
    if ctx.json() {
        return ctx.say(&super::raw(&health.body));
    }
    let ready = match ready {
        Ok(_) => "ready".to_owned(),
        Err(error) => format!("not ready: {error}"),
    };
    let text = render::pairs(&[
        ("server", chosen.name),
        ("url", chosen.server.url),
        ("health", "ok".to_owned()),
        ("ready", ready),
    ]);
    ctx.say(&text)
}
