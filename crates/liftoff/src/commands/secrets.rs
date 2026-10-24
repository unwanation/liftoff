//! `secret set|ls|get|rm|pull|push|sync|history|rollback`.

use liftoff_api::{
    EnvVar, ResolvedEnv, SecretPut, SecretRevisionView, SecretRollback, SecretValueView,
    SecretView, SecretsSync, SecretsSynced,
};
use liftoff_core::domain::{EnvName, ProjectName, ScopeRef, SecretKey, SecretValue};
use liftoff_core::ports::Clock as _;
use liftoff_core::service::{check_ci_scope, check_readable, env_scopes, merge_layers};
use liftoff_core::system::SystemClock;
use reqwest::Method;

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use super::{Ctx, ProjectRef, parse, resolve, segment};
use crate::args::{Scope, SecretCommand};
use crate::client::Client;
use crate::error::CliError;
use crate::local::{Home, LocalSecretStore, view};
use crate::mode::Mode;
use crate::project::work_tree_root;
use crate::{Sleeper, render};

/// Runs `secret …`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: SecretCommand,
) -> Result<(), CliError> {
    if ctx.is_local() {
        return local(ctx, command);
    }
    let client = ctx.client()?;
    match command {
        SecretCommand::Set { pairs, scope, ci } => set(ctx, &client, pairs, scope, ci).await,
        SecretCommand::Ls { scope } => {
            let project = ctx.want_project()?;
            ctx.finish()?;
            let query = scope_query(&client, project, ctx.env_name(), scope).await?;
            let reply = client.get("/secrets", &query).await?;
            ctx.show(&reply, |body| {
                Ok(render::secrets(&parse::<Vec<SecretView>>(body)?))
            })
        }
        SecretCommand::Get { key, scope } => {
            let key = ctx.text(key, "K", "Key")?;
            let project = ctx.want_project()?;
            ctx.finish()?;
            let query = scope_query(&client, project, ctx.env_name(), scope).await?;
            let reply = client
                .get(&format!("/secrets/{}", segment(&key)), &query)
                .await?;
            ctx.show(&reply, |body| {
                Ok(format!("{}\n", parse::<SecretValueView>(body)?.value))
            })
        }
        SecretCommand::Rm { key, scope } => {
            let key = ctx.text(key, "K", "Key to remove")?;
            let project = ctx.want_project()?;
            ctx.finish()?;
            let query = scope_query(&client, project, ctx.env_name(), scope).await?;
            let reply = client
                .delete(&format!("/secrets/{}", segment(&key)), &query)
                .await?;
            ctx.ack(&reply, &format!("removed {key}"))
        }
        SecretCommand::Pull { write } => pull(ctx, &client, write).await,
        SecretCommand::Push { move_ } => push(ctx, &client, move_).await,
        SecretCommand::Sync => sync(ctx, &client).await,
        SecretCommand::History { key, scope } => {
            let key = ctx.text(key, "K", "Key")?;
            let project = ctx.want_project()?;
            ctx.finish()?;
            let query = scope_query(&client, project, ctx.env_name(), scope).await?;
            let reply = client
                .get(&format!("/secrets/{}/versions", segment(&key)), &query)
                .await?;
            ctx.show(&reply, |body| {
                Ok(render::revisions(&parse::<Vec<SecretRevisionView>>(body)?))
            })
        }
        SecretCommand::Rollback { key, to, scope } => {
            let key = ctx.text(key, "K", "Key")?;
            let to = ctx.parsed(to, "--to", "Version to make current")?;
            let project = ctx.want_project()?;
            ctx.finish()?;
            let query = scope_query(&client, project, ctx.env_name(), scope).await?;
            let reply = client
                .send_json(
                    Method::POST,
                    &format!("/secrets/{}/rollback", segment(&key)),
                    &query,
                    &SecretRollback { to },
                )
                .await?;
            ctx.show(&reply, |body| {
                let secret: SecretView = parse(body)?;
                Ok(format!(
                    "{} is now version {} (the value of version {to})\n",
                    secret.key, secret.version
                ))
            })
        }
    }
}

