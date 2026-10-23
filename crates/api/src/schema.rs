//! The operations of the API: one row
//! per endpoint below `/api/v1`, from which both `/openapi.json`
//! (`liftoff-rest`) and the MCP tools (`liftoff-mcp`) are built.
//!
//! A row names its request and response types twice over: by the schema
//! name of their `utoipa::ToSchema` derive (the `$ref` of
//! `/openapi.json`) and by the JSON Schema of their `schemars::JsonSchema`
//! derive (the `inputSchema` of a tool), so the two stay one table.
//!
//! The tool of an operation is `liftoff_<name>`; its arguments are the
//! path parameters (`{p}`, `{e}`, ...), the query fields and the body
//! fields, side by side in one object.

use serde_json::Value;
use utoipa::openapi::path::{Parameter, ParameterIn};

/// The HTTP method of an [`Operation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
}

impl Method {
    /// The method as HTTP spells it (`GET`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// A type of the API as both documents name it.
#[derive(Debug, Clone, Copy)]
pub struct TypeRef {
    /// Its schema name in `/openapi.json` (`ProjectCreate`).
    pub name: &'static str,
    /// Its JSON Schema (the `schemars` derive).
    pub json_schema: fn() -> Value,
}

/// The query an [`Operation`] takes.
#[derive(Debug, Clone, Copy)]
pub struct Query {
    /// Its parameters in `/openapi.json` (the `utoipa::IntoParams`
    /// derive).
    pub params: fn() -> Vec<Parameter>,
    /// Its JSON Schema (the `schemars` derive): one property per field.
    pub json_schema: fn() -> Value,
}

/// What an [`Operation`] answers with.
#[derive(Debug, Clone, Copy)]
pub enum Response {
    /// No body worth describing.
    Empty,
    /// One value.
    One(TypeRef),
    /// A JSON array of values.
    Many(TypeRef),
    /// `text/plain`.
    Text,
    /// `text/event-stream`, one event per line.
    Events,
}

/// One endpoint of the API.
#[derive(Debug, Clone, Copy)]
pub struct Operation {
    /// `<resource>_<action>` (`projects_list`); the MCP tool is
    /// `liftoff_<name>`.
    pub name: &'static str,
    /// Method.
    pub method: Method,
    /// Path with the `/api/v1` prefix and `{name}` parameters.
    pub path: &'static str,
    /// What it does, for the document and the tool.
    pub summary: &'static str,
    /// The status of success.
    pub status: u16,
    /// The JSON body, if any.
    pub request: Option<TypeRef>,
    /// The query, if any.
    pub query: Option<Query>,
    /// The answer.
    pub response: Response,
}

impl Operation {
    /// The `{name}` parameters of [`Operation::path`], in order.
    pub fn path_parameters(&self) -> impl Iterator<Item = &'static str> {
        self.path
            .split('/')
            .filter_map(|segment| segment.strip_prefix('{')?.strip_suffix('}'))
    }
}

/// The query parameters of `T` (for [`Query::params`]).
#[must_use]
pub fn query_params<T: utoipa::IntoParams>() -> Vec<Parameter> {
    T::into_params(|| Some(ParameterIn::Query))
}

/// The JSON Schema of `T` (for [`TypeRef::json_schema`] and
/// [`Query::json_schema`]).
#[must_use]
pub fn json_schema<T: schemars::JsonSchema>() -> Value {
    schemars::schema_for!(T).to_value()
}

/// A [`TypeRef`] for an api type: its utoipa schema name is its Rust name.
macro_rules! ty {
    ($t:ident) => {
        TypeRef {
            name: stringify!($t),
            json_schema: json_schema::<crate::$t>,
        }
    };
}

/// A [`Query`] for an api query type.
macro_rules! query {
    ($t:ident) => {
        Query {
            params: query_params::<crate::$t>,
            json_schema: json_schema::<crate::$t>,
        }
    };
}

/// A row answering `200` without a query.
const fn op(
    name: &'static str,
    method: Method,
    path: &'static str,
    summary: &'static str,
    request: Option<TypeRef>,
    response: Response,
) -> Operation {
    Operation {
        name,
        method,
        path,
        summary,
        status: 200,
        request,
        query: None,
        response,
    }
}

/// `operation` answering `status` instead of `200`.
const fn answering(status: u16, operation: Operation) -> Operation {
    Operation {
        status,
        ..operation
    }
}

