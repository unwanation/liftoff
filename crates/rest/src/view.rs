//! Domain records to the wire types of `liftoff-api`.

use liftoff_api::{
    ArgView, ArtifactVersionView, ArtifactView, AvailableModuleView, BindingView, CommandView,
    ConnectionView, DeployView, DomainView, EnvironmentView, GitProviderView, ModuleView,
    ProjectView, QueuedView, ReleaseView, RouteView, SecretRevisionView, SecretView,
    TemplateCatalogView, TemplateGroupView, TemplateView, TemplateWarningView, TokenView, ToolView,
    UserView,
};
use liftoff_api::{
    CreatePlanView, CreateStep, CreateStepView, ProjectCreate, ProjectTemplate, SecretAskView,
    SecretPlaceView,
};
use liftoff_core::domain::{
    Artifact, ArtifactVersion, Binding, Cpus, DeployNumber, DeployToken, Domain, EnvName,
    Environment, GitConnection, Host, Project, ProjectName, Timestamp, Trigger, UpdatedBy, User,
    Visibility,
};
use liftoff_core::manifest::Deployable;
use liftoff_core::manifest::Generate;
use liftoff_core::module::AvailableModule;
use liftoff_core::service::{
    Catalog, CatalogEntry, DeployRecord, DomainRecord, ModuleRecord, QueuedDeploy, SecretEntry,
    SecretRevision,
};
use liftoff_core::service::{CreatePlan, DeployKind, SecretAsk, Side, Step};
use time::format_description::well_known::Rfc3339;

/// RFC 3339, the way every timestamp travels.
pub(crate) fn timestamp(at: Timestamp) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

fn optional(at: Option<Timestamp>) -> Option<String> {
    at.map(timestamp)
}

/// A CPU allowance as the number it is (`0.5`).
fn cpus(cpus: Cpus) -> f64 {
    f64::from(cpus.hundredths()) / 100.0
}

pub(crate) fn user(user: &User) -> UserView {
    UserView {
        id: user.id.to_string(),
        login: user.login.to_string(),
        email: user.email.clone(),
        role: user.role.to_string(),
        source: user.source.to_string(),
    }
}

pub(crate) fn connection(connection: &GitConnection) -> ConnectionView {
    ConnectionView {
        provider: connection.provider.to_string(),
        host: connection.host.to_string(),
        account_login: connection.account_login.to_string(),
        kind: connection.kind.to_string(),
        expires_at: optional(connection.expires_at),
    }
}

/// The template catalogue (`GET /git/templates`).
pub(crate) fn catalog(catalog: &Catalog) -> TemplateCatalogView {
    TemplateCatalogView {
        groups: catalog
            .groups
            .iter()
            .map(|group| TemplateGroupView {
                group: group.group.to_string(),
                templates: group.templates.iter().map(template).collect(),
            })
            .collect(),
        warnings: catalog
            .warnings
            .iter()
            .map(|warning| TemplateWarningView {
                repo: warning.source.to_string(),
                reason: warning.reason.clone(),
            })
            .collect(),
    }
}

fn template(entry: &CatalogEntry) -> TemplateView {
    let (name, description) = entry
        .manifest
        .template
        .as_ref()
        .map(|info| (info.name.clone(), info.description.clone()))
        .unwrap_or_default();
    TemplateView {
        host: entry.host.to_string(),
        repo: entry.repo.to_string(),
        name,
        description,
        kind: match entry.manifest.deploy {
            Deployable::Service { .. } => "service",
            Deployable::Function { .. } => "function",
        }
        .to_owned(),
    }
}

pub(crate) fn project(project: &Project) -> ProjectView {
    ProjectView {
        id: project.id.to_string(),
        name: project.name.to_string(),
        git_host: project.git_host.to_string(),
        repo: project.repo.to_string(),
        manifest: project.manifest.clone(),
        created_by: project.created_by.to_string(),
        created_at: timestamp(project.created_at),
    }
}

pub(crate) fn environment(project: &ProjectName, environment: &Environment) -> EnvironmentView {
    EnvironmentView {
        id: environment.id.to_string(),
        project: project.to_string(),
        name: environment.name.to_string(),
        kind: environment.kind.to_string(),
        branch: environment.pin.as_ref().map(|pin| pin.branch.to_string()),
        pinned_by: environment
            .pin
            .as_ref()
            .map(|pin| pin.pinned_by.to_string()),
        owner: environment.owner_user_id.map(|id| id.to_string()),
        state: environment.state.to_string(),
        cpus: cpus(environment.limits.cpus),
        memory_mb: environment.limits.memory_mb.get(),
        current_deploy_id: environment.current_deploy_id.map(|id| id.to_string()),
        visibility: environment.visibility.to_string(),
        created_at: timestamp(environment.created_at),
    }
}

