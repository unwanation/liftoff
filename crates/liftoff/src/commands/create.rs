//! `create`: a project from a
//! template, or of the repository the working tree belongs to.
//!
//! **From a template** (`--template`, or picked on a terminal): the
//! person picks the group, then the template of the group (its name and
//! description, from the catalogue of `lof templates`), types the project
//! name (the template's by default), picks the visibility and the owner
//! (private under their own forge account by default), answers the
//! `[secrets]` the plan asks without `generate` (generated silently), and
//! confirms the plan. The flags (`--template`, `--group`, `--secret K=V`,
//! `--private|--public`, `--owner`, `--domain`, `--tenant`) give the same
//! plan without a question; in batch mode `--template` is required
//! (CRE-15). Every question is asked, and every secret is answered,
//! before the forge generates anything: a no, or a missing `--secret`,
//! creates nothing.
//!
//! - With a server the server plans (`POST /git/create-plan`, which checks
//!   the name, the repository and the domain; interactively a taken
//!   domain is asked again, CRE-07) and generates the repository through
//!   the person's connection there (`POST /git/generate`); the
//!   CLI clones it into `./<name>`, replaces `{{project}}` and
//!   `{{domain}}`, and runs the rest of the plan (`plan`): the CI
//!   workflow, the commit and push, the server steps through their
//!   endpoints, `.liftoff` naming the server and listed in `.gitignore`
//!   (CRE-14). The tenant is `--tenant`, else the one `lof tenant use`
//!   wrote for the server (CRE-17).
//! - Without a server (local mode, LCL-03) the core's `CreateService`
//!   plans and generates over the local ports, and the plan stops after
//!   `Push`: the clone, `[substitute]` (`{{project}}` only: the domain is
//!   the server's), the `[secrets]` into the local store, the commit and
//!   the push. The CI workflow names the server's API:
//!   `lof server connect` writes it with the server steps.
//!
//! **Of an existing repository** (no template, in a work tree whose
//! `origin` is on a forge): the committed `liftoff.toml`, else what stack
//! detection proposes (the person confirms it on a terminal, `--service`/`--port` answer what the files leave open). With a server
//! it is sent as the plan's `existing`, and the plan writes and commits a
//! detected manifest (CRE-09). Without a server `create` stops after
//! step 3: a detected manifest is committed alone and pushed,
//! and the project is ready for `lof server connect`. Without a
//! template, a remote or a name there is nothing to create: 64 with a
//! hint (CRE-16).
//!
//! `--json` prints one document, `{"plan": …, "result": {project,
//! git_host, repo, url}}` (CRE-13): the plan as the server answered it
//! (or local mode built it; `null` for an existing repository without a
//! server) and what was made; the URL answers after the first `git push`
//! (a function's after `lof deploy`) and is absent in local mode.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use liftoff_api::{
    CreatePlanRequest, CreatePlanView, CreateResultView, CreateRunView, CreateStep,
    ExistingRepoBody, GeneratedRepoView, SecretAskView, TemplateCatalogView, TemplateView,
};
use liftoff_core::detect::{DirTree, Question, StackDetector};
use liftoff_core::domain::{
    ContainerPort, Host, Login, ProjectName, RepoRef, SecretKey, SecretValue, ServiceName,
    TemplateSource, TenantSlug,
};
use liftoff_core::manifest::{Deployable, Expose, ProjectManifest, RepoPath};
use liftoff_core::ports::{Clock as _, Ports as _};
use liftoff_core::service::{
    CreateOptions, CreatePlan, CreateService, ExistingManifest, Side, Step, Substitution,
    local_existing_steps, plan_secret_scope, taken_host,
};
use liftoff_core::system::SystemClock;
use reqwest::Method;
use serde_json::Value;

use super::{Ctx, parse, plan, servers, templates};
use crate::Sleeper;
use crate::args::CreateArgs;
use crate::client::Client;
use crate::error::CliError;
use crate::local::{ForgeAuth, LocalMode, clone_url, inside, view};
use crate::mode::Mode;
use crate::project::{Repository, work_tree_root};
use crate::render::printable;

/// The manifest of a project.
const MANIFEST: &str = "liftoff.toml";

/// Batch `create` with neither a template, a remote nor a name (CRE-16).
const NOTHING_TO_CREATE: &str = "nothing to create here: this directory has no `origin` remote on a forge and no project was named; run `lof create <name> --template owner/repo` for a project from a template (`lof templates` lists them), or `lof create` in a repository whose `origin` remote is on a forge";

/// What the person asked for, before anything is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wanted {
    /// The template, `[host/]owner/name`.
    template: String,
    /// The project name, as typed.
    name: String,
    /// Whether the repository is public.
    public: bool,
    /// The owner of the repository; the person's own account when `None`.
    owner: Option<String>,
}

/// Runs `create`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    args: CreateArgs,
) -> Result<(), CliError> {
    let flags = secret_flags(&args.secrets)?;
    if args.template.is_none() {
        if let Some(repo) = ctx.repository() {
            if ctx.is_local() {
                return local_existing(ctx, &args, &repo).await;
            }
            return existing(ctx, args, &repo, &flags).await;
        }
        if ctx.mode == Mode::Batch {
            return Err(if args.name.is_none() {
                CliError::Usage(NOTHING_TO_CREATE.to_owned())
            } else {
                CliError::Missing(vec!["--template".to_owned()])
            });
        }
    }
    if ctx.is_local() {
        local(ctx, args, &flags).await
    } else {
        server(ctx, args, &flags).await
    }
}