/// `operation` taking `query`.
const fn querying(query: Query, operation: Operation) -> Operation {
    Operation {
        query: Some(query),
        ..operation
    }
}

/// Every endpoint below `/api/v1`, in the documented order.
pub const OPERATIONS: &[Operation] = rows::OPERATIONS;

/// The rows, apart so their short names stay local.
mod rows {
    use super::Method::{Delete, Get, Patch, Post, Put};
    use super::Response::{Empty, Events, Many, One, Text};
    use super::{Operation, Query, TypeRef, answering, json_schema, op, query_params, querying};

    /// Every endpoint below `/api/v1`, in the documented order.
    pub(super) const OPERATIONS: &[Operation] = &[
        op(
            "health_check",
            Get,
            "/api/v1/healthz",
            "Liveness",
            None,
            Empty,
        ),
        op(
            "readiness_check",
            Get,
            "/api/v1/readyz",
            "Readiness: docker, SQLite and the Caddy admin API",
            None,
            Empty,
        ),
        op(
            "openapi_get",
            Get,
            "/api/v1/openapi.json",
            "This document",
            None,
            Empty,
        ),
        op(
            "auth_token_exchange",
            Post,
            "/api/v1/auth/token",
            "Trade a single-use token for a session",
            Some(ty!(TokenExchange)),
            One(ty!(SessionGranted)),
        ),
        answering(
            201,
            op(
                "auth_links_create",
                Post,
                "/api/v1/auth/links",
                "Issue a one-time link that opens a browser session for a target",
                Some(ty!(LinkCreate)),
                One(ty!(LinkIssued)),
            ),
        ),
        answering(
            204,
            op(
                "auth_session_delete",
                Delete,
                "/api/v1/auth/session",
                "Log out",
                None,
                Empty,
            ),
        ),
        op(
            "me_get",
            Get,
            "/api/v1/me",
            "The caller",
            None,
            One(ty!(UserView)),
        ),
        op(
            "server_get",
            Get,
            "/api/v1/server",
            "The base domain, the public IP and each dependency (docker, SQLite, Caddy admin, wildcard DNS)",
            None,
            One(ty!(ServerView)),
        ),
        op(
            "git_connections_list",
            Get,
            "/api/v1/git/connections",
            "The caller's forge connections",
            None,
            Many(ty!(ConnectionView)),
        ),
        answering(
            201,
            op(
                "git_connections_create",
                Post,
                "/api/v1/git/connections",
                "Connect a forge with a personal access token",
                Some(ty!(ConnectionCreate)),
                One(ty!(ConnectionView)),
            ),
        ),
        // The device flow of `lof git connect`.
        op(
            "git_connections_device_start",
            Post,
            "/api/v1/git/connections/{host}/device",
            "Start the forge's OAuth device flow (its git module needs an OAuth application)",
            Some(ty!(GitDeviceStart)),
            One(ty!(GitDeviceView)),
        ),
        answering(
            201,
            op(
                "git_connections_device_poll",
                Post,
                "/api/v1/git/connections/{host}/device/poll",
                "Poll the device flow: 202 until confirmed, then the stored connection",
                Some(ty!(GitDevicePoll)),
                One(ty!(ConnectionView)),
            ),
        ),
        op(
            "git_connections_delete",
            Delete,
            "/api/v1/git/connections/{host}",
            "Forget the connection on a host",
            None,
            One(ty!(Deleted)),
        ),
        querying(
            query!(TemplatesQuery),
            op(
                "git_templates_list",
                Get,
                "/api/v1/git/templates",
                "The template catalogue of `lof create`, by group (?group=&repos=)",
                None,
                One(ty!(TemplateCatalogView)),
            ),
        ),
        op(
            "git_create_plan",
            Post,
            "/api/v1/git/create-plan",
            "The plan of `lof create`, validated; nothing is created",
            Some(ty!(CreatePlanRequest)),
            One(ty!(CreatePlanView)),
        ),
        answering(
            201,
            op(
                "git_generate",
                Post,
                "/api/v1/git/generate",
                "Generate the repository of a `lof create` plan from its template, through your forge connection",
                Some(ty!(CreatePlanRequest)),
                One(ty!(GeneratedRepoView)),
            ),
        ),
        op(
            "projects_list",
            Get,
            "/api/v1/projects",
            "Every project",
            None,
            Many(ty!(ProjectView)),
        ),
        answering(
            201,
            op(
                "projects_create",
                Post,
                "/api/v1/projects",
                "Create a project for a repository",
                Some(ty!(ProjectCreate)),
                One(ty!(ProjectView)),
            ),
        ),
        op(
            "projects_get",
            Get,
            "/api/v1/projects/{p}",
            "A project",
            None,
            One(ty!(ProjectView)),
        ),
        op(
            "projects_update",
            Patch,
            "/api/v1/projects/{p}",
            "Replace the stored liftoff.toml",
            Some(ty!(ProjectPatch)),
            One(ty!(ProjectView)),
        ),
        op(
            "projects_delete",
            Delete,
            "/api/v1/projects/{p}",
            "Delete a project without environments or domains",
            None,
            One(ty!(Deleted)),
        ),
        op(
            "projects_resolve",
            Post,
            "/api/v1/projects/{p}/resolve",
            "The project of a git remote ({p} is a placeholder, e.g. -)",
            Some(ty!(ProjectResolve)),
            One(ty!(ProjectView)),
        ),
        op(
            "environments_list",
            Get,
            "/api/v1/projects/{p}/environments",
            "The environments of a project",
            None,
            Many(ty!(EnvironmentView)),
        ),
        answering(
            201,
            op(
                "environments_create",
                Post,
                "/api/v1/projects/{p}/environments",
                "Add an environment pinned to a branch",
                Some(ty!(EnvironmentCreate)),
                One(ty!(EnvironmentView)),
            ),
        ),
        op(
            "environments_get",
            Get,
            "/api/v1/projects/{p}/environments/{e}",
            "An environment",
            None,
            One(ty!(EnvironmentView)),
        ),
        op(
            "environments_update",
            Patch,
            "/api/v1/projects/{p}/environments/{e}",
            "Repin an environment or change its limits",
            Some(ty!(EnvironmentPatch)),
            One(ty!(EnvironmentView)),
        ),
        querying(
            query!(EnvironmentDelete),
            op(
                "environments_delete",
                Delete,
                "/api/v1/projects/{p}/environments/{e}",
                "Remove an environment with its volumes (?confirm=true)",
                None,
                One(ty!(Deleted)),
            ),
        ),
        op(
            "environments_start",
            Post,
            "/api/v1/projects/{p}/environments/{e}/start",
            "Start a stopped environment",
            None,
            One(ty!(EnvironmentView)),
        ),
        op(
            "environments_stop",
            Post,
            "/api/v1/projects/{p}/environments/{e}/stop",
            "Stop a running environment, keeping its volumes",
            None,
            One(ty!(EnvironmentView)),
        ),
        op(
            "environments_share",
            Post,
            "/api/v1/projects/{p}/environments/{e}/share",
            "Open a private environment to anybody",
            None,
            One(ty!(EnvironmentView)),
        ),
        op(
            "environments_unshare",
            Post,
            "/api/v1/projects/{p}/environments/{e}/unshare",
            "Make an environment private again: only its owner and the admin",
            None,
            One(ty!(EnvironmentView)),
        ),
        answering(
            202,
            op(
                "environments_redeploy",
                Post,
                "/api/v1/projects/{p}/environments/{e}/redeploy",
                "Queue a copy of a succeeded deploy",
                Some(ty!(RedeployBody)),
                One(ty!(DeploysQueued)),
            ),
        ),
        op(
            "deploys_list",
            Get,
            "/api/v1/projects/{p}/environments/{e}/deploys",
            "The deploys of an environment, newest first",
            None,
            Many(ty!(DeployView)),
        ),
        querying(
            query!(LogsQuery),
            op(
                "environments_logs",
                Get,
                "/api/v1/projects/{p}/environments/{e}/logs",
                "Container logs (?service=&tail=&follow=)",
                None,
                Events,
            ),
        ),
        answering(
            201,
            op(
                "bindings_create",
                Post,
                "/api/v1/projects/{p}/environments/{e}/bindings",
                "Route a domain to service:port of the environment, or to its function",
                Some(ty!(BindingCreate)),
                One(ty!(BindingView)),
            ),
        ),
        op(
            "bindings_delete",
            Delete,
            "/api/v1/bindings/{host}",
            "Unbind a domain",
            None,
            One(ty!(Deleted)),
        ),
        op(
            "domains_list",
            Get,
            "/api/v1/domains",
            "Every domain",
            None,
            Many(ty!(DomainView)),
        ),
        answering(
            201,
            op(
                "domains_create",
                Post,
                "/api/v1/domains",
                "Add a managed or an external domain",
                Some(ty!(DomainCreate)),
                One(ty!(DomainView)),
            ),
        ),
        op(
            "domains_update",
            Patch,
            "/api/v1/domains/{host}",
            "Set proxied (needs a DNS module)",
            Some(ty!(DomainPatch)),
            One(ty!(DomainView)),
        ),
        op(
            "domains_delete",
            Delete,
            "/api/v1/domains/{host}",
            "Remove an unbound domain",
            None,
            One(ty!(Deleted)),
        ),
        answering(
            202,
            op(
                "deploys_create",
                Post,
                "/api/v1/deploys",
                "Deploy: CI with its deploy token, or a session (JSON or multipart)",
                Some(ty!(DeployBody)),
                One(ty!(DeploysQueued)),
            ),
        ),
        op(
            "deploys_get",
            Get,
            "/api/v1/deploys/{id}",
            "A deploy",
            None,
            One(ty!(DeployView)),
        ),
        op(
            "deploys_log",
            Get,
            "/api/v1/deploys/{id}/log",
            "The log of a deploy",
            None,
            Text,
        ),
        op(
            "tokens_rotate",
            Post,
            "/api/v1/projects/{p}/token/rotate",
            "Rotate the deploy token (the value goes to LIFTOFF_TOKEN and CI)",
            None,
            One(ty!(TokenView)),
        ),
        op(
            "tokens_snippet",
            Get,
            "/api/v1/projects/{p}/snippet",
            "The CI step that deploys",
            None,
            One(ty!(SnippetView)),
        ),
        querying(
            query!(ScopeQuery),
            op(
                "secrets_list",
                Get,
                "/api/v1/secrets",
                "The secrets of a scope without values (?scope=&id=)",
                None,
                Many(ty!(SecretView)),
            ),
        ),
        op(
            "secrets_set",
            Put,
            "/api/v1/secrets",
            "Write the next version of a secret",
            Some(ty!(SecretPut)),
            One(ty!(SecretView)),
        ),
        querying(
            query!(ScopeQuery),
            op(
                "secrets_get",
                Get,
                "/api/v1/secrets/{key}",
                "The current value, for the owner of the scope (?scope=&id=)",
                None,
                One(ty!(SecretValueView)),
            ),
        ),
        querying(
            query!(ScopeQuery),
            op(
                "secrets_delete",
                Delete,
                "/api/v1/secrets/{key}",
                "Delete a secret with its history (?scope=&id=)",
                None,
                One(ty!(Deleted)),
            ),
        ),
        querying(
            query!(ScopeQuery),
            op(
                "secrets_versions",
                Get,
                "/api/v1/secrets/{key}/versions",
                "The versions of a secret, newest first (?scope=&id=)",
                None,
                Many(ty!(SecretRevisionView)),
            ),
        ),
        querying(
            query!(ScopeQuery),
            op(
                "secrets_rollback",
                Post,
                "/api/v1/secrets/{key}/rollback",
                "Make an old value the next version (?scope=&id=)",
                Some(ty!(SecretRollback)),
                One(ty!(SecretView)),
            ),
        ),
        op(
            "secrets_sync",
            Post,
            "/api/v1/secrets/sync",
            "Send the ci secrets of a project to its repository",
            Some(ty!(SecretsSync)),
            One(ty!(SecretsSynced)),
        ),
        querying(
            query!(ResolveQuery),
            op(
                "secrets_resolve",
                Get,
                "/api/v1/secrets/resolve",
                "The env an environment runs with (?project=&env=)",
                None,
                One(ty!(ResolvedEnv)),
            ),
        ),
        // Artifacts. `POST` and
        // `PUT` also take `multipart/form-data` (`html`, a `files` field
        // per file named by its `filename`, `title`); the MCP tools send
        // the JSON form.
        querying(
            query!(ArtifactsQuery),
            op(
                "artifacts_list",
                Get,
                "/api/v1/artifacts",
                "Your artifacts, newest first (?all=true: every user's, admins only)",
                None,
                Many(ty!(ArtifactView)),
            ),
        ),
        answering(
            201,
            op(
                "artifacts_publish",
                Post,
                "/api/v1/artifacts",
                "Publish an HTML page with its files as version 1 of a private artifact; the answer has its URL and a one-time login link",
                Some(ty!(ArtifactBody)),
                One(ty!(ArtifactView)),
            ),
        ),
        op(
            "artifacts_get",
            Get,
            "/api/v1/artifacts/{id}",
            "An artifact",
            None,
            One(ty!(ArtifactView)),
        ),
        op(
            "artifacts_update",
            Put,
            "/api/v1/artifacts/{id}",
            "Publish a new version (HTML and files as a whole) as the current one",
            Some(ty!(ArtifactBody)),
            One(ty!(ArtifactView)),
        ),
        op(
            "artifacts_delete",
            Delete,
            "/api/v1/artifacts/{id}",
            "Delete an artifact with every version",
            None,
            One(ty!(Deleted)),
        ),
        op(
            "artifacts_versions",
            Get,
            "/api/v1/artifacts/{id}/versions",
            "The kept versions of an artifact, oldest first",
            None,
            Many(ty!(ArtifactVersionView)),
        ),
        op(
            "artifacts_rollback",
            Post,
            "/api/v1/artifacts/{id}/rollback",
            "Copy an older version as the new current one",
            Some(ty!(ArtifactRollback)),
            One(ty!(ArtifactView)),
        ),
        op(
            "artifacts_delete_version",
            Delete,
            "/api/v1/artifacts/{id}/versions/{n}",
            "Delete a version that is not the current one",
            None,
            One(ty!(Deleted)),
        ),
        op(
            "artifacts_share",
            Post,
            "/api/v1/artifacts/{id}/share",
            "Open an artifact to anybody, for good (no TTL)",
            None,
            One(ty!(ArtifactView)),
        ),
        op(
            "artifacts_unshare",
            Post,
            "/api/v1/artifacts/{id}/unshare",
            "Make an artifact private again, with a TTL from now",
            None,
            One(ty!(ArtifactView)),
        ),
        op(
            "modules_list",
            Get,
            "/api/v1/modules",
            "The installed modules with their commands, routes and tools",
            None,
            Many(ty!(ModuleView)),
        ),
        answering(
            201,
            op(
                "modules_install",
                Post,
                "/api/v1/modules",
                "Install a module from the registry without a restart",
                Some(ty!(ModuleInstall)),
                One(ty!(ModuleView)),
            ),
        ),
        op(
            "modules_available",
            Get,
            "/api/v1/modules/available",
            "The modules of the registry this server can run",
            None,
            Many(ty!(AvailableModuleView)),
        ),
        op(
            "modules_update",
            Patch,
            "/api/v1/modules/{name}",
            "Enable, disable or configure a module",
            Some(ty!(ModulePatch)),
            One(ty!(ModuleView)),
        ),
        querying(
            query!(ModuleDelete),
            op(
                "modules_remove",
                Delete,
                "/api/v1/modules/{name}",
                "Remove a module; its tables stay unless ?purge=true",
                None,
                One(ty!(Deleted)),
            ),
        ),
        op(
            "modules_tools_call",
            Post,
            "/api/v1/modules/{name}/tools/{tool}",
            "Call an MCP tool of an enabled module, as the caller",
            Some(ty!(ModuleToolCall)),
            One(ty!(ModuleToolAnswer)),
        ),
        op(
            "users_list",
            Get,
            "/api/v1/admin/users",
            "Every user",
            None,
            Many(ty!(UserView)),
        ),
        answering(
            201,
            op(
                "admin_token_create",
                Post,
                "/api/v1/admin/token",
                "Mint a single-use token for another machine",
                None,
                One(ty!(SetupTokenGranted)),
            ),
        ),
        querying(
            query!(SessionsRevoke),
            op(
                "sessions_revoke",
                Delete,
                "/api/v1/admin/sessions",
                "End every session of a user (?login=)",
                None,
                One(ty!(Deleted)),
            ),
        ),
        answering(
            204,
            op(
                "admin_reconcile",
                Post,
                "/api/v1/admin/reconcile",
                "Bring docker, slices and Caddy in line with the store",
                None,
                Empty,
            ),
        ),
    ];
}

