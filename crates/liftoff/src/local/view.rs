//! The answers of local mode in the wire types of `liftoff-api`, as the
//! daemon's REST adapter writes them: a command prints the same text and
//! the same `--json` with and without a server.

use liftoff_api::{
    ArgView, AvailableModuleView, CommandView, ConnectionView, CreatePlanView, CreateStep,
    CreateStepView, GitProviderView, ModuleView, ProjectCreate, ProjectTemplate, ReleaseView,
    RouteView, SecretAskView, SecretPlaceView, SecretRevisionView, SecretView, TemplateCatalogView,
    TemplateGroupView, TemplateView, TemplateWarningView, ToolView,
};
use liftoff_core::domain::{GitConnection, ScopeRef, Timestamp};
use liftoff_core::manifest::{Deployable, Generate};
use liftoff_core::module::AvailableModule;
use liftoff_core::service::{
    Catalog, CatalogEntry, CreatePlan, DeployKind, ModuleRecord, SecretAsk, Side, Step,
};
use time::format_description::well_known::Rfc3339;

use super::secrets::{LocalRevision, LocalSecret, local_user};

/// RFC 3339.
pub(crate) fn timestamp(at: Timestamp) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

/// A forge connection, without its credential.
pub(crate) fn connection(connection: &GitConnection) -> ConnectionView {
    ConnectionView {
        provider: connection.provider.to_string(),
        host: connection.host.to_string(),
        account_login: connection.account_login.to_string(),
        kind: connection.kind.to_string(),
        expires_at: connection.expires_at.map(timestamp),
    }
}

/// The kind of `scope`, as the API spells it.
pub(crate) const fn scope_kind(scope: &ScopeRef) -> &'static str {
    match scope {
        ScopeRef::Tenant(_) => "tenant",
        ScopeRef::Project(_) => "project",
        ScopeRef::Environment(..) => "environment",
        _ => "unknown",
    }
}

/// A local secret, without its value; written by the local user.
pub(crate) fn secret(secret: &LocalSecret) -> SecretView {
    SecretView {
        key: secret.key.to_string(),
        scope: scope_kind(&secret.scope).to_owned(),
        version: secret.version,
        ci: secret.ci,
        updated_by: local_user().id.to_string(),
        updated_at: timestamp(secret.updated_at),
    }
}

/// A kept version of a local secret, without its value; written by the
/// local user.
pub(crate) fn revision(revision: &LocalRevision) -> SecretRevisionView {
    SecretRevisionView {
        version: revision.version,
        updated_by: local_user().id.to_string(),
        updated_at: timestamp(revision.updated_at),
    }
}

/// An installed module and what it declared.
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

/// A module of the registry with the releases this CLI can run.
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

/// The template catalogue, as `GET /git/templates` answers it.
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

/// A `lof create` plan, as `POST /git/create-plan` answers it.
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