/// The values of `--secret K=V`, by key.
fn secret_flags(given: &[String]) -> Result<BTreeMap<String, String>, CliError> {
    let mut flags = BTreeMap::new();
    for pair in given {
        let (key, value) = pair
            .split_once('=')
            .filter(|(key, _)| !key.is_empty())
            .ok_or_else(|| CliError::Usage(format!("--secret {pair:?}: write it as K=V")))?;
        let _previous = flags.insert(key.to_owned(), value.to_owned());
    }
    Ok(flags)
}

/// `--private`, `--public`, `--owner` and `--group` name the repository a
/// template generates.
fn refuse_template_flags(args: &CreateArgs) -> Result<(), CliError> {
    let given: Vec<&str> = [
        (args.group.is_some(), "--group"),
        (args.private, "--private"),
        (args.public, "--public"),
        (args.owner.is_some(), "--owner"),
    ]
    .into_iter()
    .filter_map(|(given, flag)| given.then_some(flag))
    .collect();
    if given.is_empty() {
        return Ok(());
    }
    Err(CliError::Usage(format!(
        "{} name the repository a template generates, and this repository exists already",
        given.join(", ")
    )))
}

// --- The template wizard -----------------------------------------------------

/// What the person wants, from the flags and, on a terminal, the wizard
/// over `catalogue`: the group (unless `--group` or the
/// catalogue has one only), the template with its name and description,
/// the name (the template's by default), the visibility and the owner.
fn gather<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    args: &CreateArgs,
    catalogue: Option<&TemplateCatalogView>,
) -> Result<Wanted, CliError> {
    let (template, default_name) = match (&args.template, catalogue) {
        (Some(template), _) => (template.clone(), None),
        (None, Some(catalogue)) => {
            let picked = pick_template(ctx, catalogue, args.group.as_deref())?;
            (
                format!("{}/{}", picked.host, picked.repo),
                Some(picked.name),
            )
        }
        (None, None) => return Err(CliError::Missing(vec!["--template".to_owned()])),
    };
    let wizard = default_name.is_some();
    let name = match (args.name.clone(), default_name) {
        (Some(name), _) => name,
        (None, Some(default)) => ctx
            .optional_text(&format!("Project name (empty: {default})"))?
            .unwrap_or(default),
        (None, None) => ctx.text(None, "<name>", "Project name")?,
    };
    let public = if args.public {
        true
    } else if args.private || !wizard {
        false
    } else {
        ctx.pick(
            None,
            "--public",
            "Repository",
            &[("private", false), ("public", true)],
        )?
    };
    let owner = if args.owner.is_some() || !wizard {
        args.owner.clone()
    } else {
        ctx.optional_text(
            "Owner of the repository, a user or an organisation (empty: your own account)",
        )?
    };
    Ok(Wanted {
        template,
        name,
        public,
        owner,
    })
}

/// The template picked from `catalogue`: its group first (unless `group`
/// names it or there is one only), then one of the group's templates by
/// name and description.
fn pick_template<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    catalogue: &TemplateCatalogView,
    group: Option<&str>,
) -> Result<TemplateView, CliError> {
    for warning in &catalogue.warnings {
        ctx.tell(&format!(
            "skipped template {}: {}\n",
            printable(&warning.repo),
            printable(&warning.reason)
        ))?;
    }
    let groups: Vec<_> = catalogue
        .groups
        .iter()
        .filter(|candidate| group.is_none_or(|group| candidate.group == group))
        .filter(|candidate| !candidate.templates.is_empty())
        .collect();
    let chosen = match groups.as_slice() {
        [] => {
            return Err(CliError::Usage(format!(
                "the catalogue has no template{}: list template repositories in `[local] templates` of the CLI config (or `templates.repos` of the server), or give `--template owner/repo`",
                group.map_or_else(String::new, |group| format!(" in group {group}"))
            )));
        }
        [only] => *only,
        several => {
            let names: Vec<String> = several
                .iter()
                .map(|group| printable(&group.group))
                .collect();
            let index = ctx.env.prompter.select("Group", &names)?;
            several
                .get(index)
                .copied()
                .ok_or_else(|| CliError::Local(format!("no option {index}")))?
        }
    };
    let options: Vec<String> = chosen
        .templates
        .iter()
        .map(|template| {
            format!(
                "{} — {}",
                printable(&template.name),
                printable(&template.description)
            )
        })
        .collect();
    let index = ctx.env.prompter.select("Template", &options)?;
    chosen
        .templates
        .get(index)
        .cloned()
        .ok_or_else(|| CliError::Local(format!("no option {index}")))
}

/// `./<name>` for project `name`, which must not exist yet.
fn destination<S: Sleeper>(ctx: &Ctx<'_, S>, name: &str) -> Result<std::path::PathBuf, CliError> {
    let name =
        ProjectName::new(name).map_err(|error| CliError::Usage(format!("<name>: {error}")))?;
    let dest = ctx.env.cwd.join(name.as_str());
    if dest.exists() {
        return Err(CliError::Usage(format!(
            "{} exists already: run `lof create` where `./{name}` can be made",
            dest.display()
        )));
    }
    Ok(dest)
}

