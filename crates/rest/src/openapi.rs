//! `GET /openapi.json`: the document a future frontend generates its client
//! from.
//!
//! The schemas come from the `ToSchema` derives of `liftoff-api` and the
//! query parameters from its `IntoParams` derives; the paths below
//! `/api/v1` come from [`liftoff_api::OPERATIONS`], the table the MCP
//! tools are built from too, and the few routes outside it from
//! [`ALIASES`] and [`TLS_ASK`]: one row per route of the router (a test keeps the two
//! in step), each `{name}` of a path a required string parameter.

use axum::Json;
use liftoff_api::{
    ArgView, AvailableModuleView, CommandView, ModuleInstall, ModulePatch, ModuleToolAnswer,
    ModuleToolCall, ModuleView, ReleaseView, RouteView, ToolView,
};
use liftoff_api::{ArtifactBody, ArtifactRollback, ArtifactVersionView, ArtifactView};
use liftoff_api::{
    BindingCreate, BindingView, ConnectionCreate, ConnectionView, CreatePlanRequest,
    CreatePlanView, CreateStep, CreateStepView, Deleted, DeployBody, DeployView, DeploysQueued,
    DomainCreate, DomainPatch, DomainView, EnvVar, EnvironmentCreate, EnvironmentPatch,
    EnvironmentView, ExistingRepoBody, GeneratedRepoView, GitDevicePoll, GitDeviceStart,
    GitDeviceView, GitProviderView, LinkCreate, LinkIssued, ProjectCreate, ProjectPatch,
    ProjectResolve, ProjectTemplate, ProjectView, QueuedView, RedeployBody, ResolvedEnv,
    SecretAskView, SecretPlaceView, SecretPut, SecretRevisionView, SecretRollback,
    SecretSyncFailure, SecretValueView, SecretView, SecretsSync, SecretsSynced, ServerCheckView,
    ServerView, SessionGranted, SetupTokenGranted, SnippetView, TemplateCatalogView,
    TemplateGroupView, TemplateView, TemplateWarningView, TlsAsk, TokenExchange, TokenView,
    UserView,
};
use liftoff_api::{Method, OPERATIONS, Operation, Query, Response, json_schema, query_params};
use utoipa::OpenApi;
use utoipa::openapi::path::{
    self, HttpMethod, OperationBuilder, Parameter, ParameterBuilder, ParameterIn,
};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Ref, Schema, Type};
use utoipa::openapi::{Content, RefOr, Required, ResponseBuilder};

/// The schemas of the API.
#[derive(OpenApi)]
#[openapi(components(schemas(
    ArgView,
    AvailableModuleView,
    CommandView,
    ModuleInstall,
    ModulePatch,
    ModuleToolAnswer,
    ModuleToolCall,
    ModuleView,
    ReleaseView,
    RouteView,
    ToolView,
    ArtifactBody,
    ArtifactRollback,
    ArtifactVersionView,
    ArtifactView,
    BindingCreate,
    BindingView,
    ConnectionCreate,
    ConnectionView,
    CreatePlanRequest,
    CreatePlanView,
    CreateStep,
    CreateStepView,
    ExistingRepoBody,
    GeneratedRepoView,
    GitDevicePoll,
    GitDeviceStart,
    GitDeviceView,
    GitProviderView,
    Deleted,
    DeployBody,
    DeployView,
    DeploysQueued,
    DomainCreate,
    DomainPatch,
    DomainView,
    EnvVar,
    EnvironmentCreate,
    EnvironmentPatch,
    EnvironmentView,
    LinkCreate,
    LinkIssued,
    ProjectCreate,
    ProjectPatch,
    ProjectResolve,
    ProjectTemplate,
    ProjectView,
    QueuedView,
    RedeployBody,
    ResolvedEnv,
    SecretPut,
    SecretRevisionView,
    SecretAskView,
    SecretPlaceView,
    SecretRollback,
    SecretSyncFailure,
    SecretValueView,
    SecretView,
    SecretsSync,
    SecretsSynced,
    ServerCheckView,
    ServerView,
    SessionGranted,
    SetupTokenGranted,
    SnippetView,
    TemplateCatalogView,
    TemplateGroupView,
    TemplateView,
    TemplateWarningView,
    TokenExchange,
    TokenView,
    UserView
)))]
struct Schemas;

