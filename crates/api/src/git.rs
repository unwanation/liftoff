//! Forge connections (the core knows Forgejo by token only) and the
//! template catalogue.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::projects::ProjectCreate;

/// Body of `POST /git/connections`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectionCreate {
    /// `forgejo` (`github` and `gitlab` need their module: `424`).
    pub provider: String,
    /// The forge host, e.g. `git.example.com`.
    pub host: String,
    /// The user's own access token; checked with one call and stored
    /// sealed, never shown again.
    pub token: String,
}

/// Body of `POST /git/connections/{host}/device`: starts the forge's OAuth device flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitDeviceStart {
    /// `github` (or `gitlab`): the forge whose module has an OAuth
    /// application (`git_provider.oauth`); without one the route answers
    /// `400` and the CLI asks for a token instead.
    pub provider: String,
}

/// `200` body of `POST /git/connections/{host}/device`: a device flow
/// started at the forge (RFC 8628 3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct GitDeviceView {
    /// What the CLI polls with.
    pub device_code: String,
    /// The code the person enters at the forge.
    pub user_code: String,
    /// The page to open, e.g. `https://github.com/login/device`.
    pub verification_uri: String,
    /// Seconds to wait between two polls.
    pub interval: u64,
    /// Seconds the device code stays valid.
    pub expires_in: u64,
}

/// Body of `POST /git/connections/{host}/device/poll`.
///
/// `202` [`GitDevicePending`] until the person confirmed at the forge, then
/// `201` [`ConnectionView`] (kind `oauth`), stored as `POST
/// /git/connections` stores a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitDevicePoll {
    /// [`GitDeviceStart::provider`].
    pub provider: String,
    /// [`GitDeviceView::device_code`].
    pub device_code: String,
}

/// `202` body of `POST /git/connections/{host}/device/poll`: the person
/// has not confirmed yet.
///
/// `{"status":"pending"}`: poll again after the same interval.
/// `{"status":"slow_down","interval"}` (RFC 8628 3.5): the forge asks to
/// poll less often; wait `interval` seconds between polls from now on, or
/// 5 seconds more than so far when `interval` is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct GitDevicePending {
    /// `pending` or `slow_down`.
    pub status: String,
    /// With `slow_down`: the seconds the forge asks to wait between polls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<u64>,
}

/// A stored connection. Never carries the credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ConnectionView {
    /// `forgejo`, `github` or `gitlab`.
    pub provider: String,
    /// The forge host.
    pub host: String,
    /// The account the credential logs in as.
    pub account_login: String,
    /// `token` or `oauth`.
    pub kind: String,
    /// RFC 3339; `null` for a token without expiry.
    pub expires_at: Option<String>,
}

/// Query of `GET /git/templates`.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema, IntoParams, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TemplatesQuery {
    /// Only this group: `web`, `api`, `function` or `service`.
    #[serde(default)]
    pub group: Option<String>,
    /// More template repositories, comma-separated `[host/]owner/name`
    /// (the CLI's `[local] templates`), next to the server's.
    #[serde(default)]
    pub repos: Option<String>,
}

/// Answer of `GET /git/templates`: the catalogue of `lof create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct TemplateCatalogView {
    /// The groups that have a template, in the order `web`, `api`,
    /// `function`, `service`.
    pub groups: Vec<TemplateGroupView>,
    /// The repositories skipped and why.
    pub warnings: Vec<TemplateWarningView>,
}

/// One group of the catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct TemplateGroupView {
    /// `web`, `api`, `function` or `service`.
    pub group: String,
    /// Its templates, by name.
    pub templates: Vec<TemplateView>,
}

/// One template: its `[template]` and where it lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct TemplateView {
    /// The forge host.
    pub host: String,
    /// `owner/name` on that host.
    pub repo: String,
    /// `[template] name`.
    pub name: String,
    /// `[template] description`.
    pub description: String,
    /// `service` or `function`: what the manifest deploys.
    pub kind: String,
}

/// A repository the catalogue skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct TemplateWarningView {
    /// The repository as configured or requested.
    pub repo: String,
    /// Why it was skipped.
    pub reason: String,
}

/// Body of `POST /git/create-plan`:
/// what `lof create` was told. Exactly one of `template` and `existing`.
///
/// The server builds the plan with the same service the CLI runs in local
/// mode and validates it against its store (a taken name, repository or
/// domain is `409` before anything runs); nothing is created. The CLI then
/// runs the local steps itself and the server steps through their own
/// endpoints (`POST /projects`, `/projects/{p}/environments`, `/domains`,
/// `.../bindings`, `/projects/{p}/token/rotate`, `PUT /secrets`, `POST
/// /secrets/sync`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePlanRequest {
    /// The project name, `^[a-z0-9][a-z0-9-]{0,31}$`.
    pub name: String,
    /// The template repository, `[host/]owner/name` (`--template`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// The repository `lof create` runs in, instead of a template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing: Option<ExistingRepoBody>,
    /// `--domain`: the domain instead of `<name>.<base>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// `--public`; the generated repository is private by default.
    #[serde(default)]
    pub public: bool,
    /// `--owner`: the user or organisation the repository is generated
    /// under; the caller's account on the forge by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// `--tenant`: the slug of the tenant of the new project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

