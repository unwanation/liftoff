//! Running a `lof create` plan the server built (`POST /git/create-plan`): its local steps in the working tree,
//! its server steps through their own endpoints, in the plan's order.
//!
//! `lof server connect` runs the plan of the project local mode made
//! (LCL-04); `lof create` with a server runs the same, after it
//! generated, cloned and substituted the repository of a template. The
//! values `ask_secrets` collects are known first (what local `create`
//! generated, `--secret K=V`, what `create` asked before the forge
//! generated anything), then are generated or asked; `store_secrets`
//! sends them with `PUT /secrets`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use liftoff_api::{
    BindingCreate, CreatePlanView, CreateStep, DomainCreate, EnvironmentCreate, SecretAskView,
    SecretPlaceView, SecretPut, SecretView, SecretsSync,
};
use liftoff_core::manifest::{Generate, RepoPath};
use liftoff_core::service::{Substitution, generate_value};
use reqwest::Method;

use super::{Ctx, env_path, parse, segment};
use crate::Sleeper;
use crate::client::Client;
use crate::error::CliError;
use crate::local::{ForgeAuth, inside};
use crate::mode::Mode;
use crate::project::DOTFILE;
use crate::render::printable;

/// The work tree the plan runs in and the server it runs on.
pub(super) struct Target<'a> {
    /// The top of the work tree.
    pub(super) root: &'a Path,
    /// The server's name in the config, for `.liftoff`.
    pub(super) server: &'a str,
    /// The values `ask_secrets` takes before it generates or asks, by
    /// `(scope, key)`.
    pub(super) known: BTreeMap<(String, String), String>,
    /// The credential of the person's local connection on the plan's
    /// forge, for `push` (a private repository takes none other); `None`
    /// without one.
    pub(super) auth: Option<ForgeAuth>,
    /// The default branch of the forge's repository, which the work tree
    /// is on: `prod` is pinned to it and the plan's commit is pushed to it.
    pub(super) branch: String,
    /// The files of the work tree changed before the plan ran (the
    /// substituted ones), relative to it: the plan's commit holds them
    /// and what its own steps write, nothing else.
    pub(super) changed: Vec<String>,
}

/// `path` of the plan (relative to the work tree) under `root`, refused
/// when it is absolute, climbs out with `..` or goes through a symlink:
/// what a plan writes stays in the project directory.
fn in_work_tree(root: &Path, path: &str) -> Result<PathBuf, CliError> {
    let refused = |why: &str| {
        CliError::Local(format!(
            "the plan writes {path:?}, {why}; nothing was written"
        ))
    };
    let relative = RepoPath::new(path)
        .map_err(|error| refused(&format!("which is outside the work tree ({error})")))?;
    inside(root, &relative)?.ok_or_else(|| refused("which goes through a symlink"))
}

/// The values of `asks`: known, else generated (`random:<n>`), else asked
/// on a terminal; in batch mode a missing one ends the run listing
/// `--secret <KEY>` (CRE-04).
pub(super) fn answer<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    asks: &[SecretAskView],
    known: &BTreeMap<(String, String), String>,
) -> Result<BTreeMap<String, String>, CliError> {
    let mut answers = BTreeMap::new();
    for ask in asks {
        let value = if let Some(value) = known.get(&(ask.scope.clone(), ask.key.clone())) {
            value.clone()
        } else if let Some(generate) = ask.generate.as_deref() {
            let generate = Generate::parse(generate).ok_or_else(|| {
                CliError::Unexpected(format!("{}: generate {generate:?}", ask.key))
            })?;
            generate_value(generate, liftoff_core::system::fill)
                .expose()
                .to_owned()
        } else {
            // The key and the hint are the server's (a template's text).
            let question = printable(&ask.hint.as_ref().map_or_else(
                || format!("Value of {}", ask.key),
                |hint| format!("Value of {} ({hint})", ask.key),
            ));
            ctx.secret(None, &format!("--secret {}", ask.key), &question)?
        };
        let _previous = answers.insert(ask.key.clone(), value);
    }
    if ctx.mode == Mode::Batch {
        ctx.finish()?;
    }
    Ok(answers)
}

/// `{{project}}` and `{{domain}}` replaced in the files
/// of `substitution` under the work tree `root` (CRE-05). A symlink on
/// the way is not followed and a missing or non-UTF-8 file is skipped:
/// what the plan writes stays in the project directory. The files it
/// changed, relative to `root`.
pub(super) fn substitute(
    root: &Path,
    substitution: &Substitution,
) -> Result<Vec<String>, CliError> {
    let mut changed = Vec::new();
    for file in &substitution.files {
        let Some(path) = inside(root, file)? else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(replaced) = substitution.apply_to(file, &text)
            && replaced != text
        {
            std::fs::write(&path, replaced)?;
            changed.push(file.as_str().to_owned());
        }
    }
    Ok(changed)
}

