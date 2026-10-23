//! [`CreateService`]: `lof create` as a plan (test cases CRE-01…CRE-10).
//!
//! [`CreateService::plan_from_template`] and
//! [`CreateService::plan_for_existing`]
//! build a [`CreatePlan`]: the ordered [`Step`]s of a create, each with the
//! data it needs, validated against the store when it is built (a taken
//! project name, repository or domain is [`Error::Conflict`] before any
//! step runs). The plan is executed in parts ([`Step::side`]):
//!
//! - [`Side::Forge`] (`GenerateRepo`) through the [`Git`](crate::ports::Git)
//!   port as the actor's own connection, wherever that connection lives
//!   (the CLI's wasm host in local mode);
//! - [`Side::Local`] (`Clone`, `Substitute`, `WriteManifest`,
//!   `AskSecrets`, `WriteCiSnippet`, `Commit`, `Push`, `WriteLiftoffFile`)
//!   by the CLI in the working tree;
//! - [`Side::Server`] (`CreateProject` … `SyncCi`) by
//!   [`CreateService::execute`] over the installation's services, with
//!   what the local steps found out ([`LocalOutcome`]); in local mode the
//!   plan stops after `Push` (LCL-03).
//!
//! Secret values are never part of a plan: `AskSecrets` names the keys,
//! [`answer_secrets`] fills them (`--secret K=V`, `generate`), and they
//! reach [`CreateService::execute`] through [`LocalOutcome::secrets`].
//!
//! A step is present only when it has something to do: `Substitute` with
//! a non-empty `[substitute]`, `WriteManifest` for a detected manifest,
//! `AskSecrets` and `StoreSecrets` with a non-empty `[secrets]` or a
//! registry login to ask for (a compose project on Forgejo), `Bind` with
//! `[service] expose`.
//!
//! The plan holds domain types; its wire form (`--json`, the answer of
//! `POST /git/create-plan` in server mode) is `liftoff-api`'s
//! `CreatePlanView`, which the adapters translate to.

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::binding::{BindRequest, BindingService};
use super::deploy::DeployService;
use super::domain::DomainService;
use super::environment::EnvironmentService;
use super::project::ProjectService;
use super::secret_sync::{SecretSync, SyncReport};
use super::secrets::{CI_ONLY_KEYS, SYSTEM_VARS, SecretsService};
use super::settings::Settings;
use super::support::open_connection;
use crate::domain::{
    Binding, Branch, DeployToken, Domain, EnvKind, EnvName, Environment, GitConnection,
    GitProvider, Host, Label, Login, NewProject, Project, ProjectName, RepoRef, ScopeKind,
    ScopeRef, SecretKey, SecretScope, SecretValue, TemplateRef, TemplateSource, TenantId,
    TenantSlug, User,
};
use crate::error::Error;
use crate::manifest::{Deployable, Generate, ProjectManifest, RepoPath};
use crate::ports::{Git as _, GitAccess, NewRepo, Ports, RepoRole, Store as _};
use crate::snippet::{CiSnippet, REGISTRY_TOKEN_KEY, REGISTRY_USER_KEY, SnippetTarget};

#[cfg(test)]
mod tests;

/// The commit message of the `Commit` step.
pub const COMMIT_MESSAGE: &str = "chore: set up liftoff";

/// The commit message of a detected `liftoff.toml` committed alone,
/// without a server ([`local_existing_steps`]).
pub const ADD_MANIFEST_MESSAGE: &str = "chore: add liftoff.toml";

/// The environment a plan adds.
const PROD: &str = "prod";

/// What `lof create` was told besides the name and the template.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CreateOptions {
    /// `--domain`: the domain instead of `<name>.<base>`.
    pub domain: Option<Host>,
    /// `--public`; private by default.
    pub public: bool,
    /// `--owner`: the user or organisation the repository is generated
    /// under; the actor's account on the forge by default.
    pub owner: Option<Login>,
    /// `--tenant`: the tenant of the new project.
    pub tenant: Option<TenantSlug>,
}

/// A repository `lof create` runs in:
/// what the CLI read from `git remote` and the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingRepo {
    /// The forge host of `origin`.
    pub git_host: Host,
    /// `owner/name` of `origin`.
    pub repo: RepoRef,
    /// Its `liftoff.toml`, committed or proposed by stack detection.
    pub manifest: ExistingManifest,
}

/// Where the manifest of an existing repository comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingManifest {
    /// The repository has a `liftoff.toml`.
    Committed(ProjectManifest),
    /// It has none: what [`StackDetector`](crate::detect::StackDetector)
    /// proposed and the person confirmed; the plan writes and commits it
    /// (CRE-09).
    Detected(ProjectManifest),
}

/// Who executes a [`Step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The forge, through the actor's own connection.
    Forge,
    /// The CLI, in the working tree.
    Local,
    /// The installation ([`CreateService::execute`]).
    Server,
}