/// The `[secrets]` the plan asks.
fn asks(plan: &CreatePlanView) -> Vec<SecretAskView> {
    plan.steps
        .iter()
        .filter_map(|view| match &view.step {
            CreateStep::AskSecrets { secrets } => Some(secrets.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// The values of the plan's `[secrets]`, before the forge generates
/// anything: `--secret K=V` (stronger than `generate`), generated, asked
/// without echo on a terminal, or missing as `--secret <KEY>` in batch
/// mode (CRE-04). By `(scope, key)`, as `plan::run` takes them.
fn answer_secrets<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    plan: &CreatePlanView,
    flags: &BTreeMap<String, String>,
) -> Result<BTreeMap<(String, String), String>, CliError> {
    let asks = asks(plan);
    if let Some(unknown) = flags
        .keys()
        .find(|key| !asks.iter().any(|ask| &ask.key == *key))
    {
        return Err(CliError::Usage(format!(
            "--secret {unknown}: the plan of {} asks no such secret",
            plan.project
        )));
    }
    let known: BTreeMap<(String, String), String> = asks
        .iter()
        .filter_map(|ask| {
            flags
                .get(&ask.key)
                .map(|value| ((ask.scope.clone(), ask.key.clone()), value.clone()))
        })
        .collect();
    let answers = plan::answer(ctx, &asks, &known)?;
    Ok(asks
        .iter()
        .filter_map(|ask| {
            answers
                .get(&ask.key)
                .map(|value| ((ask.scope.clone(), ask.key.clone()), value.clone()))
        })
        .collect())
}

/// On a terminal, the plan shown on stderr and a yes to it; a no ends the
/// run with nothing created.
fn confirm_plan<S: Sleeper>(ctx: &mut Ctx<'_, S>, plan: &CreatePlanView) -> Result<(), CliError> {
    if ctx.mode != Mode::Interactive {
        return Ok(());
    }
    ctx.tell(&describe(plan))?;
    ctx.confirm(false, "--yes", &format!("Create {}?", plan.project))
}

/// The plan, one line per step, each with its control characters
/// replaced: the plan's text is the server's, and a template's paths
/// reach it.
fn describe(plan: &CreatePlanView) -> String {
    let mut text = printable(&format!(
        "The plan for {} ({}/{}, {}):",
        plan.project, plan.git_host, plan.repo, plan.domain
    ));
    text.push('\n');
    for view in &plan.steps {
        let line = match &view.step {
            CreateStep::GenerateRepo {
                template,
                repo,
                private,
            } => format!(
                "generate {} repository {repo} from {template}",
                if *private {
                    "the private"
                } else {
                    "the public"
                }
            ),
            CreateStep::Clone => format!("clone it into ./{}", plan.project),
            CreateStep::Substitute { files, .. } => {
                format!(
                    "replace {{{{project}}}} and {{{{domain}}}} in {}",
                    files.join(", ")
                )
            }
            CreateStep::WriteManifest { .. } => "write liftoff.toml".to_owned(),
            CreateStep::AskSecrets { secrets } => format!(
                "secrets {}",
                secrets
                    .iter()
                    .map(|ask| ask.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            CreateStep::WriteCiSnippet { path, .. } => format!("write the CI workflow {path}"),
            CreateStep::Commit { message } => format!("commit \"{message}\""),
            CreateStep::Push => "push".to_owned(),
            CreateStep::CreateProject { .. } => format!("create project {}", plan.project),
            CreateStep::AddEnvironment { name, kind } => {
                format!("add environment {name} ({kind}) on the default branch")
            }
            CreateStep::AddDomain { host, .. } => format!("add domain {host}"),
            CreateStep::Bind {
                service,
                port,
                host,
                ..
            } => format!("bind {host} to {service}:{port}"),
            CreateStep::IssueToken => {
                "issue the deploy token (LIFTOFF_TOKEN goes to CI)".to_owned()
            }
            CreateStep::StoreSecrets { .. } => "store the secrets".to_owned(),
            CreateStep::SyncCi => "sync the CI secrets".to_owned(),
            CreateStep::WriteLiftoffFile => "write .liftoff".to_owned(),
        };
        let _ok = writeln!(text, "  {}", printable(&line));
    }
    text
}

/// What `--json` prints: the plan as the server answered it (`plan`) and
/// what was made.
fn json_run(plan: Value, result: &CreateResultView) -> Result<String, CliError> {
    let result = serde_json::to_value(result)
        .map_err(|error| CliError::Local(format!("cannot encode the answer: {error}")))?;
    let mut document = serde_json::Map::new();
    let _previous = document.insert("plan".to_owned(), plan);
    let _previous = document.insert("result".to_owned(), result);
    Ok(format!("{}\n", Value::Object(document)))
}

// --- With a server -----------------------------------------------------------

/// The server's plan for `request`, as typed and as received. On a
/// terminal a taken domain is asked again and planned with it (CRE-07);
/// in batch mode the `409` ends the run.
async fn server_plan<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    client: &Client,
    request: &mut CreatePlanRequest,
) -> Result<(CreatePlanView, Value), CliError> {
    loop {
        match client
            .send_json(Method::POST, "/git/create-plan", &[], request)
            .await
        {
            Ok(reply) => return Ok((parse(&reply.body)?, parse(&reply.body)?)),
            Err(CliError::Api { message, .. })
                if ctx.mode == Mode::Interactive && taken_domain(&message) =>
            {
                let domain = ctx.text(None, "--domain", &format!("{message}; another domain"))?;
                request.domain = Some(domain);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether the server's `409` says a domain is taken (the core's
/// [`taken_host`]): a name or a repository conflict is not asked again.
fn taken_domain(message: &str) -> bool {
    taken_host(message).is_some()
}

/// The tenant of the new project: `--tenant`, else what `lof tenant use`
/// wrote for the chosen server (CRE-17); `None` leaves it to the server.
fn tenant<S: Sleeper>(ctx: &Ctx<'_, S>, flag: Option<String>) -> Option<String> {
    flag.or_else(|| ctx.chosen.as_ref()?.server.tenant.clone())
}

/// `create` from a template with a server (see the [module
/// documentation](self)).
async fn server<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    args: CreateArgs,
    flags: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    let server_name = ctx
        .chosen
        .as_ref()
        .map(|chosen| chosen.name.clone())
        .ok_or(CliError::NotLoggedIn)?;
    let catalogue = if args.template.is_none() {
        let repos = templates::local_templates(ctx);
        let (catalogue, _body) =
            templates::server_catalogue(&client, args.group.as_deref(), &repos).await?;
        Some(catalogue)
    } else {
        None
    };
    let wanted = gather(ctx, &args, catalogue.as_ref())?;
    ctx.finish()?;
    let dest = destination(ctx, &wanted.name)?;
    let mut request = CreatePlanRequest {
        name: wanted.name,
        template: Some(wanted.template),
        existing: None,
        domain: args.domain.clone(),
        public: wanted.public,
        owner: wanted.owner,
        tenant: tenant(ctx, args.tenant.clone()),
    };
    let (plan, received) = server_plan(ctx, &client, &mut request).await?;
    let known = answer_secrets(ctx, &plan, flags)?;
    confirm_plan(ctx, &plan)?;
    // The git access the clone uses is settled before the forge generates
    // anything: a broken local store fails with nothing made.
    let auth = Box::pin(servers::local_forge_auth(ctx, &plan.git_host)?).await?;
    let reply = client
        .send_json(Method::POST, "/git/generate", &[], &request)
        .await?;
    let generated: GeneratedRepoView = parse(&reply.body)?;
    let (mut done, changed, branch) =
        clone_generated(ctx, &plan, &generated, &dest, auth.as_ref())?;
    let rest = after_clone(&plan);
    let target = plan::Target {
        root: &dest,
        server: &server_name,
        known,
        auth,
        branch,
        changed,
    };
    done.push_str(&plan::run(ctx, &client, &rest, &target).await?);
    let result = CreateResultView {
        project: plan.project.clone(),
        git_host: generated.git_host.clone(),
        repo: generated.repo.clone(),
        url: Some(format!("https://{}", plan.domain)),
    };
    if ctx.json() {
        return ctx.say(&json_run(received, &result)?);
    }
    ctx.say(&format!(
        "{done}\n{} is ready: it answers at https://{} {} (`cd {}`).\n",
        plan.project,
        plan.domain,
        answers_after(&plan, "the first"),
        plan.project
    ))
}

/// When the URL of `plan`'s project answers: after
/// `lof deploy` for a function, else after `which` `git push`.
fn answers_after(plan: &CreatePlanView, which: &str) -> String {
    let function = plan.steps.iter().any(
        |view| matches!(&view.step, CreateStep::AddEnvironment { kind, .. } if kind == "function"),
    );
    if function {
        "after `lof deploy`".to_owned()
    } else {
        format!("after {which} `git push`")
    }
}

/// The local steps for the repository the server's forge generated:
/// cloned into `dest` (with `auth` when the person has a local connection
/// there), `{{project}}` and `{{domain}}` replaced. What was done, the
/// files the substitution changed (the plan's commit holds them) and the
/// default branch of the clone.
fn clone_generated<S: Sleeper>(
    ctx: &Ctx<'_, S>,
    plan: &CreatePlanView,
    generated: &GeneratedRepoView,
    dest: &Path,
    auth: Option<&ForgeAuth>,
) -> Result<(String, Vec<String>, String), CliError> {
    let host = Host::new(&generated.git_host)
        .map_err(|error| CliError::Unexpected(format!("git_host: {error}")))?;
    let repo = RepoRef::new(&generated.repo)
        .map_err(|error| CliError::Unexpected(format!("repo: {error}")))?;
    let git = ctx.env.local.git();
    git.clone_into(&ctx.env.cwd, &clone_url(&host, &repo), dest, auth)
        .map_err(|error| {
            CliError::Local(format!(
                "{}/{} was generated on the forge, but cannot be cloned: {error}. Give this \
                 machine git access to it (`lof git connect` here, or a git credential \
                 helper), clone it and run `lof create` in the clone",
                generated.git_host, generated.repo
            ))
        })?;
    let done = format!(
        "generated {}/{} and cloned it into ./{}\n",
        generated.git_host, generated.repo, plan.project
    );
    let mut changed = Vec::new();
    for view in &plan.steps {
        if let CreateStep::Substitute {
            files,
            project,
            domain,
        } = &view.step
        {
            changed.extend(plan::substitute(
                dest,
                &substitution(files, project, domain)?,
            )?);
        }
    }
    let branch = git.current_branch(dest)?;
    Ok((done, changed, branch))
}

/// `plan` without the steps [`clone_generated`] ran.
fn after_clone(plan: &CreatePlanView) -> CreatePlanView {
    CreatePlanView {
        steps: plan
            .steps
            .iter()
            .filter(|view| {
                !matches!(
                    view.step,
                    CreateStep::GenerateRepo { .. }
                        | CreateStep::Clone
                        | CreateStep::Substitute { .. }
                )
            })
            .cloned()
            .collect(),
        ..plan.clone()
    }
}

/// The `Substitute` step of a plan as the core's [`Substitution`].
fn substitution(files: &[String], project: &str, domain: &str) -> Result<Substitution, CliError> {
    let unexpected = |error: liftoff_core::error::ValidationError| {
        CliError::Unexpected(format!("substitute: {error}"))
    };
    Ok(Substitution {
        files: files
            .iter()
            .map(|file| RepoPath::new(file))
            .collect::<Result<_, _>>()
            .map_err(unexpected)?,
        project: ProjectName::new(project).map_err(unexpected)?,
        domain: Host::new(domain).map_err(unexpected)?,
    })
}

/// `create` in the work tree of `repo` with a server (an existing
/// repository; see the [module documentation](self)).
async fn existing<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    args: CreateArgs,
    repo: &Repository,
    flags: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    refuse_template_flags(&args)?;
    let client = ctx.client()?;
    let server_name = ctx
        .chosen
        .as_ref()
        .map(|chosen| chosen.name.clone())
        .ok_or(CliError::NotLoggedIn)?;
    // `liftoff.toml` lives at the top of the working tree, wherever in it
    // `create` runs.
    let root = work_tree_root(&ctx.env.cwd);
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| repo.name().to_ascii_lowercase());
    let (manifest, detected) = work_tree_manifest(ctx, &root, &args)?;
    let auth = Box::pin(servers::local_forge_auth(ctx, &repo.host)?).await?;
    let branch = servers::ready_work_tree(ctx, &root, auth.as_ref(), "lof create")?;
    let mut request = CreatePlanRequest {
        name,
        existing: Some(ExistingRepoBody {
            git_host: repo.host.clone(),
            repo: repo.repo.clone(),
            manifest,
            detected,
        }),
        domain: args.domain.clone(),
        tenant: tenant(ctx, args.tenant.clone()),
        ..CreatePlanRequest::default()
    };
    let (plan, received) = server_plan(ctx, &client, &mut request).await?;
    let known = answer_secrets(ctx, &plan, flags)?;
    confirm_plan(ctx, &plan)?;
    let target = plan::Target {
        root: &root,
        server: &server_name,
        known,
        auth,
        branch,
        changed: Vec::new(),
    };
    let done = plan::run(ctx, &client, &plan, &target).await?;
    let result = CreateResultView {
        project: plan.project.clone(),
        git_host: plan.git_host.clone(),
        repo: plan.repo.clone(),
        url: Some(format!("https://{}", plan.domain)),
    };
    if ctx.json() {
        return ctx.say(&json_run(received, &result)?);
    }
    ctx.say(&format!(
        "{done}\n{} is a project of {server_name} now: it answers at https://{} {}.\n",
        plan.project,
        plan.domain,
        answers_after(&plan, "the next")
    ))
}

/// The `liftoff.toml` of the work tree `root`, and whether it was
/// detected: the committed one, else what stack detection proposes
/// ([`detect`]), confirmed on a terminal.
/// A batch run that lacks `--service`/`--port` ends here.
fn work_tree_manifest<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    root: &Path,
    args: &CreateArgs,
) -> Result<(String, bool), CliError> {
    let committed = match std::fs::read_to_string(root.join(MANIFEST)) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(CliError::Local(format!("cannot read {MANIFEST}: {error}"))),
    };
    let (manifest, detected) = match committed {
        Some(text) => (text, false),
        None => (detect(ctx, root, args.service.clone(), args.port)?, true),
    };
    ctx.finish()?;
    if detected && ctx.mode == Mode::Interactive {
        ctx.confirm(
            false,
            "--yes",
            &format!(
                "This repository has no {MANIFEST}; detected:\n\n{manifest}\nCommit it as {MANIFEST}?"
            ),
        )?;
    }
    Ok((manifest, detected))
}

/// The `liftoff.toml` stack detection proposes for the work tree `root`, with what the files leave open answered: the stack, on a
/// terminal, among the recognised ones (else the first); the service and
/// port that get the domain by `--service`/`--port`, else picked or typed
/// on a terminal, else missing in batch mode; the build command of a
/// component typed on a terminal. A repository whose files settle
/// nothing is a compose project (a function is recognised by its files);
/// `[dev] command` and
/// the template group stay unasked (`[dev]` is optional, `lof dev` says
/// when it needs one; the group is a template's).
fn detect<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    root: &Path,
    service: Option<String>,
    port: Option<u16>,
) -> Result<String, CliError> {
    let tree = DirTree::new(root);
    let mut detected = StackDetector::detect(&tree);
    let flagged = service.is_some() || port.is_some();
    for question in detected.questions.clone() {
        if !detected.is_open(&question) {
            continue;
        }
        match question {
            Question::Stack { alternatives } if ctx.mode == Mode::Interactive => {
                let names: Vec<String> = alternatives
                    .iter()
                    .map(|alternative| alternative.source.to_owned())
                    .collect();
                let index = ctx
                    .env
                    .prompter
                    .select("Which stack is the project?", &names)?;
                if let Some(alternative) = alternatives.get(index) {
                    detected.choose_stack(alternative);
                }
            }
            Question::Expose { candidates } => {
                let already = matches!(
                    &detected.deploy,
                    Some(Deployable::Service {
                        expose: Some(_),
                        ..
                    })
                );
                if already && !flagged {
                    continue;
                }
                let expose = ask_expose(ctx, &candidates, service.clone(), port)?;
                detected.answer_expose(&tree, expose);
            }
            Question::Build => {
                if ctx.mode == Mode::Batch {
                    return Err(CliError::Usage(format!(
                        "this repository builds a component, and its build command is not known: write `[function] build` into {MANIFEST} and run `lof create` again"
                    )));
                }
                let build = ctx.text(None, "build", "Command that builds the component")?;
                detected.answer_build(build);
            }
            Question::Stack { .. } | Question::Kind | Question::DevCommand | Question::Group => {}
        }
    }
    if flagged
        && matches!(detected.deploy, Some(Deployable::Service { .. }))
        && !detected
            .questions
            .iter()
            .any(|question| matches!(question, Question::Expose { .. }))
    {
        let expose = ask_expose(ctx, &[], service, port)?;
        detected.answer_expose(&tree, expose);
    }
    // In batch mode a missing `--service`/`--port` is reported by
    // `finish`; until then the proposal stands in without `expose`.
    let manifest = detected.manifest().ok_or_else(|| {
        CliError::Usage(format!(
            "the files of this repository do not tell how it deploys: write {MANIFEST} and run `lof create` again"
        ))
    })?;
    Ok(manifest.to_toml())
}

/// The service and port that get the domain: `--service`/`--port`, else
/// one of `candidates` picked on a terminal, else typed (or missing in
/// batch mode).
fn ask_expose<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    candidates: &[Expose],
    service: Option<String>,
    port: Option<u16>,
) -> Result<Option<Expose>, CliError> {
    if service.is_none()
        && port.is_none()
        && ctx.mode == Mode::Interactive
        && !candidates.is_empty()
    {
        let names: Vec<String> = candidates
            .iter()
            .map(|expose| format!("{}:{}", expose.service, expose.port.get()))
            .collect();
        let index = ctx
            .env
            .prompter
            .select("Service and port that get the domain", &names)?;
        return Ok(candidates.get(index).cloned());
    }
    let service = ctx.text(service, "--service", "Compose service that serves HTTP")?;
    let port: u16 = ctx.parsed(port, "--port", "Port it listens on in its container")?;
    if ctx.mode == Mode::Batch && (service.is_empty() || port == 0) {
        return Ok(None);
    }
    Ok(Some(Expose {
        service: ServiceName::new(&service)
            .map_err(|error| CliError::Usage(format!("--service: {error}")))?,
        port: ContainerPort::new(port)
            .map_err(|error| CliError::Usage(format!("--port: {error}")))?,
    }))
}

// --- Without a server --------------------------------------------------------

/// `create` from a template without a server (see the [module
/// documentation](self)).
async fn local<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    args: CreateArgs,
    flags: &BTreeMap<String, String>,
) -> Result<(), CliError> {
    if let Some(name) = &args.name {
        let _dest = destination(ctx, name)?;
    }
    let local = ctx.local_mode().await?;
    let catalogue = if args.template.is_none() {
        let repos = templates::local_templates(ctx);
        Some(templates::local_catalogue(&local, args.group.as_deref(), &repos).await?)
    } else {
        None
    };
    let wanted = gather(ctx, &args, catalogue.as_ref())?;
    ctx.finish()?;
    let dest = destination(ctx, &wanted.name)?;
    let name = ProjectName::new(&wanted.name)
        .map_err(|error| CliError::Usage(format!("<name>: {error}")))?;
    let template = TemplateSource::new(&wanted.template)
        .map_err(|error| CliError::Usage(format!("--template: {error}")))?;
    let options = local_options(&args, &wanted)?;
    let create = CreateService::new(Arc::clone(&local.ports), Arc::clone(&local.settings));
    let plan = create
        .plan_from_template(&local.actor, name.clone(), template.clone(), options)
        .await?;
    let planned = view::create_plan(&plan);
    let known = answer_secrets(ctx, &planned, flags)?;
    confirm_plan(ctx, &planned)?;
    let answers = known
        .into_iter()
        .map(|((_, key), value)| {
            SecretKey::new(&key)
                .map(|key| (key, SecretValue::new(value)))
                .map_err(|error| CliError::Unexpected(error.to_string()))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let repo = create.generate(&local.actor, &plan).await?;
    let access = local.forge_access(&plan.git_host).await?.ok_or_else(|| {
        CliError::Local(format!(
            "no connection to {}: run `lof git connect` first",
            plan.git_host
        ))
    })?;
    let provider = access.provider.as_str();
    let auth = ForgeAuth::of(&access);
    let mut done = format!(
        "generated {}/{repo} from {}\n",
        plan.git_host, template.repo
    );
    let work = LocalWork {
        plan: &plan,
        repo: &repo,
        dest: &dest,
        auth: &auth,
        answers: &answers,
    };
    done.push_str(&run_local_steps(ctx, &local, &work)?);
    if ctx.json() {
        let run = CreateRunView {
            plan: planned,
            result: CreateResultView {
                project: name.to_string(),
                git_host: plan.git_host.to_string(),
                repo: repo.to_string(),
                url: None,
            },
        };
        return ctx.say_json_or(&run, |_| String::new());
    }
    ctx.say(&format!(
        "{done}\nThere is no server yet: {name} is ready for `lof server connect`.\n\
         Next: `lof login <url>` and `lof git connect {provider}` on that server (its plan \
         uses your forge connection there), then `cd {name}` and `lof server connect <url>`; \
         its other secrets follow with `lof secret push --move`.\n"
    ))
}

/// `create` in the work tree of `repo` without a server (steps 1–3 are `git remote` and `liftoff.toml`, and without a server `create` stops there). A committed manifest is
/// ready as it is; a detected one, confirmed on a terminal, is written at
/// the top of the work tree, committed alone and pushed to the default
/// branch: the core's [`local_existing_steps`], run in order.
/// `lof server connect` makes it a project of a server.
async fn local_existing<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    args: &CreateArgs,
    repo: &Repository,
) -> Result<(), CliError> {
    refuse_template_flags(args)?;
    refuse_server_flags(args)?;
    let root = work_tree_root(&ctx.env.cwd);
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| repo.name().to_ascii_lowercase());
    let name =
        ProjectName::new(&name).map_err(|error| CliError::Usage(format!("<name>: {error}")))?;
    let (manifest, detected) = work_tree_manifest(ctx, &root, args)?;
    let manifest = ProjectManifest::parse(&manifest)
        .map_err(|error| CliError::Usage(format!("{MANIFEST}: {error}")))?;
    let manifest = if detected {
        ExistingManifest::Detected(manifest)
    } else {
        ExistingManifest::Committed(manifest)
    };
    let steps = local_existing_steps(&manifest);
    let mut done = String::new();
    if !steps.is_empty() {
        let auth = Box::pin(servers::local_forge_auth(ctx, &repo.host)?).await?;
        let branch = servers::ready_work_tree(ctx, &root, auth.as_ref(), "lof create")?;
        let git = ctx.env.local.git();
        for step in &steps {
            match step {
                Step::WriteManifest { content } => {
                    // A symlinked `liftoff.toml` is not written through.
                    let path = RepoPath::new(MANIFEST)
                        .map_err(|error| CliError::Unexpected(error.to_string()))?;
                    let file = inside(&root, &path)?.ok_or_else(|| {
                        CliError::Local(format!(
                            "{MANIFEST} of the work tree is a symlink, and `lof create` writes it: make it a file; nothing was changed"
                        ))
                    })?;
                    std::fs::write(file, content)?;
                    let _ok = writeln!(done, "wrote {MANIFEST}");
                }
                // The commit holds `liftoff.toml` alone: the rest of the
                // work tree is the person's.
                Step::Commit { message } => {
                    if git.commit_paths(&root, &[MANIFEST.to_owned()], message)? {
                        let _ok = writeln!(done, "committed \"{message}\"");
                    }
                }
                Step::Push => {
                    git.push_to(&root, &branch, auth.as_ref())?;
                    let _ok = writeln!(done, "pushed to {branch}");
                }
                other => {
                    return Err(CliError::Unexpected(format!(
                        "an existing repository's local step {other:?}"
                    )));
                }
            }
        }
    }
    if ctx.json() {
        let result = CreateResultView {
            project: name.to_string(),
            git_host: repo.host.clone(),
            repo: repo.repo.clone(),
            url: None,
        };
        return ctx.say(&json_run(Value::Null, &result)?);
    }
    ctx.say(&format!(
        "{done}There is no server yet: {name} is ready for `lof server connect`.\n\
         Next: `lof login <url>` and `lof git connect` on that server (its plan uses your \
         forge connection there), then `lof server connect <url>` here.\n"
    ))
}