/// The operation called `name`.
#[must_use]
pub fn operation(name: &str) -> Option<&'static Operation> {
    OPERATIONS.iter().find(|operation| operation.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod operations {
        use super::*;

        // TC: MCP-01 (the API names `liftoff_projects_list`,
        // `liftoff_deploys_create` and `liftoff_secrets_set`)
        #[test]
        fn named_after_resource_and_action_when_spec_names_them() {
            let found: Vec<Option<(Method, &str)>> =
                ["projects_list", "deploys_create", "secrets_set"]
                    .into_iter()
                    .map(|name| operation(name).map(|found| (found.method, found.path)))
                    .collect();
            assert_eq!(
                found,
                vec![
                    Some((Method::Get, "/api/v1/projects")),
                    Some((Method::Post, "/api/v1/deploys")),
                    Some((Method::Put, "/api/v1/secrets")),
                ]
            );
        }

        // TC: ART-15, ART-16, ART-17 (the routes of artifacts;
        // the MCP tools are `liftoff_<name>` of these
        // rows: `liftoff_artifacts_publish`, `..._update`, `..._share`,
        // `..._unshare`, `..._list`, `..._versions`, `..._rollback`,
        // `..._delete`, `..._get`, and `..._delete_version` for the one
        // more endpoint)
        #[test]
        fn artifact_rows_named_for_their_tools_when_listed() {
            let found: Vec<Option<(Method, &str, u16)>> = [
                "artifacts_list",
                "artifacts_publish",
                "artifacts_get",
                "artifacts_update",
                "artifacts_delete",
                "artifacts_versions",
                "artifacts_rollback",
                "artifacts_delete_version",
                "artifacts_share",
                "artifacts_unshare",
            ]
            .into_iter()
            .map(|name| operation(name).map(|found| (found.method, found.path, found.status)))
            .collect();
            assert_eq!(
                found,
                vec![
                    Some((Method::Get, "/api/v1/artifacts", 200)),
                    Some((Method::Post, "/api/v1/artifacts", 201)),
                    Some((Method::Get, "/api/v1/artifacts/{id}", 200)),
                    Some((Method::Put, "/api/v1/artifacts/{id}", 200)),
                    Some((Method::Delete, "/api/v1/artifacts/{id}", 200)),
                    Some((Method::Get, "/api/v1/artifacts/{id}/versions", 200)),
                    Some((Method::Post, "/api/v1/artifacts/{id}/rollback", 200)),
                    Some((Method::Delete, "/api/v1/artifacts/{id}/versions/{n}", 200)),
                    Some((Method::Post, "/api/v1/artifacts/{id}/share", 200)),
                    Some((Method::Post, "/api/v1/artifacts/{id}/unshare", 200)),
                ]
            );
        }

        // TC: ART-15 (the JSON form of publish and update is one body,
        // `{html, title?, files: {name: base64}}`, as the MCP tools send
        // it)
        #[test]
        fn publish_and_update_take_artifact_body_when_listed() {
            let bodies: Vec<Option<&str>> = ["artifacts_publish", "artifacts_update"]
                .into_iter()
                .map(|name| {
                    operation(name)
                        .and_then(|found| found.request)
                        .map(|request| request.name)
                })
                .collect();
            assert_eq!(bodies, vec![Some("ArtifactBody"), Some("ArtifactBody")]);
        }

        // TC: MCP-01 (one tool per operation: names never repeat)
        #[test]
        fn names_unique_when_listed() {
            let mut names: Vec<&str> = OPERATIONS.iter().map(|operation| operation.name).collect();
            let listed = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!((listed > 40, names.len()), (true, listed));
        }

        // TC: MCP-01 (`liftoff_<resource>_<action>` is a valid MCP tool
        // name: lowercase letters, digits and `_`, at most 64 characters)
        #[test]
        fn names_snake_case_when_listed() {
            let bad: Vec<&str> = OPERATIONS
                .iter()
                .map(|operation| operation.name)
                .filter(|name| {
                    !name.starts_with(|first: char| first.is_ascii_lowercase())
                        || !name.chars().all(|character| {
                            character.is_ascii_lowercase()
                                || character.is_ascii_digit()
                                || character == '_'
                        })
                        || name.len() + "liftoff_".len() > 64
                })
                .collect();
            assert_eq!((OPERATIONS.is_empty(), bad), (false, Vec::<&str>::new()));
        }

        // TC: MCP-01 (one row per method and path)
        #[test]
        fn routes_unique_when_listed() {
            let mut routes: Vec<(&str, &str)> = OPERATIONS
                .iter()
                .map(|operation| (operation.method.as_str(), operation.path))
                .collect();
            let listed = routes.len();
            routes.sort_unstable();
            routes.dedup();
            assert_eq!((listed > 40, routes.len()), (true, listed));
        }
    }
}