/// The kind of the environment `prod` a plan adds, from the manifest's
/// `[service]` or `[function]` (CRE-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeployKind {
    /// `[service]`: a compose environment.
    Compose,
    /// `[function]`: a function environment.
    Function,
}

/// One entry of `[secrets]` as the `AskSecrets` step asks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretAsk {
    /// The key.
    pub key: SecretKey,
    /// `hint`, shown with the question.
    pub hint: Option<String>,
    /// `generate`: the CLI makes the value instead of asking.
    pub generate: Option<Generate>,
    /// Where `StoreSecrets` stores it: the manifest's `scope`, `project`
    /// when it has none.
    pub scope: ScopeKind,
    /// Stored with `ci = true` and sent to the forge's CI by `SyncCi`: the
    /// registry login a plan asks itself, never a key of `[secrets]`.
    pub ci: bool,
}

/// Where the `StoreSecrets` step stores one value `AskSecrets` collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretPlace {
    /// The key.
    pub key: SecretKey,
    /// Its scope (`prod` for `environment`).
    pub scope: ScopeKind,
    /// Stored with `ci = true` (a project secret).
    pub ci: bool,
}

/// The `Substitute` step: `{{project}}` and `{{domain}}` replaced in the
/// files `[substitute] files` lists, and only there (CRE-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Substitution {
    /// `[substitute] files`, in manifest order.
    pub files: Vec<RepoPath>,
    /// What `{{project}}` becomes.
    pub project: ProjectName,
    /// What `{{domain}}` becomes.
    pub domain: Host,
}

/// One step of a [`CreatePlan`], in the documented order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Generates `target` from the template repository `template` on the
    /// plan's forge host.
    GenerateRepo {
        /// The template repository.
        template: RepoRef,
        /// The repository to create.
        target: NewRepo,
    },
    /// Clones the plan's repository into `./<project>`.
    Clone,
    /// Replaces the placeholders in the listed files.
    Substitute(Substitution),
    /// Writes `liftoff.toml` with `content` (a detected manifest, CRE-09).
    WriteManifest {
        /// The manifest as `liftoff.toml` text.
        content: String,
    },
    /// Asks for (or generates) the values of `[secrets]`, by key.
    AskSecrets(Vec<SecretAsk>),
    /// Writes the CI workflow, unless the repository already has the file.
    WriteCiSnippet(CiSnippet),
    /// Commits what the steps before changed, when they changed anything.
    Commit {
        /// The commit message ([`COMMIT_MESSAGE`]).
        message: String,
    },
    /// Pushes the default branch to `origin`.
    Push,
    /// Creates the project ([`ProjectService::create`](super::ProjectService::create)).
    CreateProject {
        /// The project.
        project: NewProject,
        /// The template it comes from, when it does.
        template: Option<TemplateRef>,
    },
    /// Adds the environment `name`, pinned to the repository's default
    /// branch ([`LocalOutcome::default_branch`]) by the actor (CRE-06).
    AddEnvironment {
        /// `prod`.
        name: EnvName,
        /// From the manifest.
        kind: DeployKind,
    },
    /// Adds the domain `host`: managed when it is `<label>.<base>`,
    /// external otherwise (CRE-07).
    AddDomain {
        /// `<name>.<base>` or `--domain`.
        host: Host,
        /// Its label when it is `<label>.<base>` (a managed domain);
        /// `None` for an external one. The plan says which, so whoever
        /// runs the step (the CLI through `POST /domains`) need not know
        /// the base domain.
        label: Option<Label>,
    },
    /// Binds the domain to `[service] expose` on `env`.
    Bind {
        /// `prod`.
        env: EnvName,
        /// The service, port and domain.
        request: BindRequest,
    },
    /// Issues the deploy token; `LIFTOFF_TOKEN` is stored with `ci = true`
    /// and sent to the forge (CRE-08; [`DeployService::rotate_token`]).
    IssueToken,
    /// Stores the values of `AskSecrets`, each key in its scope (`prod`
    /// for `environment`) with its `ci` flag; the registry login of a
    /// compose project on Forgejo is `ci`, which `SyncCi` sends.
    StoreSecrets(Vec<SecretPlace>),
    /// Syncs the `ci = true` secrets to the forge ([`SecretSync`]).
    SyncCi,
    /// Writes `.liftoff` and adds it to `.gitignore`.
    WriteLiftoffFile,
}

impl Step {
    /// Who executes the step.
    #[must_use]
    pub const fn side(&self) -> Side {
        match self {
            Self::GenerateRepo { .. } => Side::Forge,
            Self::Clone
            | Self::Substitute(_)
            | Self::WriteManifest { .. }
            | Self::AskSecrets(_)
            | Self::WriteCiSnippet(_)
            | Self::Commit { .. }
            | Self::Push
            | Self::WriteLiftoffFile => Side::Local,
            Self::CreateProject { .. }
            | Self::AddEnvironment { .. }
            | Self::AddDomain { .. }
            | Self::Bind { .. }
            | Self::IssueToken
            | Self::StoreSecrets(_)
            | Self::SyncCi => Side::Server,
        }
    }
}