pub(crate) fn binding(
    project: &ProjectName,
    environment: &EnvName,
    host: &Host,
    binding: &Binding,
) -> BindingView {
    BindingView {
        host: host.to_string(),
        project: project.to_string(),
        environment: environment.to_string(),
        service: binding.service.to_string(),
        container_port: binding.container_port.get(),
        host_port: binding.host_port.get(),
    }
}

pub(crate) fn domain(project: &ProjectName, domain: &Domain) -> DomainView {
    DomainView {
        host: domain.host.to_string(),
        kind: domain.kind.to_string(),
        project: project.to_string(),
        verified_at: optional(domain.verified_at),
        record_lost_at: optional(domain.record_lost_at),
        proxied: domain.proxied,
    }
}

pub(crate) fn domain_record(record: &DomainRecord) -> DomainView {
    domain(&record.project, &record.domain)
}

pub(crate) fn queued(queued: &QueuedDeploy) -> QueuedView {
    QueuedView {
        project: queued.project.to_string(),
        environment: queued.environment.to_string(),
        deploy_id: queued.deploy_id.to_string(),
        number: queued.number.get(),
    }
}

fn trigger(trigger: Trigger) -> String {
    match trigger {
        Trigger::User(id) => id.to_string(),
        Trigger::Token(id) => id.to_string(),
    }
}

pub(crate) fn deploy(record: &DeployRecord) -> DeployView {
    let deploy = &record.deploy;
    DeployView {
        id: deploy.id.to_string(),
        number: deploy.number.get(),
        project: record.project.to_string(),
        environment: record.environment.to_string(),
        source: deploy.source.to_string(),
        git_ref: deploy.git_ref.as_ref().map(ToString::to_string),
        tag: deploy.tag.to_string(),
        status: deploy.status.to_string(),
        error: deploy.error.clone(),
        rolled_back_to: deploy.rolled_back_to.map(DeployNumber::get),
        triggered_by: trigger(deploy.triggered_by),
        started_at: optional(deploy.started_at),
        finished_at: optional(deploy.finished_at),
    }
}

pub(crate) fn token(project: &ProjectName, token: &DeployToken) -> TokenView {
    TokenView {
        project: project.to_string(),
        id: token.id.to_string(),
        created_at: timestamp(token.created_at),
    }
}

fn updated_by(by: UpdatedBy) -> String {
    match by {
        UpdatedBy::User(id) => id.to_string(),
        UpdatedBy::Ci => "ci".to_owned(),
        UpdatedBy::Import => "import".to_owned(),
    }
}

pub(crate) fn secret(entry: &SecretEntry) -> SecretView {
    SecretView {
        key: entry.key.to_string(),
        scope: entry.scope.to_string(),
        version: entry.version.get(),
        ci: entry.ci,
        updated_by: updated_by(entry.updated_by),
        updated_at: timestamp(entry.updated_at),
    }
}

pub(crate) fn revision(revision: &SecretRevision) -> SecretRevisionView {
    SecretRevisionView {
        version: revision.version.get(),
        updated_by: updated_by(revision.updated_by),
        updated_at: timestamp(revision.updated_at),
    }
}

/// An installed module: what it declared, never its configuration. A module that did not load declares nothing.
pub(crate) fn module(record: &ModuleRecord) -> ModuleView {
    let installed = &record.installed;
    let manifest = record.manifest.as_ref();
    ModuleView {
        name: installed.name.to_string(),
        version: installed.version.to_string(),
        enabled: installed.enabled,
        error: installed.error.clone(),
        installed_at: timestamp(installed.installed_at),
        commands: manifest.map_or_else(Vec::new, |manifest| {
            manifest
                .commands
                .iter()
                .map(|command| CommandView {
                    name: command.name.clone(),
                    description: command.description.clone(),
                    args: command
                        .args
                        .iter()
                        .map(|arg| ArgView {
                            name: arg.name.clone(),
                            kind: arg.kind.as_str().to_owned(),
                            required: arg.required,
                            hint: arg.hint.clone(),
                        })
                        .collect(),
                })
                .collect()
        }),
        routes: manifest.map_or_else(Vec::new, |manifest| {
            manifest
                .routes
                .iter()
                .map(|route| RouteView {
                    method: route.method.clone(),
                    path: route.path.clone(),
                    public: route.public,
                })
                .collect()
        }),
        tools: manifest.map_or_else(Vec::new, |manifest| {
            manifest
                .tools
                .iter()
                .map(|tool| ToolView {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    schema: tool.schema.clone(),
                })
                .collect()
        }),
        config_schema: manifest.and_then(|manifest| manifest.config_schema.clone()),
        git_provider: manifest
            .and_then(|manifest| manifest.git_provider.as_ref())
            .map(|spec| GitProviderView {
                hosts: spec.hosts.clone(),
                oauth: spec.oauth,
            }),
    }
}

/// A module of the registry with the releases this daemon can run.
pub(crate) fn available_module(module: &AvailableModule) -> AvailableModuleView {
    AvailableModuleView {
        name: module.name.to_string(),
        releases: module
            .releases
            .iter()
            .map(|release| ReleaseView {
                version: release.version.to_string(),
                wit_version: release.wit_version.clone(),
                liftoffd_min: release.liftoffd_min.clone(),
                requires: release.requires.iter().map(ToString::to_string).collect(),
                config_schema: release.config_schema.clone(),
            })
            .collect(),
    }
}