/// `--secret`, `--domain` and `--tenant` give the server's plan, which
/// an existing repository without a server does not have:
/// `lof server connect` plans it.
fn refuse_server_flags(args: &CreateArgs) -> Result<(), CliError> {
    let given: Vec<&str> = [
        (!args.secrets.is_empty(), "--secret"),
        (args.domain.is_some(), "--domain"),
        (args.tenant.is_some(), "--tenant"),
    ]
    .into_iter()
    .filter_map(|(given, flag)| given.then_some(flag))
    .collect();
    if given.is_empty() {
        return Ok(());
    }
    Err(CliError::Usage(format!(
        "{} apply to the project on a server; without one, `lof create` here only commits {MANIFEST} (`lof server connect` plans the rest)",
        given.join(", ")
    )))
}

/// The options of local mode's plan, validated as the server validates
/// its request.
fn local_options(args: &CreateArgs, wanted: &Wanted) -> Result<CreateOptions, CliError> {
    Ok(CreateOptions {
        domain: args
            .domain
            .as_deref()
            .map(Host::new)
            .transpose()
            .map_err(|error| CliError::Usage(format!("--domain: {error}")))?,
        public: wanted.public,
        owner: wanted
            .owner
            .as_deref()
            .map(Login::new)
            .transpose()
            .map_err(|error| CliError::Usage(format!("--owner: {error}")))?,
        tenant: args
            .tenant
            .as_deref()
            .map(TenantSlug::new)
            .transpose()
            .map_err(|error| CliError::Usage(format!("--tenant: {error}")))?,
    })
}