/// The `id` of `scope` for `PUT /secrets`: `<project>/prod` for the
/// environment the plan adds, the project otherwise.
fn secret_id(project: &str, env: &str, scope: &str) -> String {
    if scope == "environment" {
        format!("{project}/{env}")
    } else {
        project.to_owned()
    }
}

/// Writes `.liftoff` naming `server` (keeping its `env`) and lists it in
/// `.gitignore` (it is never committed). Neither is written
/// through a symlink: both stay in the project directory.
fn write_liftoff_file(root: &Path, server: &str) -> Result<(), CliError> {
    let path = in_work_tree(root, DOTFILE)?;
    let ignore = in_work_tree(root, ".gitignore")?;
    let mut table: toml::Table = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or_default();
    let _previous = table.insert("server".to_owned(), server.into());
    let text = toml::to_string(&table)
        .map_err(|error| CliError::Local(format!("cannot encode {DOTFILE}: {error}")))?;
    std::fs::write(&path, text)?;
    let mut ignored = std::fs::read_to_string(&ignore).unwrap_or_default();
    if !ignored.lines().any(|line| line.trim() == DOTFILE) {
        if !ignored.is_empty() && !ignored.ends_with('\n') {
            ignored.push('\n');
        }
        ignored.push_str(DOTFILE);
        ignored.push('\n');
        std::fs::write(&ignore, ignored)?;
    }
    Ok(())
}

/// What the steps of a plan found out and did so far.
#[derive(Debug, Default)]
struct Progress {
    /// The values `ask_secrets` collected, by key.
    asked: BTreeMap<String, String>,
    /// The environment `add_environment` added (`prod`).
    env: Option<String>,
    /// The files of the work tree the plan changed, relative to it: what
    /// its commit holds.
    written: Vec<String>,
    /// One line per step done.
    done: String,
}

impl Progress {
    /// Adds the line `text`, its control characters replaced: it names
    /// what the server's plan says (paths, the commit message).
    fn line(&mut self, text: &str) {
        let _ok = writeln!(self.done, "{}", printable(text));
    }
}

/// Runs the steps of `plan` that follow a repository's creation, in
/// order: the local ones in `target.root`, the server ones on `client`.
/// What it did, one line per step, for the person.
///
/// # Errors
///
/// A step that fails ends the run; the steps before it stay done.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
    plan: &CreatePlanView,
    target: &Target<'_>,
) -> Result<String, CliError> {
    let mut progress = Progress {
        written: target.changed.clone(),
        ..Progress::default()
    };
    for view in &plan.steps {
        if !local_step(ctx, plan, target, &view.step, &mut progress)? {
            server_step(client, plan, target, &view.step, &mut progress).await?;
        }
    }
    Ok(progress.done)
}

/// Runs `step` when it is one of the work tree's; whether it was.
fn local_step<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    plan: &CreatePlanView,
    target: &Target<'_>,
    step: &CreateStep,
    progress: &mut Progress,
) -> Result<bool, CliError> {
    let git = ctx.env.local.git();
    match step {
        CreateStep::GenerateRepo { .. } | CreateStep::Clone | CreateStep::Substitute { .. } => {
            return Err(CliError::Unexpected(format!(
                "the plan for {} creates a new repository, which `lof server connect` does not do",
                plan.project
            )));
        }
        CreateStep::WriteManifest { content } => {
            std::fs::write(in_work_tree(target.root, "liftoff.toml")?, content)?;
            progress.written.push("liftoff.toml".to_owned());
            progress.line("wrote liftoff.toml");
        }
        CreateStep::AskSecrets { secrets } => {
            progress.asked = answer(ctx, secrets, &target.known)?;
        }
        CreateStep::WriteCiSnippet { path, content } => {
            let file = in_work_tree(target.root, path)?;
            if !file.exists() {
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&file, content)?;
                progress.written.push(path.clone());
                progress.line(&format!("wrote {path}"));
            }
        }
        CreateStep::Commit { message } => {
            if git.commit_paths(target.root, &progress.written, message)? {
                progress.line(&format!("committed \"{message}\""));
            }
        }
        CreateStep::Push => {
            git.push_to(target.root, &target.branch, target.auth.as_ref())?;
            progress.line(&format!("pushed to {}", target.branch));
        }
        CreateStep::WriteLiftoffFile => {
            write_liftoff_file(target.root, target.server)?;
            progress.line(&format!("wrote {DOTFILE} (server {})", target.server));
        }
        CreateStep::CreateProject { .. }
        | CreateStep::AddEnvironment { .. }
        | CreateStep::AddDomain { .. }
        | CreateStep::Bind { .. }
        | CreateStep::IssueToken
        | CreateStep::StoreSecrets { .. }
        | CreateStep::SyncCi => return Ok(false),
    }
    Ok(true)
}

