//! Use-case services (onwards).
//!
//! One file per service, each generic over [`Ports`](crate::ports::Ports)
//! and built from `Arc<P>` plus `Arc<Settings>`. Every method takes the
//! acting [`User`](crate::domain::User): in single-user mode the admin; the
//! `authorize` hook of the `tenants` module narrows it.
//!
//! Every change of domains, bindings or environment states ends with
//! [`Proxy::apply`](crate::ports::Proxy::apply) of the whole
//! [`ProxyConfig::from_store`](crate::proxy_config::ProxyConfig::from_store)
//! under the service's [`Settings`].
//! External steps follow "check → act outside → record" with compensation
//! when a later step fails.

pub mod artifacts;
pub mod auth;
pub mod authorizer;
pub mod binding;
pub mod create;
pub mod deploy;
pub mod domain;
pub mod environment;
pub mod gate_listener;
pub mod git;
pub mod kv;
pub mod links;
pub mod listener;
pub mod mcp_auth;
pub mod module;
pub mod module_route;
pub mod project;
pub mod reconcile;
pub mod request;
pub mod secret_sync;
pub mod secrets;
pub mod settings;
mod support;
pub mod templates;
pub mod tenancy;
pub mod tunnels;
pub mod visibility;
pub mod web_panel;

pub use artifacts::{
    ArtifactContent, ArtifactImport, ArtifactUpload, ArtifactView, ArtifactsService, ImportOutcome,
    ImportedArtifact, PublishedArtifact, ServedFile,
};
pub use auth::{
    AuthService, LoginGrant, Principal, ProviderLogin, SETUP_ADMIN_LOGIN, bootstrap_admin,
};
pub use authorizer::Authorizer;
pub use binding::{BindRequest, BindingService};
pub use create::{
    ADD_MANIFEST_MESSAGE, COMMIT_MESSAGE, CreateOptions, CreatePlan, CreateService, Created,
    DeployKind, ExistingManifest, ExistingRepo, GeneratedRepo, LocalOutcome, MissingSecrets,
    SecretAsk, SecretPlace, Side, Step, Substitution, answer_secrets, generate_value,
    local_existing_steps, plan_secret_scope,
};
pub use deploy::{
    CiDeployOutcome, CiDeployRequest, CiFunctionRequest, DeployRecord, DeployService, QueuedDeploy,
};
pub use domain::{DomainRecord, DomainService, WildcardDns, WildcardVerdict, taken_host};
pub use environment::{EnvironmentChange, EnvironmentService, LogLines, LogsRequest};
pub use gate_listener::{GateAnswer, GateListener};
pub use git::{DevicePolled, GitService};
pub use kv::{KvRefusal, KvWrite, MAX_KV_KEY_BYTES, check_kv_key, check_kv_write};
pub use links::{BrowserSession, IssuedLink, LINK_TTL, LinksService, OpenedLink, OpenedOwn};
pub use listener::{
    LIFTOFF_COOKIE_PREFIX, LIFTOFF_HEADER_PREFIX, ListenerRoute, forwarded_cookies,
    forwards_header, is_liftoff_authorization,
};
pub use mcp_auth::{MCP_MODULE, McpAuthService, McpGrant, ProtectedResource};
pub use module::{InstallRequest, ModuleChange, ModuleRecord, ModuleService};
pub use module_route::ModuleRouteService;
pub use project::ProjectService;
pub use reconcile::Reconciler;
pub use request::{
    HOST_SESSION_COOKIE, bearer_of, cookie_value, hidden_from_hooks, hook_request,
    host_session_cleared, host_session_cookie,
};
pub use secret_sync::{SecretSync, SyncFailure, SyncReport};
pub use secrets::{
    DEPLOY_TOKEN_KEY, SecretEntry, SecretRevision, SecretsService, check_ci_scope, check_readable,
    env_scopes, merge_layers,
};
pub use settings::{ArtifactSettings, ModuleSettings, ProxySettings, Settings, TlsMode};
pub use templates::{
    Catalog, CatalogEntry, CatalogGroup, CatalogWarning, MAX_REQUESTED_TEMPLATES,
    TEMPLATE_CACHE_TTL, TemplateCatalog,
};
pub use tenancy::{TenancyService, TenantOf};
pub use tunnels::{Exposure, OpenedTunnel, TunnelEnd, TunnelsService, is_exposures};
pub use visibility::{Access, AccessDecision, AccessRequest, VisibilityService};
pub use web_panel::{
    PanelOrigin, SameSite, WEB_MODULE, WebPanelService, browser_session_cookie, with_same_site,
};