/// What `lof create` will do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePlan {
    /// The project's name.
    pub project: ProjectName,
    /// The forge host of its repository.
    pub git_host: Host,
    /// Its repository.
    pub repo: RepoRef,
    /// Its domain.
    pub domain: Host,
    /// The steps, in order.
    pub steps: Vec<Step>,
}

/// What the local steps of a plan found out, for its server steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalOutcome {
    /// The repository's default branch (`prod` is pinned to it).
    pub default_branch: Branch,
    /// The values `AskSecrets` collected ([`answer_secrets`]), by key.
    pub secrets: BTreeMap<SecretKey, SecretValue>,
}

/// What [`CreateService::execute`] created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    /// The project.
    pub project: Project,
    /// `prod`.
    pub environment: Environment,
    /// The domain.
    pub domain: Domain,
    /// The binding, when the manifest exposes a service.
    pub binding: Option<Binding>,
    /// The live deploy token.
    pub token: DeployToken,
    /// What `SyncCi` did.
    pub sync: SyncReport,
    /// The tenant secrets of `StoreSecrets` that already existed: kept as
    /// they were, the template's values not stored.
    pub kept: Vec<SecretKey>,
}

/// The repository [`CreateService::generate_from_template`] generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedRepo {
    /// Its forge host.
    pub git_host: Host,
    /// `owner/name`.
    pub repo: RepoRef,
}

/// What `lof create` does in an existing repository without a server.
///
/// it stops after step 3, and
/// `lof server connect` plans the rest. A committed manifest is ready as
/// it is, nothing to do; a detected one is written at the top of the work
/// tree, committed alone ([`ADD_MANIFEST_MESSAGE`]) and pushed to the
/// default branch. The CLI runs the steps in order.
#[must_use]
pub fn local_existing_steps(manifest: &ExistingManifest) -> Vec<Step> {
    match manifest {
        ExistingManifest::Committed(_) => Vec::new(),
        ExistingManifest::Detected(manifest) => vec![
            Step::WriteManifest {
                content: manifest.to_toml(),
            },
            Step::Commit {
                message: ADD_MANIFEST_MESSAGE.to_owned(),
            },
            Step::Push,
        ],
    }
}

/// Required secrets that have no value in batch mode (CRE-04): the CLI
/// exits 64 listing them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSecrets(pub Vec<SecretKey>);

/// `missing values for the secrets A, B`, the keys in order.
impl core::fmt::Display for MissingSecrets {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("missing values for the secrets ")?;
        for (index, key) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{key}")?;
        }
        Ok(())
    }
}

impl core::error::Error for MissingSecrets {}

/// The value `generate = "random:<n>"` makes: `n` bytes from `random` (a
/// CSPRNG in the CLI), encoded as URL-safe base64 without padding
/// (CRE-03).
#[must_use]
pub fn generate_value(generate: Generate, random: impl FnOnce(&mut [u8])) -> SecretValue {
    let Generate::Random(length) = generate;
    let mut bytes = vec![0_u8; usize::from(length)];
    random(&mut bytes);
    SecretValue::new(URL_SAFE_NO_PAD.encode(&bytes))
}

/// The values of `asks`: the one `given` (`--secret K=V`, or what the
/// person typed) for a key, otherwise a generated one when the key has
/// `generate`.
///
/// # Errors
///
/// [`MissingSecrets`] listing, by key, every ask without `generate` that
/// `given` has no value for (CRE-04).
pub fn answer_secrets(
    asks: &[SecretAsk],
    mut given: BTreeMap<SecretKey, SecretValue>,
    mut random: impl FnMut(&mut [u8]),
) -> Result<BTreeMap<SecretKey, SecretValue>, MissingSecrets> {
    let mut answers = BTreeMap::new();
    let mut missing = Vec::new();
    for ask in asks {
        let answer = match (given.remove(&ask.key), ask.generate) {
            (Some(value), _) => value,
            (None, Some(generate)) => generate_value(generate, &mut random),
            (None, None) => {
                missing.push(ask.key.clone());
                continue;
            }
        };
        let _previous: Option<SecretValue> = answers.insert(ask.key.clone(), answer);
    }
    if missing.is_empty() {
        Ok(answers)
    } else {
        missing.sort();
        missing.dedup();
        Err(MissingSecrets(missing))
    }
}

impl Substitution {
    /// `text` with every `{{project}}` and `{{domain}}` replaced; other
    /// braces are left as they are.
    #[must_use]
    pub fn apply(&self, text: &str) -> String {
        text.replace("{{project}}", self.project.as_str())
            .replace("{{domain}}", self.domain.as_str())
    }