/// `scope` and `id` of a scope: `<project>/<environment>` for the
/// environment, the project name for the project and for the tenant (the
/// tenant that project is in; the server answers `424` `tenants` for a
/// project outside every tenant).
async fn scope_ref(
    client: &Client,
    project: ProjectRef,
    env: String,
    scope: Scope,
) -> Result<(&'static str, String), CliError> {
    let project = resolve(client, project).await?;
    Ok(match scope {
        Scope::Environment => ("environment", format!("{project}/{env}")),
        Scope::Project => ("project", project),
        Scope::Tenant => ("tenant", project),
    })
}

async fn scope_query(
    client: &Client,
    project: ProjectRef,
    env: String,
    scope: Scope,
) -> Result<Vec<(&'static str, String)>, CliError> {
    let (scope, id) = scope_ref(client, project, env, scope).await?;
    Ok(vec![("scope", scope.to_owned()), ("id", id)])
}

/// The pairs of `set K=V…`, a value asked for a bare key on a terminal;
/// the value may contain `=`.
fn entries<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    pairs: Vec<String>,
) -> Result<Vec<(String, String)>, CliError> {
    let mut entries = Vec::new();
    if pairs.is_empty() {
        if ctx.mode == Mode::Batch {
            let _placeholder: String = ctx.text(None, "K=V", "Key")?;
        } else {
            let key = ctx.text(None, "K=V", "Key")?;
            let value = ctx.secret(None, "K=V", &format!("Value of {key}"))?;
            entries.push((key, value));
        }
    }
    for pair in pairs {
        match pair.split_once('=') {
            Some((key, value)) => entries.push((key.to_owned(), value.to_owned())),
            None if ctx.mode == Mode::Interactive => {
                let value = ctx.secret(None, "K=V", &format!("Value of {pair}"))?;
                entries.push((pair, value));
            }
            None => return Err(CliError::Usage(format!("expected K=V, got {pair:?}"))),
        }
    }
    Ok(entries)
}

/// `set K=V…`: one new version per pair.
async fn set<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
    pairs: Vec<String>,
    scope: Scope,
    ci: bool,
) -> Result<(), CliError> {
    let entries = entries(ctx, pairs)?;
    let project = ctx.want_project()?;
    ctx.finish()?;
    let (scope, id) = scope_ref(client, project, ctx.env_name(), scope).await?;
    for (key, value) in entries {
        let body = SecretPut {
            scope: scope.to_owned(),
            id: id.clone(),
            key,
            value,
            ci,
        };
        let reply = client
            .send_json(Method::PUT, "/secrets", &[], &body)
            .await?;
        ctx.show(&reply, |text| {
            let secret: SecretView = parse(text)?;
            Ok(format!(
                "set {} in {} {id} (version {})\n",
                secret.key, secret.scope, secret.version
            ))
        })?;
    }
    Ok(())
}

/// `pull`: the `.env` the environment runs with, in the order the server
/// resolves it, and nothing else on stdout.
async fn pull<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
    write: bool,
) -> Result<(), CliError> {
    let project = ctx.want_project()?;
    ctx.finish()?;
    let project = resolve(client, project).await?;
    let query = [("project", project), ("env", ctx.env_name())];
    let reply = client.get("/secrets/resolve", &query).await?;
    if write {
        return write_dotenv(ctx, &parse::<ResolvedEnv>(&reply.body)?);
    }
    ctx.show(&reply, |body| {
        let env: ResolvedEnv = parse(body)?;
        Ok(env
            .vars
            .iter()
            .map(|var| render::dotenv_line(&var.key, &var.value))
            .collect())
    })
}

