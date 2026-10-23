//! Request and response types of the liftoff HTTP API (`/api/v1`).
//!
//! Shared by the REST adapter (`liftoff-rest`) and the CLI (`liftoff`), so
//! both sides agree on the wire format without the CLI depending on the
//! domain crate. Input types reject unknown fields; every type derives
//! [`utoipa::ToSchema`] for `/openapi.json` and [`schemars::JsonSchema`]
//! for the MCP tools; [`OPERATIONS`] lists the endpoints both are built
//! from.
//!
//! Identifiers, hosts and timestamps travel as plain strings (ULIDs, FQDNs,
//! RFC 3339); the server validates them into domain types at the boundary.
//!
//! The v2 API types for the entities:
//! forge connections, projects, environments, bindings, domains, deploys,
//! secrets and administration; artifacts.

mod admin;
mod artifacts;
mod auth;
mod deploys;
mod domains;
mod environments;
mod git;
mod modules;
mod projects;
mod schema;
mod secrets;
mod server;
mod users;

pub use admin::SessionsRevoke;
pub use artifacts::{
    ArtifactBody, ArtifactRollback, ArtifactVersionView, ArtifactView, ArtifactsQuery,
};
pub use auth::{
    DeviceLogin, DevicePoll, LinkCreate, LinkIssued, SessionGranted, SetupTokenGranted,
    TokenExchange,
};
pub use deploys::{
    DeployBody, DeployView, DeploysQueued, QueuedView, RedeployBody, SnippetView, TokenView,
};
pub use domains::{DomainCreate, DomainPatch, DomainView, TlsAsk};
pub use environments::{
    BindingCreate, BindingView, EnvironmentCreate, EnvironmentDelete, EnvironmentPatch,
    EnvironmentView, LogsQuery,
};
pub use git::{
    ConnectionCreate, ConnectionView, CreatePlanRequest, CreatePlanView, CreateResultView,
    CreateRunView, CreateStep, CreateStepView, ExistingRepoBody, GeneratedRepoView,
    GitDevicePending, GitDevicePoll, GitDeviceStart, GitDeviceView, SecretAskView, SecretPlaceView,
    TemplateCatalogView, TemplateGroupView, TemplateView, TemplateWarningView, TemplatesQuery,
};
pub use modules::{
    ArgView, AvailableModuleView, CommandView, GitProviderView, ModuleDelete, ModuleInstall,
    ModulePatch, ModuleToolAnswer, ModuleToolCall, ModuleView, ReleaseView, RouteView, ToolView,
};
pub use projects::{ProjectCreate, ProjectPatch, ProjectResolve, ProjectTemplate, ProjectView};
pub use schema::{
    Method, OPERATIONS, Operation, Query, Response, TypeRef, json_schema, operation, query_params,
};
pub use secrets::{
    EnvVar, ResolveQuery, ResolvedEnv, ScopeQuery, SecretPut, SecretRevisionView, SecretRollback,
    SecretSyncFailure, SecretValueView, SecretView, SecretsSync, SecretsSynced,
};
pub use server::{ServerCheckView, ServerView};
pub use users::{Deleted, UserView};

#[cfg(test)]
mod tests {
    use super::*;

    mod wire_names {
        use super::*;

        #[test]
        fn round_trips_when_user_view_serialised() {
            let view = UserView {
                id: "u1".to_owned(),
                login: "owner".to_owned(),
                email: "owner@example.com".to_owned(),
                role: "admin".to_owned(),
                source: "local".to_owned(),
            };
            let json = serde_json::to_value(&view).unwrap();
            assert_eq!(
                (
                    json["login"].as_str(),
                    serde_json::from_value::<UserView>(json.clone()).ok()
                ),
                (Some("owner"), Some(view))
            );
        }
    }
}