/// The keys the tenant of `project` already holds, asked only when
/// `secrets` place one there.
async fn tenant_keys(
    client: &Client,
    project: &str,
    secrets: &[SecretPlaceView],
) -> Result<Vec<String>, CliError> {
    if !secrets.iter().any(|place| place.scope == "tenant") {
        return Ok(Vec::new());
    }
    let query = [("scope", "tenant".to_owned()), ("id", project.to_owned())];
    let reply = client.get("/secrets", &query).await?;
    Ok(parse::<Vec<SecretView>>(&reply.body)?
        .into_iter()
        .map(|secret| secret.key)
        .collect())
}

/// `store_secrets`: `PUT /secrets` of each value `ask_secrets` collected,
/// in its scope (`prod` for the environment). A tenant secret that
/// already exists is shared with the tenant's other projects: it is kept
/// as it is, as `CreateService::store_secrets` keeps it.
async fn store_secrets(
    client: &Client,
    project: &str,
    secrets: &[SecretPlaceView],
    progress: &mut Progress,
) -> Result<(), CliError> {
    let env = progress.env.clone().unwrap_or_else(|| "prod".to_owned());
    let shared = tenant_keys(client, project, secrets).await?;
    let mut stored = 0_usize;
    for place in secrets {
        if place.scope == "tenant" && shared.contains(&place.key) {
            progress.line(&format!(
                "kept tenant secret {} (the tenant's other projects share it)",
                place.key
            ));
            continue;
        }
        let value = progress
            .asked
            .get(&place.key)
            .cloned()
            .ok_or_else(|| CliError::Unexpected(format!("{} was not asked", place.key)))?;
        let body = SecretPut {
            scope: place.scope.clone(),
            id: secret_id(project, &env, &place.scope),
            key: place.key.clone(),
            value,
            ci: place.ci,
        };
        let _reply = client
            .send_json(Method::PUT, "/secrets", &[], &body)
            .await?;
        stored = stored.saturating_add(1);
    }
    progress.line(&format!("stored {stored} secrets"));
    Ok(())
}

/// Runs the server step `step` through its endpoint.
async fn server_step(
    client: &Client,
    plan: &CreatePlanView,
    target: &Target<'_>,
    step: &CreateStep,
    progress: &mut Progress,
) -> Result<(), CliError> {
    let project = plan.project.as_str();
    match step {
        CreateStep::CreateProject { project: body } => {
            let _reply = client
                .send_json(Method::POST, "/projects", &[], body)
                .await?;
            progress.line(&format!("created project {project}"));
        }
        CreateStep::AddEnvironment { name, kind } => {
            let branch = target.branch.clone();
            let body = EnvironmentCreate {
                name: name.clone(),
                branch: branch.clone(),
                kind: Some(kind.clone()),
            };
            let path = format!("/projects/{}/environments", segment(project));
            let _reply = client.send_json(Method::POST, &path, &[], &body).await?;
            progress.env = Some(name.clone());
            progress.line(&format!("added environment {name} on {branch}"));
        }
        CreateStep::AddDomain { host, label } => {
            let body = DomainCreate {
                project: project.to_owned(),
                name: label.clone().unwrap_or_else(|| host.clone()),
                external: label.is_none(),
                proxied: false,
            };
            let _reply = client
                .send_json(Method::POST, "/domains", &[], &body)
                .await?;
            progress.line(&format!("added domain {host}"));
        }
        CreateStep::Bind {
            env,
            service,
            port,
            host,
        } => {
            let body = BindingCreate {
                host: host.clone(),
                service: Some(service.clone()),
                port: Some(*port),
            };
            let path = format!("{}/bindings", env_path(project, env));
            let _reply = client.send_json(Method::POST, &path, &[], &body).await?;
            progress.line(&format!("bound {host} to {service}:{port}"));
        }
        CreateStep::IssueToken => {
            let _reply = client
                .post(&format!("/projects/{}/token/rotate", segment(project)))
                .await?;
            progress.line("issued the deploy token (LIFTOFF_TOKEN goes to CI)");
        }
        CreateStep::StoreSecrets { secrets } => {
            store_secrets(client, project, secrets, progress).await?;
        }
        CreateStep::SyncCi => {
            let body = SecretsSync {
                project: project.to_owned(),
            };
            let _reply = client
                .send_json(Method::POST, "/secrets/sync", &[], &body)
                .await?;
            progress.line("synced the CI secrets");
        }
        // The work tree's steps ran in `local_step`.
        CreateStep::GenerateRepo { .. }
        | CreateStep::Clone
        | CreateStep::Substitute { .. }
        | CreateStep::WriteManifest { .. }
        | CreateStep::AskSecrets { .. }
        | CreateStep::WriteCiSnippet { .. }
        | CreateStep::Commit { .. }
        | CreateStep::Push
        | CreateStep::WriteLiftoffFile => {}
    }
    Ok(())
}