/// `push [--move]` (LCL-05): the local store's secrets of the
/// project (made without a server) copied to the server, each in its scope
/// as locally (`PUT /secrets`); with `--move` each is deleted locally once
/// the server has it, and never before. The first refusal ends the run:
/// what was not pushed stays.
async fn push<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
    move_: bool,
) -> Result<(), CliError> {
    let project = ctx.want_project()?;
    ctx.finish()?;
    let local_name = local_project_name(&project)?;
    let home = ctx.home();
    // Nothing was kept locally: nothing to push, and no local key or store
    // is made as a side effect.
    if !home.secrets().exists() {
        return Ok(());
    }
    let remote = resolve(client, project).await?;
    let store = LocalSecretStore::open(&home)?;
    for (secret, value) in store.of_project(&local_name)? {
        let (scope, id) = match &secret.scope {
            ScopeRef::Environment(_, env) => ("environment", format!("{remote}/{env}")),
            ScopeRef::Tenant(_) => ("tenant", remote.clone()),
            _ => ("project", remote.clone()),
        };
        let body = SecretPut {
            scope: scope.to_owned(),
            id: id.clone(),
            key: secret.key.to_string(),
            value: value.expose().to_owned(),
            ci: secret.ci,
        };
        let reply = client
            .send_json(Method::PUT, "/secrets", &[], &body)
            .await?;
        let verb = if move_ {
            let _removed = store.remove(&secret.scope, &secret.key)?;
            "moved"
        } else {
            "pushed"
        };
        ctx.show(&reply, |_| {
            Ok(format!("{verb} {} to {scope} {id}\n", secret.key))
        })?;
    }
    Ok(())
}

/// `sync` (SYNC-01…04): `POST /secrets/sync` for the project,
/// then each key with what happened to it. A key the forge refused ends
/// the run with exit 1, after the keys that were sent are printed.
async fn sync<S: Sleeper>(ctx: &mut Ctx<'_, S>, client: &Client) -> Result<(), CliError> {
    let project = ctx.want_project()?;
    ctx.finish()?;
    let project = resolve(client, project).await?;
    let body = SecretsSync {
        project: project.clone(),
    };
    let reply = client
        .send_json(Method::POST, "/secrets/sync", &[], &body)
        .await?;
    let answer: SecretsSynced = parse(&reply.body)?;
    ctx.show(&reply, |_| Ok(render::synced(&answer, &project)))?;
    if answer.failed.is_empty() {
        return Ok(());
    }
    let keys: Vec<&str> = answer
        .failed
        .iter()
        .map(|failure| failure.key.as_str())
        .collect();
    let noun = if keys.len() == 1 { "secret" } else { "secrets" };
    Err(CliError::SyncFailed(format!(
        "{} CI {noun} not synced: {}",
        keys.len(),
        keys.join(", ")
    )))
}

// ---- local mode -----------------------------------------------------------------

/// The name of a local project: `--project`, else the name of the
/// `origin` repository (there is no server to resolve it).
pub(super) fn local_project_name(project: &ProjectRef) -> Result<ProjectName, CliError> {
    let name = match project {
        ProjectRef::Named(name) => name.clone(),
        ProjectRef::Repo(repo) => repo.name().to_ascii_lowercase(),
    };
    ProjectName::new(&name).map_err(|error| CliError::Usage(format!("--project: {error}")))
}

/// The local scope `scope` of `project`, `env` for an environment.
fn local_scope(project: ProjectName, env: &str, scope: Scope) -> Result<ScopeRef, CliError> {
    Ok(match scope {
        Scope::Environment => ScopeRef::Environment(
            project,
            EnvName::new(env).map_err(|error| CliError::Usage(format!("--env: {error}")))?,
        ),
        Scope::Project => ScopeRef::Project(project),
        Scope::Tenant => ScopeRef::Tenant(project),
    })
}

/// How a scope is named in what `set` prints: `demo/prod`, `demo`.
fn scope_id(scope: &ScopeRef) -> String {
    match scope {
        ScopeRef::Environment(project, env) => format!("{project}/{env}"),
        ScopeRef::Project(project) | ScopeRef::Tenant(project) => project.to_string(),
        _ => String::new(),
    }
}

/// A key of the command line.
fn secret_key(raw: &str) -> Result<SecretKey, CliError> {
    SecretKey::new(raw).map_err(|error| CliError::Usage(format!("{raw}: {error}")))
}