    /// [`Substitution::apply`] of `text` when `path` is one of the listed
    /// files; `None` (leave the file alone) otherwise (CRE-05).
    #[must_use]
    pub fn apply_to(&self, path: &RepoPath, text: &str) -> Option<String> {
        self.files.contains(path).then(|| self.apply(text))
    }

    /// [`Substitution::apply_to`] before the project has a domain: in
    /// local mode (LCL-03) the plan stops before `AddDomain`, so only
    /// `{{project}}` is replaced and `{{domain}}` stays for the server.
    #[must_use]
    pub fn apply_project_to(&self, path: &RepoPath, text: &str) -> Option<String> {
        self.files
            .contains(path)
            .then(|| text.replace("{{project}}", self.project.as_str()))
    }
}

/// The scope a secret of `kind` of project `project` is stored in by a
/// plan: `env` for an environment.
fn scope_of(project: &ProjectName, env: &EnvName, kind: ScopeKind) -> ScopeRef {
    match kind {
        ScopeKind::Tenant => ScopeRef::Tenant(project.clone()),
        ScopeKind::Project => ScopeRef::Project(project.clone()),
        ScopeKind::Environment => ScopeRef::Environment(project.clone(), env.clone()),
    }
}

/// The scope `StoreSecrets` stores a secret of `kind` of `project` in.
///
/// An environment secret goes to `prod`, the environment the plan adds.
/// Local mode keeps the values of `AskSecrets` there too (LCL-03), so `lof
/// server connect` finds them where the plan names them.
///
/// # Errors
///
/// Only if `prod` is no environment name (a bug).
pub fn plan_secret_scope(project: &ProjectName, kind: ScopeKind) -> Result<ScopeRef, Error> {
    Ok(scope_of(project, &EnvName::new(PROD)?, kind))
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct CreateService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for CreateService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> CreateService<P> {
    /// A service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// The plan that creates project `name` from `template`:
    /// the template's `liftoff.toml` is read through the actor's own
    /// connection on the template's host, the repository is
    /// `<owner>/<name>` on that host.
    ///
    /// `authorize` is asked [`Action::ProjectCreate`](crate::ports::Action::ProjectCreate)
    /// as [`ProjectService::create`](super::ProjectService::create) asks
    /// it.
    ///
    /// # Errors
    ///
    /// - [`Error::Conflict`] when the name, the repository or the domain is
    ///   taken (CRE-02, CRE-07);
    /// - [`Error::Forbidden`] when `authorize` refuses or the actor has no
    ///   connection on the template's host;
    /// - [`Error::Validation`] on `manifest` when the template has no
    ///   valid `liftoff.toml`;
    /// - whatever the forge or the store report.
    pub async fn plan_from_template(
        &self,
        actor: &User,
        name: ProjectName,
        template: TemplateSource,
        options: CreateOptions,
    ) -> Result<CreatePlan, Error> {
        // `authorize` first: a refusal reaches no forge.
        let projects = ProjectService::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        let tenant = projects
            .authorize_create(actor, options.tenant.as_ref())
            .await?;
        let (access, text) = self.read_template(actor, &template).await?;
        let owner = options
            .owner
            .clone()
            .unwrap_or_else(|| access.login.clone());
        let repo = RepoRef::new(&format!("{owner}/{name}"))?;
        let manifest = ProjectManifest::parse(&text)?;
        let new = NewProject {
            tenant: options.tenant.clone(),
            name,
            git_host: access.host.clone(),
            repo,
            manifest: Some(manifest.to_toml()),
        };
        projects.admit_in(tenant, &new).await?;
        let domain = self.admit_domain(&new.name, options.domain.clone()).await?;
        let mut steps = vec![
            Step::GenerateRepo {
                template: template.repo,
                target: NewRepo {
                    repo: new.repo.clone(),
                    private: !options.public,
                },
            },
            Step::Clone,
        ];
        if !manifest.substitute.is_empty() {
            steps.push(Step::Substitute(Substitution {
                files: manifest.substitute.clone(),
                project: new.name.clone(),
                domain: domain.clone(),
            }));
        }
        let template_ref = manifest
            .template
            .as_ref()
            .map(|info| TemplateRef::new(info.group, &info.name))
            .transpose()?;
        self.finish_plan(steps, &manifest, new, (template_ref, &access.login), domain)
            .await
    }

    /// The plan that makes the existing repository `existing` project
    /// `name`.
    ///
    /// # Errors
    ///
    /// - [`Error::Conflict`] when the repository already is a project,
    ///   naming it (CRE-10), or the name or the domain is taken;
    /// - [`Error::Forbidden`] when `authorize` refuses;
    /// - whatever the store reports.
    pub async fn plan_for_existing(
        &self,
        actor: &User,
        name: ProjectName,
        existing: ExistingRepo,
        options: CreateOptions,
    ) -> Result<CreatePlan, Error> {
        let ExistingRepo {
            git_host,
            repo,
            manifest,
        } = existing;
        let (manifest, detected) = match manifest {
            ExistingManifest::Committed(manifest) => (manifest, false),
            ExistingManifest::Detected(manifest) => (manifest, true),
        };
        let new = NewProject {
            tenant: options.tenant.clone(),
            name,
            git_host,
            repo,
            manifest: Some(manifest.to_toml()),
        };
        let _tenant = ProjectService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
            .admit(actor, &new)
            .await?;
        let domain = self.admit_domain(&new.name, options.domain.clone()).await?;
        let login = self
            .ensure_repo_admin(actor, &new.git_host, &new.repo)
            .await?;
        let mut steps = Vec::new();
        if detected {
            steps.push(Step::WriteManifest {
                content: manifest.to_toml(),
            });
        }
        self.finish_plan(steps, &manifest, new, (None, &login), domain)
            .await
    }

    /// Runs the [`Side::Forge`] step of `plan` (`GenerateRepo`) as `actor`, through their own connection on the plan's
    /// forge host: the repository the forge created. A plan without that
    /// step (an existing repository) generates nothing and answers its
    /// repository.
    ///
    /// The CLI runs it before the local steps, in local mode over its own
    /// ports (LCL-03). `authorize` is asked
    /// [`Action::ProjectCreate`](crate::ports::Action::ProjectCreate) as
    /// the plan asked it, before anything reaches the forge.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] when `authorize` refuses or the actor has no
    /// connection on the plan's host; whatever the forge reports.
    pub async fn generate(&self, actor: &User, plan: &CreatePlan) -> Result<RepoRef, Error> {
        let Some((template, target)) = plan.steps.iter().find_map(|step| match step {
            Step::GenerateRepo { template, target } => Some((template, target)),
            _ => None,
        }) else {
            return Ok(plan.repo.clone());
        };
        let tenant = plan.steps.iter().find_map(|step| match step {
            Step::CreateProject { project, .. } => project.tenant.as_ref(),
            _ => None,
        });
        let _tenant = ProjectService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
            .authorize_create(actor, tenant)
            .await?;
        let connection = self
            .ports
            .store()
            .git_connection(actor.id, &plan.git_host)
            .await?
            .ok_or_else(|| no_connection(&plan.git_host))?;
        let access = open_connection(self.ports.as_ref(), &connection)?;
        self.ports
            .git()
            .generate_from_template(&access, template, target)
            .await
    }

    /// Generates the repository of the plan that creates project `name`
    /// from `template` ([`CreateService::plan_from_template`], then
    /// [`CreateService::generate`]): what `POST /git/generate` does in
    /// server mode, where the forge connection is the server's. The plan is built again, so a name, repository or
    /// domain taken since the person saw it is still a conflict, and
    /// nothing reaches the forge then.
    ///
    /// # Errors
    ///
    /// Those of [`CreateService::plan_from_template`] and
    /// [`CreateService::generate`].
    pub async fn generate_from_template(
        &self,
        actor: &User,
        name: ProjectName,
        template: TemplateSource,
        options: CreateOptions,
    ) -> Result<GeneratedRepo, Error> {
        let plan = self
            .plan_from_template(actor, name, template, options)
            .await?;
        let repo = self.generate(actor, &plan).await?;
        Ok(GeneratedRepo {
            git_host: plan.git_host,
            repo,
        })
    }

    /// Executes the [`Side::Server`] steps of `plan` as `actor`, in order,
    /// through the services that own them (each asks `authorize`).
    ///
    /// # Errors
    ///
    /// - [`Error::Validation`] on `secrets` naming the keys of
    ///   `StoreSecrets` that `local` has no value for; on `kind` for a
    ///   function plan (environments of kind `function` arrive in phase
    ///   4); both before any step runs;
    /// - whatever the step's service reports; the steps before it stay
    ///   done.
    pub async fn execute(
        &self,
        actor: &User,
        plan: &CreatePlan,
        local: LocalOutcome,
    ) -> Result<Created, Error> {
        let missing: Vec<SecretKey> = plan
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::StoreSecrets(places) => Some(places),
                _ => None,
            })
            .flatten()
            .filter(|place| !local.secrets.contains_key(&place.key))
            .map(|place| place.key.clone())
            .collect();
        if !missing.is_empty() {
            return Err(Error::invalid(
                "secrets",
                MissingSecrets(missing).to_string(),
            ));
        }
        let mut done = Done::default();
        for step in plan.steps.iter().filter(|step| step.side() == Side::Server) {
            self.run(actor, plan, &local, step, &mut done).await?;
        }
        let incomplete = || Error::internal_msg("a create plan without its server steps");
        Ok(Created {
            project: done.project.ok_or_else(incomplete)?,
            environment: done.environment.ok_or_else(incomplete)?,
            domain: done.domain.ok_or_else(incomplete)?,
            binding: done.binding,
            token: done.token.ok_or_else(incomplete)?,
            sync: done.sync.ok_or_else(incomplete)?,
            kept: done.kept,
        })
    }

    /// Runs the server step `step` of `plan`, keeping what it made in
    /// `done`.
    async fn run(
        &self,
        actor: &User,
        plan: &CreatePlan,
        local: &LocalOutcome,
        step: &Step,
        done: &mut Done,
    ) -> Result<(), Error> {
        let ports = &self.ports;
        let settings = &self.settings;
        let project = &plan.project;
        match step {
            Step::CreateProject {
                project: new,
                template,
            } => {
                done.project = Some(
                    ProjectService::new(Arc::clone(ports), Arc::clone(settings))
                        .create_from_template(actor, new.clone(), template.clone())
                        .await?,
                );
            }
            Step::AddEnvironment { name, kind } => {
                done.environment = Some(
                    EnvironmentService::new(Arc::clone(ports), Arc::clone(settings))
                        .add(
                            actor,
                            project,
                            name.clone(),
                            local.default_branch.clone(),
                            env_kind(*kind),
                        )
                        .await?,
                );
            }
            Step::AddDomain { host, label } => {
                self.add_domain_step(actor, project, host, label.as_ref(), done)
                    .await?;
            }
            Step::Bind { env, request } => {
                done.binding = Some(
                    BindingService::new(Arc::clone(ports), Arc::clone(settings))
                        .bind(actor, project, env, request.clone())
                        .await?,
                );
            }
            Step::IssueToken => {
                done.token = Some(
                    DeployService::new(Arc::clone(ports), Arc::clone(settings))
                        .rotate_token(actor, project)
                        .await?,
                );
            }
            Step::StoreSecrets(places) => {
                let env = done
                    .environment
                    .as_ref()
                    .map(|environment| &environment.name);
                let tenant = done.project.as_ref().and_then(|stored| stored.tenant_id);
                done.kept = self
                    .store_secrets(actor, (project, tenant), env, places, local)
                    .await?;
            }
            Step::SyncCi => {
                done.sync = Some(
                    SecretSync::new(Arc::clone(ports))
                        .sync(actor, project)
                        .await?,
                );
            }
            Step::GenerateRepo { .. }
            | Step::Clone
            | Step::Substitute(_)
            | Step::WriteManifest { .. }
            | Step::AskSecrets(_)
            | Step::WriteCiSnippet(_)
            | Step::Commit { .. }
            | Step::Push
            | Step::WriteLiftoffFile => {}
        }
        Ok(())
    }

    /// The `AddDomain` step with what it makes kept in `done`: the domain
    /// and, for a function, its binding. A function has no `[service]
    /// expose` to bind: its domain is tied to it here.
    async fn add_domain_step(
        &self,
        actor: &User,
        project: &ProjectName,
        host: &Host,
        label: Option<&Label>,
        done: &mut Done,
    ) -> Result<(), Error> {
        let domain = self.add_domain(actor, project, host, label).await?;
        if let Some(function) = done
            .environment
            .as_ref()
            .filter(|environment| environment.kind == EnvKind::Function)
        {
            done.binding = Some(
                BindingService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
                    .bind_function(actor, project, &function.name, &domain.host)
                    .await?,
            );
        }
        done.domain = Some(domain);
        Ok(())
    }

    /// The `AddDomain` step: the managed domain `label` when the plan
    /// names one, the external `host` otherwise.
    async fn add_domain(
        &self,
        actor: &User,
        project: &ProjectName,
        host: &Host,
        label: Option<&Label>,
    ) -> Result<Domain, Error> {
        let domains = DomainService::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        match label {
            Some(label) => {
                domains
                    .add_managed(actor, project, label.clone(), false)
                    .await
            }
            None => domains.add_external(actor, project, host.clone()).await,
        }
    }

    /// The `StoreSecrets` step: the value `local` has for each of `places`,
    /// stored in its scope (`environment` is `env`, `prod` by default)
    /// with its `ci` flag.
    /// A tenant secret that already exists is shared with the tenant's
    /// other projects: it is kept as it is, and its key returned.
    async fn store_secrets(
        &self,
        actor: &User,
        (project, tenant): (&ProjectName, Option<TenantId>),
        env: Option<&EnvName>,
        places: &[SecretPlace],
        local: &LocalOutcome,
    ) -> Result<Vec<SecretKey>, Error> {
        let env = env.map_or_else(|| EnvName::new(PROD), |env| Ok(env.clone()))?;
        let secrets = SecretsService::new(Arc::clone(&self.ports));
        let store = self.ports.store();
        let mut kept = Vec::new();
        for SecretPlace { key, scope, ci } in places {
            if *scope == ScopeKind::Tenant
                && let Some(tenant) = tenant
                && store
                    .secret(&SecretScope::Tenant(tenant), key)
                    .await?
                    .is_some()
            {
                kept.push(key.clone());
                continue;
            }
            let value = local
                .secrets
                .get(key)
                .cloned()
                .ok_or_else(|| Error::invalid("secrets", format!("{key} has no value")))?;
            let scope = scope_of(project, &env, *scope);
            let _entry = secrets.set(actor, &scope, key.clone(), value, *ci).await?;
        }
        Ok(kept)
    }

    /// The domain of project `name`, `domain` or `<name>.<base>`, once
    /// [`DomainService`] would add it (CRE-07).
    async fn admit_domain(&self, name: &ProjectName, domain: Option<Host>) -> Result<Host, Error> {
        let base = &self.settings.base;
        let domains = DomainService::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        let label = match &domain {
            Some(domain) => domain.label_in(base),
            None => Some(Label::new(name.as_str())?),
        };
        let admitted = match (label, domain) {
            (Some(label), _) => domains.admit_managed(&label).await,
            (None, Some(domain)) => domains.admit_external(&domain).await.map(|()| domain),
            (None, None) => Err(Error::internal_msg("a project name that is no label")),
        };
        admitted.map_err(|error| match error {
            Error::Conflict(why) => Error::conflict(format!("{why}: choose another with --domain")),
            other => other,
        })
    }

    /// `steps` (what the plan does in the working tree before it commits)
    /// followed by the steps every plan shares: the secrets, the CI
    /// workflow, the commit and push, the server steps and `.liftoff`.
    ///
    /// A compose project on a forge whose CI needs a registry login
    /// ([`GitProvider::ci_needs_registry_login`](crate::domain::GitProvider::ci_needs_registry_login):
    /// Forgejo) also asks for `REGISTRY_USER` and `REGISTRY_TOKEN`, stored
    /// as `ci = true` project secrets that `SyncCi` sends: a credential of
    /// its own, never `login`'s forge connection, whose token administers
    /// every repository of the account and would be readable by every
    /// workflow of the repository.
    async fn finish_plan(
        &self,
        mut steps: Vec<Step>,
        manifest: &ProjectManifest,
        new: NewProject,
        (template, login): (Option<TemplateRef>, &Login),
        domain: Host,
    ) -> Result<CreatePlan, Error> {
        let provider = self.ports.git().provider_of(&new.git_host).await?;
        let asks = secret_asks(manifest, provider, &new.git_host, login)?;
        if !asks.is_empty() {
            steps.push(Step::AskSecrets(asks.clone()));
        }
        let target = SnippetTarget {
            provider,
            forge_host: new.git_host.clone(),
            api_host: self.settings.proxy.api_host.clone(),
        };
        steps.push(Step::WriteCiSnippet(CiSnippet::render(
            &target,
            &manifest.deploy,
        )));
        steps.push(Step::Commit {
            message: COMMIT_MESSAGE.to_owned(),
        });
        steps.push(Step::Push);
        let project = new.name.clone();
        let git_host = new.git_host.clone();
        let repo = new.repo.clone();
        steps.push(Step::CreateProject {
            project: new,
            template,
        });
        let env = EnvName::new(PROD)?;
        let (kind, expose) = match &manifest.deploy {
            Deployable::Service { expose, .. } => (DeployKind::Compose, expose.as_ref()),
            Deployable::Function { .. } => (DeployKind::Function, None),
        };
        steps.push(Step::AddEnvironment {
            name: env.clone(),
            kind,
        });
        steps.push(Step::AddDomain {
            host: domain.clone(),
            label: domain.label_in(&self.settings.base),
        });
        if let Some(expose) = expose {
            steps.push(Step::Bind {
                env,
                request: BindRequest {
                    service: expose.service.clone(),
                    port: expose.port,
                    host: domain.clone(),
                },
            });
        }
        steps.push(Step::IssueToken);
        if !asks.is_empty() {
            steps.push(Step::StoreSecrets(
                asks.into_iter()
                    .map(|ask| SecretPlace {
                        key: ask.key,
                        scope: ask.scope,
                        ci: ask.ci,
                    })
                    .collect(),
            ));
        }
        steps.push(Step::SyncCi);
        steps.push(Step::WriteLiftoffFile);
        Ok(CreatePlan {
            project,
            git_host,
            repo,
            domain,
            steps,
        })
    }

    /// The actor's connection the template is read through and the text
    /// of its `liftoff.toml`: on the template's host, or, for an entry
    /// without one, the first of the actor's connections that has it.
    async fn read_template(
        &self,
        actor: &User,
        template: &TemplateSource,
    ) -> Result<(GitAccess, String), Error> {
        let connections = self.ports.store().git_connections_of_user(actor.id).await?;
        let candidates: Vec<&GitConnection> = match &template.host {
            Some(host) => {
                let found: Vec<&GitConnection> = connections
                    .iter()
                    .filter(|connection| &connection.host == host)
                    .collect();
                if found.is_empty() {
                    return Err(no_connection(host));
                }
                found
            }
            None => connections.iter().collect(),
        };
        if candidates.is_empty() {
            return Err(Error::forbidden(
                "no forge connection: run `lof git connect` first",
            ));
        }
        for connection in candidates {
            let access = open_connection(self.ports.as_ref(), connection)?;
            let read = self
                .ports
                .git()
                .list_templates(&access, core::slice::from_ref(&template.repo))
                .await?;
            if let Some(text) = read.into_iter().find_map(|found| found.manifest) {
                return Ok((access, text));
            }
        }
        Err(Error::invalid(
            ProjectManifest::FIELD,
            format!("{} has no liftoff.toml", template.repo),
        ))
    }

    /// The login of the actor's connection on `host`; [`Error::Forbidden`]
    /// unless it administers `repo`: an existing repository is
    /// checked when the plan is built, before anything is committed.
    async fn ensure_repo_admin(
        &self,
        actor: &User,
        host: &Host,
        repo: &RepoRef,
    ) -> Result<Login, Error> {
        let connection = self
            .ports
            .store()
            .git_connection(actor.id, host)
            .await?
            .ok_or_else(|| no_connection(host))?;
        let access = open_connection(self.ports.as_ref(), &connection)?;
        if self.ports.git().repo_role(&access, repo).await? < RepoRole::Admin {
            return Err(Error::forbidden(format!(
                "{} is not an admin of {repo} on {host}",
                access.login
            )));
        }
        Ok(access.login)
    }
}