/// What the local steps of a template's plan work on.
struct LocalWork<'a> {
    plan: &'a CreatePlan,
    /// The repository the forge generated.
    repo: &'a RepoRef,
    /// `./<name>`.
    dest: &'a Path,
    auth: &'a ForgeAuth,
    /// The values of `AskSecrets`.
    answers: &'a BTreeMap<SecretKey, SecretValue>,
}

/// Runs the local steps of `work.plan` up to `Push`, in order; what they
/// did, one line each.
fn run_local_steps<S: Sleeper>(
    ctx: &Ctx<'_, S>,
    local: &LocalMode,
    work: &LocalWork<'_>,
) -> Result<String, CliError> {
    let LocalWork {
        plan,
        repo,
        dest,
        auth,
        answers,
    } = *work;
    let git = ctx.env.local.git();
    let mut done = String::new();
    for step in plan
        .steps
        .iter()
        .take_while(|step| step.side() != Side::Server)
    {
        match step {
            Step::Clone => {
                let url = clone_url(&plan.git_host, repo);
                git.clone_into(&ctx.env.cwd, &url, dest, Some(auth))?;
                let _ok = writeln!(done, "cloned it into ./{}", plan.project);
            }
            Step::Substitute(substitution) => {
                for file in &substitution.files {
                    // A template's symlink is not followed out of `./<name>`.
                    let Some(path) = inside(dest, file)? else {
                        continue;
                    };
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    if let Some(replaced) = substitution.apply_project_to(file, &text) {
                        std::fs::write(&path, replaced)?;
                    }
                }
            }
            Step::WriteManifest { content } => {
                // Nor is a template's symlinked `liftoff.toml`.
                let manifest = RepoPath::new(MANIFEST)
                    .map_err(|error| CliError::Unexpected(error.to_string()))?;
                let path = inside(dest, &manifest)?.ok_or_else(|| {
                    CliError::Local(format!(
                        "the template's {MANIFEST} is a symlink; it was not written through"
                    ))
                })?;
                std::fs::write(path, content)?;
            }
            Step::AskSecrets(asks) => {
                let store = local.ports.store();
                for ask in asks {
                    if let Some(value) = answers.get(&ask.key) {
                        let scope = plan_secret_scope(&plan.project, ask.scope)?;
                        let _secret =
                            store.set(&scope, &ask.key, value, ask.ci, SystemClock.now())?;
                    }
                }
                let _ok = writeln!(
                    done,
                    "kept {} secrets in the local store (`lof secret ls`)",
                    asks.len()
                );
            }
            Step::Commit { message } => {
                let _committed = git.commit_all(dest, message.as_str())?;
            }
            Step::Push => {
                git.push(dest, Some(auth))?;
                let _ok = writeln!(done, "pushed to {}/{repo}", plan.git_host);
            }
            // The plan of a template generates first (`generate`); the CI
            // workflow names the server's API, written by `server
            // connect` (see the module documentation).
            _ => {}
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod taken_domain {
        use super::*;

        // TC: CRE-07 (the `detail` of the server's `409`, as the core's
        // `DomainService` words it)
        #[test]
        fn recognises_taken_host_when_server_says_so() {
            assert!(taken_domain("demo.apps.example.com is taken"));
        }

        // TC: CRE-07 (a name conflict is not a domain's)
        #[test]
        fn ignores_project_conflict_when_name_taken() {
            assert!(!taken_domain("project demo already exists"));
        }
    }

    mod secret_flags {
        use super::*;

        // TC: CRE-12
        #[test]
        fn keeps_equals_signs_of_value_when_parsed() {
            assert_eq!(
                secret_flags(&["URL=postgres://a?b=c".to_owned()]).ok(),
                Some(BTreeMap::from([(
                    "URL".to_owned(),
                    "postgres://a?b=c".to_owned()
                )]))
            );
        }

        // TC: CRE-12
        #[test]
        fn refuses_pair_without_key_when_parsed() {
            assert!(matches!(
                secret_flags(&["=x".to_owned()]),
                Err(CliError::Usage(_))
            ));
        }
    }
}