/// `secret …` without a server (LCL-02): the local store
/// in the config home, every value sealed under the local key, with its
/// versions. `push` needs the server it copies to, `sync` the server whose
/// CI secrets it sends.
fn local<S: Sleeper>(ctx: &mut Ctx<'_, S>, command: SecretCommand) -> Result<(), CliError> {
    match command {
        SecretCommand::Set { pairs, scope, ci } => local_set(ctx, pairs, scope, ci),
        SecretCommand::Ls { scope } => {
            let project = ctx.want_project()?;
            ctx.finish()?;
            let scope = local_scope(local_project_name(&project)?, &ctx.env_name(), scope)?;
            let listed = LocalSecretStore::open(&ctx.home())?.list(&scope)?;
            let views: Vec<SecretView> = listed.iter().map(view::secret).collect();
            ctx.say_json_or(&views, |views| render::secrets(views))
        }
        SecretCommand::Get { key, scope } => {
            let key = ctx.text(key, "K", "Key")?;
            let project = ctx.want_project()?;
            ctx.finish()?;
            let key = secret_key(&key)?;
            check_readable(&key)?;
            let scope = local_scope(local_project_name(&project)?, &ctx.env_name(), scope)?;
            let (value, version) = LocalSecretStore::open(&ctx.home())?
                .get(&scope, &key)?
                .ok_or_else(|| CliError::Local(format!("secret {key} not found")))?;
            let view = SecretValueView {
                key: key.to_string(),
                value: value.expose().to_owned(),
                version,
            };
            ctx.say_json_or(&view, |view| format!("{}\n", view.value))
        }
        SecretCommand::Rm { key, scope } => {
            let key = ctx.text(key, "K", "Key to remove")?;
            let project = ctx.want_project()?;
            ctx.finish()?;
            let key = secret_key(&key)?;
            let scope = local_scope(local_project_name(&project)?, &ctx.env_name(), scope)?;
            if !LocalSecretStore::open(&ctx.home())?.remove(&scope, &key)? {
                return Err(CliError::Local(format!("secret {key} not found")));
            }
            ctx.say_json_or(&serde_json::json!({"key": key.as_str()}), |_| {
                format!("removed {key}\n")
            })
        }
        SecretCommand::Pull { write } => local_pull(ctx, write),
        SecretCommand::Push { .. } | SecretCommand::Sync => Err(needs_server(&command)),
        SecretCommand::History { key, scope } => {
            let key = ctx.text(key, "K", "Key")?;
            let project = ctx.want_project()?;
            ctx.finish()?;
            let key = secret_key(&key)?;
            let scope = local_scope(local_project_name(&project)?, &ctx.env_name(), scope)?;
            let revisions: Vec<SecretRevisionView> = LocalSecretStore::open(&ctx.home())?
                .history(&scope, &key)?
                .iter()
                .map(view::revision)
                .collect();
            ctx.say_json_or(&revisions, |revisions| render::revisions(revisions))
        }
        SecretCommand::Rollback { key, to, scope } => {
            let key = ctx.text(key, "K", "Key")?;
            let to = ctx.parsed(to, "--to", "Version to make current")?;
            let project = ctx.want_project()?;
            ctx.finish()?;
            let key = secret_key(&key)?;
            let scope = local_scope(local_project_name(&project)?, &ctx.env_name(), scope)?;
            let secret = LocalSecretStore::open(&ctx.home())?.rollback(
                &scope,
                &key,
                to,
                SystemClock.now(),
            )?;
            ctx.say_json_or(&view::secret(&secret), |secret| {
                format!(
                    "{} is now version {} (the value of version {to})\n",
                    secret.key, secret.version
                )
            })
        }
    }
}

/// Why `push` and `sync` refuse without a server: `push` copies to one,
/// and the CI secrets `sync` sends (`LIFTOFF_TOKEN` among them) are the
/// server's.
fn needs_server(command: &SecretCommand) -> CliError {
    let what = if matches!(command, SecretCommand::Sync) {
        "secret sync sends the server's CI secrets (LIFTOFF_TOKEN among them) to the forge"
    } else {
        "secret push copies the secrets of local mode to a server"
    };
    CliError::Local(format!(
        "{what}: log in with `lof login <url>` and connect the project with `lof server connect <url>` first"
    ))
}