/// What the server steps of a plan made so far.
#[derive(Debug, Default)]
struct Done {
    project: Option<Project>,
    environment: Option<Environment>,
    domain: Option<Domain>,
    binding: Option<Binding>,
    token: Option<DeployToken>,
    sync: Option<SyncReport>,
    kept: Vec<SecretKey>,
}

/// The environment kind of `kind`.
const fn env_kind(kind: DeployKind) -> EnvKind {
    match kind {
        DeployKind::Compose => EnvKind::Compose,
        DeployKind::Function => EnvKind::Function,
    }
}

/// [`Error::Validation`] on `secrets` when `manifest` asks for a key
/// liftoff sets itself (the deploy token, the registry login, the system
/// variables): a template may not take them over.
fn refuse_reserved_secrets(manifest: &ProjectManifest) -> Result<(), Error> {
    let reserved = manifest
        .secrets
        .keys()
        .find(|key| CI_ONLY_KEYS.contains(&key.as_str()) || SYSTEM_VARS.contains(&key.as_str()));
    if let Some(key) = reserved {
        return Err(Error::invalid(
            "secrets",
            format!("{key} is set by liftoff: a template may not ask for it"),
        ));
    }
    Ok(())
}

/// The `AskSecrets` of `manifest` on a forge of `provider` at `host`: its
/// `[secrets]`, then, for a compose project whose CI needs one, the
/// registry login ([`registry_asks`]).
///
/// # Errors
///
/// [`Error::Validation`] on `secrets` when `[secrets]` asks for a key
/// liftoff sets itself.
fn secret_asks(
    manifest: &ProjectManifest,
    provider: GitProvider,
    host: &Host,
    login: &Login,
) -> Result<Vec<SecretAsk>, Error> {
    refuse_reserved_secrets(manifest)?;
    let mut asks: Vec<SecretAsk> = manifest
        .secrets
        .iter()
        .map(|(key, spec)| SecretAsk {
            key: key.clone(),
            hint: spec.hint.clone(),
            generate: spec.generate,
            scope: spec.scope.unwrap_or(ScopeKind::Project),
            ci: false,
        })
        .collect();
    if provider.ci_needs_registry_login() && matches!(manifest.deploy, Deployable::Service { .. }) {
        asks.extend(registry_asks(host, login)?);
    }
    Ok(asks)
}