/// The method as utoipa names it.
const fn http(method: Method) -> HttpMethod {
    match method {
        Method::Get => HttpMethod::Get,
        Method::Post => HttpMethod::Post,
        Method::Put => HttpMethod::Put,
        Method::Patch => HttpMethod::Patch,
        Method::Delete => HttpMethod::Delete,
    }
}

/// The root aliases of rows of [`OPERATIONS`]: `(alias, path in the
/// table)`. They are not tools.
const ALIASES: &[(&str, &str)] = &[
    ("/healthz", "/api/v1/healthz"),
    ("/readyz", "/api/v1/readyz"),
    ("/openapi.json", "/api/v1/openapi.json"),
];

/// Caddy's on-demand TLS check: the one route neither below `/api/v1`
/// nor an alias. It is not a tool.
const TLS_ASK: Operation = Operation {
    name: "internal_tls_ask",
    method: Method::Get,
    path: "/internal/tls-ask",
    summary: "Caddy's on-demand TLS permission (?domain=), loopback peers only",
    status: 200,
    request: None,
    query: Some(Query {
        params: query_params::<TlsAsk>,
        json_schema: json_schema::<TlsAsk>,
    }),
    response: Response::Empty,
};

/// Every route of the router: the table, its root aliases and
/// [`TLS_ASK`].
fn routes() -> impl Iterator<Item = Operation> {
    let aliases = ALIASES.iter().filter_map(|(alias, target)| {
        OPERATIONS
            .iter()
            .find(|operation| operation.path == *target)
            .map(|found| Operation {
                path: alias,
                ..*found
            })
    });
    OPERATIONS
        .iter()
        .copied()
        .chain(aliases)
        .chain(core::iter::once(TLS_ASK))
}

/// A required string path parameter for every `{name}` of `operation`'s
/// path.
fn path_parameters(operation: &Operation) -> impl Iterator<Item = Parameter> + '_ {
    operation.path_parameters().map(|name| {
        ParameterBuilder::new()
            .name(name)
            .parameter_in(ParameterIn::Path)
            .required(Required::True)
            .schema(Some(ObjectBuilder::new().schema_type(Type::String)))
            .build()
    })
}

fn schema_ref(name: &str) -> RefOr<Schema> {
    RefOr::Ref(Ref::from_schema_name(name))
}

fn json_content(schema: RefOr<Schema>) -> Content {
    Content::new(Some(schema))
}

fn path_item(route: &Operation) -> path::Operation {
    let mut builder = OperationBuilder::new().summary(Some(route.summary));
    let query = route
        .query
        .map(|query| (query.params)())
        .unwrap_or_default();
    for parameter in path_parameters(route).chain(query) {
        builder = builder.parameter(parameter);
    }
    if let Some(request) = route.request {
        builder = builder.request_body(Some(
            RequestBodyBuilder::new()
                .content("application/json", json_content(schema_ref(request.name)))
                .build(),
        ));
    }
    let ok = match route.response {
        Response::Empty => ResponseBuilder::new().description("Success").build(),
        Response::Text => ResponseBuilder::new()
            .description("Success")
            .content("text/plain", Content::new(None::<RefOr<Schema>>))
            .build(),
        Response::Events => ResponseBuilder::new()
            .description("Success")
            .content("text/event-stream", Content::new(None::<RefOr<Schema>>))
            .build(),
        Response::One(answer) => ResponseBuilder::new()
            .description("Success")
            .content("application/json", json_content(schema_ref(answer.name)))
            .build(),
        Response::Many(answer) => ResponseBuilder::new()
            .description("Success")
            .content(
                "application/json",
                json_content(RefOr::T(Schema::Array(
                    ArrayBuilder::new().items(schema_ref(answer.name)).build(),
                ))),
            )
            .build(),
    };
    builder.response(route.status.to_string(), ok).build()
}

/// The document.
pub(crate) fn document() -> utoipa::openapi::OpenApi {
    let mut doc = Schemas::openapi();
    "liftoff".clone_into(&mut doc.info.title);
    env!("CARGO_PKG_VERSION").clone_into(&mut doc.info.version);
    doc.info.description =
        Some("Deploy-as-a-service. Errors are `application/problem+json`.".to_owned());
    for route in routes() {
        doc.paths
            .add_path_operation(route.path, vec![http(route.method)], path_item(&route));
    }
    doc
}

pub(crate) async fn serve() -> Json<utoipa::openapi::OpenApi> {
    Json(document())
}