/// A `lof create` plan (`POST /git/create-plan`).
pub(crate) fn create_plan(plan: &CreatePlan) -> CreatePlanView {
    CreatePlanView {
        project: plan.project.to_string(),
        git_host: plan.git_host.to_string(),
        repo: plan.repo.to_string(),
        domain: plan.domain.to_string(),
        steps: plan
            .steps
            .iter()
            .map(|step| CreateStepView {
                side: match step.side() {
                    Side::Forge => "forge",
                    Side::Local => "local",
                    Side::Server => "server",
                }
                .to_owned(),
                step: create_step(step),
            })
            .collect(),
    }
}

/// One step of a plan.
fn create_step(step: &Step) -> CreateStep {
    match step {
        Step::GenerateRepo { template, target } => CreateStep::GenerateRepo {
            template: template.to_string(),
            repo: target.repo.to_string(),
            private: target.private,
        },
        Step::Clone => CreateStep::Clone,
        Step::Substitute(substitution) => CreateStep::Substitute {
            files: substitution
                .files
                .iter()
                .map(|file| file.as_str().to_owned())
                .collect(),
            project: substitution.project.to_string(),
            domain: substitution.domain.to_string(),
        },
        Step::WriteManifest { content } => CreateStep::WriteManifest {
            content: content.clone(),
        },
        Step::AskSecrets(asks) => CreateStep::AskSecrets {
            secrets: asks.iter().map(secret_ask).collect(),
        },
        Step::WriteCiSnippet(snippet) => CreateStep::WriteCiSnippet {
            path: snippet.path.as_str().to_owned(),
            content: snippet.content.clone(),
        },
        Step::Commit { message } => CreateStep::Commit {
            message: message.clone(),
        },
        Step::Push => CreateStep::Push,
        Step::CreateProject { project, template } => CreateStep::CreateProject {
            project: ProjectCreate {
                name: project.name.to_string(),
                git_host: project.git_host.to_string(),
                repo: project.repo.to_string(),
                manifest: project.manifest.clone(),
                tenant: project.tenant.as_ref().map(ToString::to_string),
                template: template.as_ref().map(|template| ProjectTemplate {
                    group: template.group.clone(),
                    name: template.name.clone(),
                }),
            },
        },
        Step::AddEnvironment { name, kind } => CreateStep::AddEnvironment {
            name: name.to_string(),
            kind: match kind {
                DeployKind::Compose => "compose",
                DeployKind::Function => "function",
            }
            .to_owned(),
        },
        Step::AddDomain { host, label } => CreateStep::AddDomain {
            host: host.to_string(),
            label: label.as_ref().map(ToString::to_string),
        },
        Step::Bind { env, request } => CreateStep::Bind {
            env: env.to_string(),
            service: request.service.to_string(),
            port: request.port.get(),
            host: request.host.to_string(),
        },
        Step::IssueToken => CreateStep::IssueToken,
        Step::StoreSecrets(places) => CreateStep::StoreSecrets {
            secrets: places
                .iter()
                .map(|place| SecretPlaceView {
                    key: place.key.to_string(),
                    scope: place.scope.as_str().to_owned(),
                    ci: place.ci,
                })
                .collect(),
        },
        Step::SyncCi => CreateStep::SyncCi,
        Step::WriteLiftoffFile => CreateStep::WriteLiftoffFile,
    }
}

/// One entry of `[secrets]` as the plan asks it.
fn secret_ask(ask: &SecretAsk) -> SecretAskView {
    SecretAskView {
        key: ask.key.to_string(),
        hint: ask.hint.clone(),
        generate: ask
            .generate
            .map(|Generate::Random(length)| format!("random:{length}")),
        scope: ask.scope.as_str().to_owned(),
        ci: ask.ci,
    }
}

/// An artifact as the API answers it: `url` its
/// address, `login_url` the one-time link of a private publish.
pub(crate) fn artifact(
    artifact: &Artifact,
    url: String,
    login_url: Option<String>,
) -> ArtifactView {
    ArtifactView {
        id: artifact.id.to_string(),
        url,
        title: artifact.title.clone(),
        shared: artifact.visibility == Visibility::Shared,
        expires_at: optional(artifact.expires_at),
        version: artifact.current_version.get(),
        created_at: timestamp(artifact.created_at),
        updated_at: timestamp(artifact.updated_at),
        login_url,
    }
}

/// One version of an artifact (`GET /artifacts/{id}/versions`).
pub(crate) fn artifact_version(version: &ArtifactVersion) -> ArtifactVersionView {
    ArtifactVersionView {
        n: version.number.get(),
        title: version.title.clone(),
        size: version.size_bytes,
        created_at: timestamp(version.created_at),
    }
}