/// What a plan asks for the registry login of a compose workflow on
/// `host` (CRE-08): the account the image is pushed as (`login`, the
/// actor's own account, for example) and a token of it limited to
/// packages. Both are `ci = true` project secrets, write-only to people
/// (`SecretsService::get`) and never part of an environment's env.
fn registry_asks(host: &Host, login: &Login) -> Result<[SecretAsk; 2], Error> {
    Ok([
        SecretAsk {
            key: registry_key(REGISTRY_USER_KEY)?,
            hint: Some(format!(
                "the {host} account the workflow pushes the image as, e.g. {login}"
            )),
            generate: None,
            scope: ScopeKind::Project,
            ci: true,
        },
        SecretAsk {
            key: registry_key(REGISTRY_TOKEN_KEY)?,
            hint: Some(
                "a token of that account with only the package scope (write:package), not the `lof git connect` token"
                    .to_owned(),
            ),
            generate: None,
            scope: ScopeKind::Project,
            ci: true,
        },
    ])
}

/// The key `name`, one of the registry login's constants.
fn registry_key(name: &str) -> Result<SecretKey, Error> {
    SecretKey::new(name).map_err(|error| Error::internal_msg(format!("{name}: {error}")))
}

/// The refusal for an actor without a connection on `host`.
fn no_connection(host: &Host) -> Error {
    Error::forbidden(format!(
        "no connection to {host}: run `lof git connect {host}` first"
    ))
}