/// The repository `lof create` runs in: what the CLI read from `git
/// remote` and the working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExistingRepoBody {
    /// The forge host of `origin`.
    pub git_host: String,
    /// `owner/name` of `origin`.
    pub repo: String,
    /// The `liftoff.toml` text: the committed one, or what stack detection
    /// proposed and the person confirmed.
    pub manifest: String,
    /// Whether `manifest` was detected: the plan then writes and commits
    /// it.
    #[serde(default)]
    pub detected: bool,
}

/// `200` body of `POST /git/create-plan`: what `lof create` will do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct CreatePlanView {
    /// The project name.
    pub project: String,
    /// The forge host of its repository.
    pub git_host: String,
    /// `owner/name` of its repository.
    pub repo: String,
    /// Its domain.
    pub domain: String,
    /// The steps, in order.
    pub steps: Vec<CreateStepView>,
}

/// One step of a plan, by `step`; `side` says who runs it: `forge` (the
/// caller's connection), `local` (the CLI, in the working tree) or
/// `server` (the endpoint named in [`CreatePlanRequest`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct CreateStepView {
    /// `forge`, `local` or `server`.
    pub side: String,
    /// The step and its data.
    #[serde(flatten)]
    pub step: CreateStep,
}

/// The steps of a plan, in the documented order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum CreateStep {
    /// Generates `repo` from the template repository `template`.
    GenerateRepo {
        /// `owner/name` of the template.
        template: String,
        /// `owner/name` of the repository to create.
        repo: String,
        /// Whether it is private.
        private: bool,
    },
    /// Clones the repository into `./<project>`.
    Clone,
    /// Replaces `{{project}}` and `{{domain}}` in the listed files only.
    Substitute {
        /// `[substitute] files`.
        files: Vec<String>,
        /// What `{{project}}` becomes.
        project: String,
        /// What `{{domain}}` becomes.
        domain: String,
    },
    /// Writes `liftoff.toml` (a detected manifest).
    WriteManifest {
        /// The file's text.
        content: String,
    },
    /// Asks for (or generates) the values of `[secrets]`.
    AskSecrets {
        /// By key.
        secrets: Vec<SecretAskView>,
    },
    /// Writes the CI workflow, unless the repository has the file.
    WriteCiSnippet {
        /// Where, relative to the repository root.
        path: String,
        /// The workflow.
        content: String,
    },
    /// Commits what the steps before changed, when they changed anything.
    Commit {
        /// The commit message.
        message: String,
    },
    /// Pushes the default branch to `origin`.
    Push,
    /// `POST /projects` with `project`.
    CreateProject {
        /// The body.
        project: ProjectCreate,
    },
    /// `POST /projects/{p}/environments`, pinned to the default branch.
    AddEnvironment {
        /// `prod`.
        name: String,
        /// `compose` or `function`.
        kind: String,
    },
    /// `POST /domains`: managed (`name` = `label`) when the plan names a
    /// label under the base domain, external (`name` = `host`) otherwise.
    AddDomain {
        /// The host.
        host: String,
        /// Its label under the base domain; absent for an external domain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// `POST /projects/{p}/environments/{env}/bindings`.
    Bind {
        /// `prod`.
        env: String,
        /// The compose service.
        service: String,
        /// Its port inside the container.
        port: u16,
        /// The domain.
        host: String,
    },
    /// `POST /projects/{p}/token/rotate`: `LIFTOFF_TOKEN` goes to CI.
    IssueToken,
    /// `PUT /secrets` for each value `ask_secrets` collected.
    StoreSecrets {
        /// By key.
        secrets: Vec<SecretPlaceView>,
    },
    /// `POST /secrets/sync`.
    SyncCi,
    /// Writes `.liftoff` and adds it to `.gitignore`.
    WriteLiftoffFile,
}

/// One entry of `[secrets]` as `ask_secrets` asks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct SecretAskView {
    /// The key.
    pub key: String,
    /// Shown with the question.
    pub hint: Option<String>,
    /// `random:<n>`: the CLI generates the value (URL-safe base64 without
    /// padding of `n` random bytes) instead of asking.
    pub generate: Option<String>,
    /// Where `store_secrets` stores it: `tenant`, `project` or
    /// `environment` (`prod`).
    pub scope: String,
    /// Stored with `ci = true` and sent to the forge's CI: the registry
    /// login a Forgejo compose plan asks for.
    pub ci: bool,
}

/// A key and the scope `store_secrets` stores it in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct SecretPlaceView {
    /// The key.
    pub key: String,
    /// `tenant`, `project` or `environment` (`prod`).
    pub scope: String,
    /// `PUT /secrets` with `ci = true` (a project secret).
    pub ci: bool,
}

/// `201` body of `POST /git/generate`.
///
/// The repository the forge generated from the template of a
/// [`CreatePlanRequest`] through the caller's connection on the server.
/// The body of the request is the one of `POST /git/create-plan`; the
/// server plans again (a taken name is still `409`) and runs the plan's
/// `generate_repo` step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct GeneratedRepoView {
    /// The forge host.
    pub git_host: String,
    /// `owner/name` of the new repository.
    pub repo: String,
}

/// What `lof create --json` prints (CRE-13): the
/// plan as the server answered it (or local mode built it), then what
/// was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct CreateRunView {
    /// The plan.
    pub plan: CreatePlanView,
    /// What was made.
    pub result: CreateResultView,
}

/// The `result` of [`CreateRunView`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct CreateResultView {
    /// The project name.
    pub project: String,
    /// The forge host of its repository.
    pub git_host: String,
    /// `owner/name` of its repository.
    pub repo: String,
    /// `https://<domain>`, which answers after the first `git push`;
    /// absent in local mode (the plan stops before the domain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