/// `secret set K=V…` without a server.
fn local_set<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    pairs: Vec<String>,
    scope: Scope,
    ci: bool,
) -> Result<(), CliError> {
    let entries = entries(ctx, pairs)?;
    let project = ctx.want_project()?;
    ctx.finish()?;
    let scope = local_scope(local_project_name(&project)?, &ctx.env_name(), scope)?;
    check_ci_scope(&scope, ci)?;
    let store = LocalSecretStore::open(&ctx.home())?;
    let id = scope_id(&scope);
    for (key, value) in entries {
        let key = secret_key(&key)?;
        let secret = store.set(
            &scope,
            &key,
            &SecretValue::new(value),
            ci,
            SystemClock.now(),
        )?;
        ctx.say_json_or(&view::secret(&secret), |view| {
            format!(
                "set {} in {} {id} (version {})\n",
                view.key, view.scope, view.version
            )
        })?;
    }
    Ok(())
}

/// `secret pull` without a server: the local secrets of the environment
/// as `.env`, merged as the core merges an env (`env_scopes`, `merge_layers`: tenant, then project, then environment, the lower scope winning; the keys for CI only left out).
fn local_pull<S: Sleeper>(ctx: &mut Ctx<'_, S>, write: bool) -> Result<(), CliError> {
    let project = ctx.want_project()?;
    ctx.finish()?;
    let env = local_resolve(&ctx.home(), &project, &ctx.env_name())?;
    if write {
        return write_dotenv(ctx, &env);
    }
    ctx.say_json_or(&env, |env| {
        env.vars
            .iter()
            .map(|var| render::dotenv_line(&var.key, &var.value))
            .collect()
    })
}

/// `pull --write`: `env` into `.env.<env>` at the top of the working
/// tree, replaced whole, readable by its owner only (`0600`, also when it
/// existed); the path goes to stdout, how many keys to stderr.
fn write_dotenv<S: Sleeper>(ctx: &mut Ctx<'_, S>, env: &ResolvedEnv) -> Result<(), CliError> {
    let name = format!(".env.{}", ctx.env_name());
    let path = work_tree_root(&ctx.env.cwd).join(&name);
    let text: String = env
        .vars
        .iter()
        .map(|var| render::dotenv_line(&var.key, &var.value))
        .collect();
    let cannot = |error: std::io::Error| CliError::Local(format!("cannot write {name}: {error}"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(cannot)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(cannot)?;
    file.write_all(text.as_bytes()).map_err(cannot)?;
    ctx.say(&format!("{}\n", path.display()))?;
    ctx.tell(&format!(
        "wrote {} keys to {name}; keep it out of git (`.gitignore`)\n",
        env.vars.len()
    ))
}

/// The environment `env` of `project` runs with in local mode, merged as
/// the core merges an env (`env_scopes`, `merge_layers`: tenant, then project, then environment, the lower scope winning; the keys for CI only left out): what `secret pull` prints and writes.
pub(super) fn local_resolve(
    home: &Home,
    project: &ProjectRef,
    env: &str,
) -> Result<ResolvedEnv, CliError> {
    let project = local_project_name(project)?;
    let env = EnvName::new(env).map_err(|error| CliError::Usage(format!("--env: {error}")))?;
    let store = LocalSecretStore::open(home)?;
    let mut layers = Vec::new();
    for scope in env_scopes(&project, &env) {
        for secret in store.list(&scope)? {
            if let Some((value, _version)) = store.get(&scope, &secret.key)? {
                layers.push((secret.key, value));
            }
        }
    }
    Ok(ResolvedEnv {
        vars: merge_layers(layers)?
            .into_iter()
            .map(|(key, value)| EnvVar {
                key: key.to_string(),
                value: value.expose().to_owned(),
            })
            .collect(),
    })
}
