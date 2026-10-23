use core::future::ready;

use crate::domain::{
    ArtifactId, Deploy, Domain, DomainId, Environment, EnvironmentId, Host, Login, ModuleName,
    Origin, ProjectId, SecretScope, TenantId, TenantQuotas, TenantSlug, Timestamp, User, UserId,
    UserRole, WebUrl,
};
use crate::error::Error;
use crate::module::RouteSpec;

/// What a hook decided. With several subscribers the first
/// [`HookResult::Deny`] wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookResult {
    /// Allowed; a later subscriber may still deny (the first `deny` wins).
    Allow,
    /// Refused, with a reason that is safe to show.
    Deny(String),
    /// No opinion: the core decides.
    Continue,
}

/// What an actor asks the core to do: one variant per operation of a core
/// service that acts for a user (the table "action → call site" is in `docs/ARCHITECTURE.md`).
///
/// Every such operation asks [`Hooks::authorize`] exactly once, through the
/// single [`Authorizer`](crate::service::Authorizer) (MOD-04). Operations
/// without a user actor (the CI deploy by token, the deploy queue, start-up
/// reconcile, certificate checks, login) are not actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    /// `ProjectService::create` (`project.create`).
    ProjectCreate,
    /// `ProjectService::list` (`project.list`).
    ProjectList,
    /// `ProjectService::get` (`project.get`).
    ProjectGet,
    /// `ProjectService::set_manifest` (`project.set_manifest`).
    ProjectSetManifest,
    /// `ProjectService::remove` (`project.remove`).
    ProjectRemove,
    /// `ProjectService::resolve` (`project.resolve`).
    ProjectResolve,
    /// `EnvironmentService::list` (`environment.list`).
    EnvironmentList,
    /// `EnvironmentService::get` (`environment.get`).
    EnvironmentGet,
    /// `EnvironmentService::update` (`environment.update`).
    EnvironmentUpdate,
    /// `EnvironmentService::deploys` (`environment.deploys`).
    EnvironmentDeploys,
    /// `EnvironmentService::logs` (`environment.logs`).
    EnvironmentLogs,
    /// `EnvironmentService::add` (`environment.add`).
    EnvironmentAdd,
    /// `EnvironmentService::stop` (`environment.stop`).
    EnvironmentStop,
    /// `EnvironmentService::start` (`environment.start`).
    EnvironmentStart,
    /// `EnvironmentService::remove` (`environment.remove`).
    EnvironmentRemove,
    /// `VisibilityService::share` (`environment.share`).
    EnvironmentShare,
    /// `VisibilityService::unshare` (`environment.unshare`).
    EnvironmentUnshare,
    /// `DomainService::list` (`domain.list`).
    DomainList,
    /// `DomainService::add_managed` (`domain.add_managed`).
    DomainAddManaged,
    /// `DomainService::add_external` (`domain.add_external`).
    DomainAddExternal,
    /// `DomainService::remove` (`domain.remove`).
    DomainRemove,
    /// `DomainService::set_proxied` (`domain.set_proxied`).
    DomainSetProxied,
    /// `BindingService::bind` (`binding.bind`).
    BindingBind,
    /// `BindingService::unbind` (`binding.unbind`).
    BindingUnbind,
    /// `DeployService::redeploy` (`deploy.redeploy`).
    DeployRedeploy,
    /// `DeployService::request_cli` (`deploy.request`).
    DeployRequest,
    /// `DeployService::get` (`deploy.get`).
    DeployGet,
    /// `DeployService::rotate_token` (`deploy.rotate_token`).
    DeployRotateToken,
    /// `DeployService::snippet` (`deploy.snippet`).
    DeploySnippet,
    /// `SecretsService::set` (`secret.set`).
    SecretSet,
    /// `SecretsService::get` and `get_versioned` (`secret.get`).
    SecretGet,
    /// `SecretsService::list` (`secret.list`).
    SecretList,
    /// `SecretsService::remove` (`secret.remove`).
    SecretRemove,
    /// `SecretsService::history` (`secret.history`).
    SecretHistory,
    /// `SecretsService::rollback` (`secret.rollback`).
    SecretRollback,
    /// `SecretsService::sync` (`secret.sync`).
    SecretSync,
    /// `SecretsService::resolve` (`secret.resolve`).
    SecretResolve,
    /// `GitService::list` (`git.list`).
    GitList,
    /// `GitService::connect` (`git.connect`).
    GitConnect,
    /// `GitService::disconnect` (`git.disconnect`).
    GitDisconnect,
    /// `TemplateCatalog::list` (`git.templates`).
    GitTemplates,
    /// `AuthService::create_admin_token` (`admin.token`).
    AdminToken,
    /// `AuthService::users` (`admin.users`).
    AdminUsers,
    /// `AuthService::revoke_sessions` (`admin.revoke_sessions`).
    AdminRevokeSessions,
    /// `Reconciler::reconcile` (`admin.reconcile`).
    AdminReconcile,
    /// `ModuleService::list` (`module.list`).
    ModuleList,
    /// `ModuleService::available` (`module.available`).
    ModuleAvailable,
    /// `ModuleService::install` (`module.install`).
    ModuleInstall,
    /// `ModuleService::update` (`module.update`).
    ModuleUpdate,
    /// `ModuleService::remove` (`module.remove`).
    ModuleRemove,
    /// `LinksService::issue` (`link.issue`).
    LinkIssue,
    /// `TunnelsService::open` (`tunnel.open`).
    TunnelOpen,
    /// `ArtifactsService::publish` (`artifact.publish`).
    ArtifactPublish,
    /// `ArtifactsService::update` (`artifact.update`).
    ArtifactUpdate,
    /// `ArtifactsService::share` (`artifact.share`).
    ArtifactShare,
    /// `ArtifactsService::unshare` (`artifact.unshare`).
    ArtifactUnshare,
    /// `ArtifactsService::rollback` (`artifact.rollback`).
    ArtifactRollback,
    /// `ArtifactsService::delete` (`artifact.remove`).
    ArtifactRemove,
    /// `ArtifactsService::delete_version` (`artifact.remove_version`).
    ArtifactRemoveVersion,
    /// `ArtifactsService::list` (`artifact.list`).
    ArtifactList,
    /// `ArtifactsService::versions` (`artifact.versions`).
    ArtifactVersions,
    /// `ArtifactsService::get` (`artifact.get`).
    ArtifactGet,
}

impl Action {
    /// Every action, in declaration order.
    pub const ALL: [Self; 62] = [
        Self::ProjectCreate,
        Self::ProjectList,
        Self::ProjectGet,
        Self::ProjectSetManifest,
        Self::ProjectRemove,
        Self::ProjectResolve,
        Self::EnvironmentList,
        Self::EnvironmentGet,
        Self::EnvironmentUpdate,
        Self::EnvironmentDeploys,
        Self::EnvironmentLogs,
        Self::EnvironmentAdd,
        Self::EnvironmentStop,
        Self::EnvironmentStart,
        Self::EnvironmentRemove,
        Self::EnvironmentShare,
        Self::EnvironmentUnshare,
        Self::DomainList,
        Self::DomainAddManaged,
        Self::DomainAddExternal,
        Self::DomainRemove,
        Self::DomainSetProxied,
        Self::BindingBind,
        Self::BindingUnbind,
        Self::DeployRedeploy,
        Self::DeployRequest,
        Self::DeployGet,
        Self::DeployRotateToken,
        Self::DeploySnippet,
        Self::SecretSet,
        Self::SecretGet,
        Self::SecretList,
        Self::SecretRemove,
        Self::SecretHistory,
        Self::SecretRollback,
        Self::SecretSync,
        Self::SecretResolve,
        Self::GitList,
        Self::GitConnect,
        Self::GitDisconnect,
        Self::GitTemplates,
        Self::AdminToken,
        Self::AdminUsers,
        Self::AdminRevokeSessions,
        Self::AdminReconcile,
        Self::ModuleList,
        Self::ModuleAvailable,
        Self::ModuleInstall,
        Self::ModuleUpdate,
        Self::ModuleRemove,
        Self::LinkIssue,
        Self::TunnelOpen,
        Self::ArtifactPublish,
        Self::ArtifactUpdate,
        Self::ArtifactShare,
        Self::ArtifactUnshare,
        Self::ArtifactRollback,
        Self::ArtifactRemove,
        Self::ArtifactRemoveVersion,
        Self::ArtifactList,
        Self::ArtifactVersions,
        Self::ArtifactGet,
    ];

    /// Whether only the admin may do it, whatever a module answers: the
    /// installation-wide `admin.*` operations and installing, changing and
    /// removing modules (`module.install`, `module.update`,
    /// `module.remove`). A module's `allow` does not
    /// open them to anyone else; its `deny` still refuses them.
    #[must_use]
    pub const fn is_admin_only(self) -> bool {
        matches!(
            self,
            Self::AdminToken
                | Self::AdminUsers
                | Self::AdminRevokeSessions
                | Self::AdminReconcile
                | Self::ModuleInstall
                | Self::ModuleUpdate
                | Self::ModuleRemove
        )
    }

    /// The stable name modules and the audit log see (`project.remove`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProjectCreate => "project.create",
            Self::ProjectList => "project.list",
            Self::ProjectGet => "project.get",
            Self::ProjectSetManifest => "project.set_manifest",
            Self::ProjectRemove => "project.remove",
            Self::ProjectResolve => "project.resolve",
            Self::EnvironmentList => "environment.list",
            Self::EnvironmentGet => "environment.get",
            Self::EnvironmentUpdate => "environment.update",
            Self::EnvironmentDeploys => "environment.deploys",
            Self::EnvironmentLogs => "environment.logs",
            Self::EnvironmentAdd => "environment.add",
            Self::EnvironmentStop => "environment.stop",
            Self::EnvironmentStart => "environment.start",
            Self::EnvironmentRemove => "environment.remove",
            Self::EnvironmentShare => "environment.share",
            Self::EnvironmentUnshare => "environment.unshare",
            Self::DomainList => "domain.list",
            Self::DomainAddManaged => "domain.add_managed",
            Self::DomainAddExternal => "domain.add_external",
            Self::DomainRemove => "domain.remove",
            Self::DomainSetProxied => "domain.set_proxied",
            Self::BindingBind => "binding.bind",
            Self::BindingUnbind => "binding.unbind",
            Self::DeployRedeploy => "deploy.redeploy",
            Self::DeployRequest => "deploy.request",
            Self::DeployGet => "deploy.get",
            Self::DeployRotateToken => "deploy.rotate_token",
            Self::DeploySnippet => "deploy.snippet",
            Self::SecretSet => "secret.set",
            Self::SecretGet => "secret.get",
            Self::SecretList => "secret.list",
            Self::SecretRemove => "secret.remove",
            Self::SecretHistory => "secret.history",
            Self::SecretRollback => "secret.rollback",
            Self::SecretSync => "secret.sync",
            Self::SecretResolve => "secret.resolve",
            Self::GitList => "git.list",
            Self::GitConnect => "git.connect",
            Self::GitDisconnect => "git.disconnect",
            Self::GitTemplates => "git.templates",
            Self::AdminToken => "admin.token",
            Self::AdminUsers => "admin.users",
            Self::AdminRevokeSessions => "admin.revoke_sessions",
            Self::AdminReconcile => "admin.reconcile",
            Self::ModuleList => "module.list",
            Self::ModuleAvailable => "module.available",
            Self::ModuleInstall => "module.install",
            Self::ModuleUpdate => "module.update",
            Self::ModuleRemove => "module.remove",
            Self::LinkIssue => "link.issue",
            Self::TunnelOpen => "tunnel.open",
            Self::ArtifactPublish => "artifact.publish",
            Self::ArtifactUpdate => "artifact.update",
            Self::ArtifactShare => "artifact.share",
            Self::ArtifactUnshare => "artifact.unshare",
            Self::ArtifactRollback => "artifact.rollback",
            Self::ArtifactRemove => "artifact.remove",
            Self::ArtifactRemoveVersion => "artifact.remove_version",
            Self::ArtifactList => "artifact.list",
            Self::ArtifactVersions => "artifact.versions",
            Self::ArtifactGet => "artifact.get",
        }
    }
}

impl core::fmt::Display for Action {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an [`Action`] is about, as [`Hooks::authorize`] sees it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Resource {
    /// A project, and what lives at its level (its environments list,
    /// new environments and domains, its deploy token).
    Project(ProjectId),
    /// An environment, and what lives in it (its deploys and bindings).
    Environment(EnvironmentId),
    /// A domain.
    Domain(DomainId),
    /// The secrets of a scope.
    Secret(SecretScope),
    /// A tenant of the `tenants` module, and what lives at its level (a new project in it).
    Tenant(TenantId),
    /// An artifact.
    Artifact(ArtifactId),
    /// The installation as a whole: everything without a narrower
    /// resource (users, sessions, reconcile, the lists of every project
    /// or domain, a new project, the actor's own forge connections).
    Admin,
}

/// What the `authenticate` hook decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResult {
    /// The request is this user; the bearer is not checked (LOGN-05).
    Actor(UserId),
    /// Refused, with a reason that is safe to show.
    Deny(String),
    /// No opinion: the core checks the bearer session.
    Continue,
}

/// The parts of an HTTP request a hook sees.
///
/// Its [`Debug`] shows the values of [`HookRequest::CREDENTIAL_HEADERS`]
/// as `***`, so formatting a request never logs a credential.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct HookRequest {
    /// `GET`, `POST`, ...
    pub method: String,
    /// Path and query.
    pub path: String,
    /// Header names (lower case) and values.
    pub headers: Vec<(String, String)>,
}

impl HookRequest {
    /// Headers whose values are credentials: hidden by [`Debug`].
    pub const CREDENTIAL_HEADERS: [&'static str; 3] =
        ["authorization", "proxy-authorization", "cookie"];
}

impl core::fmt::Debug for HookRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(name, value)| {
                let shown = if Self::CREDENTIAL_HEADERS
                    .iter()
                    .any(|credential| name.eq_ignore_ascii_case(credential))
                {
                    "***"
                } else {
                    value.as_str()
                };
                (name.as_str(), shown)
            })
            .collect();
        f.debug_struct("HookRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("headers", &headers)
            .finish()
    }
}

/// A page the core renders and a module may replace (`login`, `403`, `503`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PageKind {
    /// The browser login page; `oidc` replaces it with a
    /// redirect to its provider.
    Login,
    /// A request the actor may not make (403).
    Forbidden,
    /// A domain with nothing running behind it (503).
    Unavailable,
    /// A tunnel without its CLI (503 "dev offline").
    Offline,
}

impl PageKind {
    /// The stable name modules see (`login`, `forbidden`, `unavailable`,
    /// `offline`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Forbidden => "forbidden",
            Self::Unavailable => "unavailable",
            Self::Offline => "offline",
        }
    }
}

/// What a module is asked to render in place of the core's page: the page and where the browser was going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    /// The page.
    pub kind: PageKind,
    /// The application host the page is shown for (the login's `host`,
    /// the listener's host), when there is one.
    pub host: Option<Host>,
    /// The address the browser asked for, `https://<host><path?query>`,
    /// when the page knows it: where a page the panel renders sends the
    /// person back to.
    pub return_to: Option<WebUrl>,
}

impl PageRequest {
    /// The page of `kind` without a host or an address.
    #[must_use]
    pub const fn of(kind: PageKind) -> Self {
        Self {
            kind,
            host: None,
            return_to: None,
        }
    }

    /// The page of `kind` for `host`, coming back to `path_and_query`
    /// (an absolute path) there; without a return when the two make no
    /// [`WebUrl`].
    #[must_use]
    pub fn at(kind: PageKind, host: &Host, path_and_query: &str) -> Self {
        let back = path_and_query
            .starts_with('/')
            .then(|| WebUrl::new(&format!("https://{host}{path_and_query}")).ok())
            .flatten();
        Self {
            kind,
            host: Some(host.clone()),
            return_to: back,
        }
    }
}

/// A module's page in place of the core's (`render_page`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModulePage {
    /// The HTML of the page (`oidc`'s login).
    Html(String),
    /// `302` to this URL (the panel `liftoff-web`'s page). [`ModuleHooks`](crate::module::ModuleHooks) passes it
    /// on only from the module `web` and only on the panel's origin.
    Redirect(WebUrl),
}

/// A request to a route of a module, below `/api/v1/m/<module>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModuleRequest {
    /// `GET`, `POST`, ...
    pub method: String,
    /// The path below the module's prefix (`/tenants`), with the query
    /// when the request has one (`/callback?code=…`).
    pub path: String,
    /// Header names (lower case) and values the module may see.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl ModuleRequest {
    /// The header in which a module's route sees the values of the
    /// browser's [`ModuleRequest::LOGIN_STATE_COOKIE_PREFIX`] cookies,
    /// joined by `, ` (v1 SSO-05). Modules never
    /// see a cookie, but a browser login must bind its OAuth
    /// `state` to the browser that started it (login CSRF): the login
    /// sets the cookie, the callback compares. The core drops a header
    /// of this name that the client sent, and shows it only to
    /// [`ModuleRequest::BROWSER_LOGIN_MODULE`].
    pub const LOGIN_STATE_HEADER: &'static str = "x-liftoff-login-state";
    /// The prefix of the cookies whose values reach
    /// [`ModuleRequest::LOGIN_STATE_HEADER`]: one cookie per started
    /// login, `__Host-liftoff_login_<a few characters of its state>`.
    pub const LOGIN_STATE_COOKIE_PREFIX: &'static str = "__Host-liftoff_login_";
    /// The module of the browser login (`GET /api/v1/m/oidc/login`, where
    /// the gate sends a browser without a session): the only one whose
    /// routes see [`ModuleRequest::LOGIN_STATE_HEADER`] and may set
    /// [`ModuleRequest::SESSION_COOKIE`] and the login-state cookies.
    pub const BROWSER_LOGIN_MODULE: &'static str = "oidc";
    /// The cookie of the browser session on the API host, which the
    /// browser login sets and `/gate/start` reads.
    pub const SESSION_COOKIE: &'static str = "__Host-liftoff_session";
}

/// What a module's route answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModuleResponse {
    /// The HTTP status.
    pub status: u16,
    /// Header names and values.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

/// The OAuth protected resource of `POST|GET|DELETE /mcp`, as the `mcp`
/// module keeps it (its configuration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResource {
    /// `resource`: the URL of the protected resource
    /// (`https://<api>/mcp`), which `/.well-known/oauth-protected-resource`
    /// names.
    pub resource: String,
    /// `allowed_audiences`: an access token of `/mcp` names one of them in
    /// its `aud`.
    pub audiences: Vec<String>,
}

/// The panel `liftoff-web` as the `web` module keeps it (its configuration; the `web` row).
///
/// While a module answers it, `/api/v1` answers CORS for `origin` alone,
/// with credentials, and the browser session's cookie is `SameSite=None`
/// (the panel's requests are cross-site); without an answer neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebPanel {
    /// `origin`: the panel's origin, `https://<host>[:<port>]` without a
    /// path, as browsers send it in `Origin`.
    pub origin: Origin,
}

/// The person an OAuth access token stands for, as the module that
/// verified it (`oidc`: issuer, audience, signature against the JWKS,
/// expiry) hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthIdentity {
    /// `iss`, as the token names it; the core accepts only the issuer the
    /// admin configured for `oidc`.
    pub issuer: String,
    /// `sub`.
    pub subject: String,
    /// The login (`preferred_username`).
    pub login: Login,
    /// The verified e-mail address, empty without one.
    pub email: String,
    /// The role the module read from the roles claim (as the device flow
    /// does).
    pub role: UserRole,
    /// `exp`: the `mcp` session ends with the token.
    pub expires_at: Timestamp,
}

/// Extension points of the core. Modules subscribe;
/// every default answers "no opinion", so [`NoHooks`] is the core without
/// modules.
pub trait Hooks: Send + Sync {
    /// Called first for every request; may name the actor.
    fn authenticate(&self, _request: &HookRequest) -> impl Future<Output = AuthResult> + Send {
        ready(AuthResult::Continue)
    }
    /// Whether `actor` may do `action` on `resource`.
    fn authorize(
        &self,
        _actor: &User,
        _action: Action,
        _resource: &Resource,
    ) -> impl Future<Output = HookResult> + Send {
        ready(HookResult::Continue)
    }
    /// Before docker is touched; [`HookResult::Deny`] fails the deploy (DEP-08).
    fn before_deploy(
        &self,
        _environment: &Environment,
        _deploy: &Deploy,
    ) -> impl Future<Output = HookResult> + Send {
        ready(HookResult::Continue)
    }
    /// After a deploy finished.
    fn after_deploy(
        &self,
        _environment: &Environment,
        _deploy: &Deploy,
    ) -> impl Future<Output = HookResult> + Send {
        ready(HookResult::Continue)
    }
    /// After a domain was stored.
    fn domain_created(&self, _domain: &Domain) -> impl Future<Output = HookResult> + Send {
        ready(HookResult::Continue)
    }
    /// After a domain was deleted.
    fn domain_removed(&self, _domain: &Domain) -> impl Future<Output = HookResult> + Send {
        ready(HookResult::Continue)
    }
    /// Whether `actor`, signed in, may pass the gate of the protected
    /// application `host` with `request` (`gate(host,
    /// request)`; the core resolved the session to `actor`,).
    fn gate(
        &self,
        _host: &Host,
        _actor: &User,
        _request: &HookRequest,
    ) -> impl Future<Output = HookResult> + Send {
        ready(HookResult::Continue)
    }
    /// The quotas of `tenant` (the `tenants` module keeps them): the core holds a new project to `max_projects` and gives a
    /// new environment the tenant's limits (TEN-04, TEN-05). `Ok(None)`
    /// is no opinion: no quota, the default limits.
    ///
    /// # Errors
    ///
    /// When a module that keeps quotas could not answer: the operation
    /// that needs them fails rather than run without them (fail closed).
    fn tenant_quotas(
        &self,
        _tenant: TenantId,
    ) -> impl Future<Output = Result<Option<TenantQuotas>, Error>> + Send {
        ready(Ok(None))
    }
    /// The tenant a new project of `actor` goes to (with the `tenants` module every project has one):
    /// the tenant of the slug `tenant` when the actor named one, else the
    /// module's choice (the actor's only tenant). `Ok(None)` is no
    /// opinion: the project is outside every tenant.
    ///
    /// # Errors
    ///
    /// What the module refuses with: an unknown slug ([`Error::NotFound`]),
    /// a choice it cannot make ([`Error::Validation`]); and a module that
    /// could not answer.
    fn project_tenant(
        &self,
        _actor: &User,
        _tenant: Option<&TenantSlug>,
    ) -> impl Future<Output = Result<Option<TenantId>, Error>> + Send {
        ready(Ok(None))
    }
    /// The protected resource of `/mcp`: the
    /// `mcp` module answers it from its configuration. `Ok(None)` is no
    /// opinion: no module serves MCP over HTTP.
    ///
    /// # Errors
    ///
    /// When a module that keeps it could not answer.
    fn mcp_resource(&self) -> impl Future<Output = Result<Option<McpResource>, Error>> + Send {
        ready(Ok(None))
    }
    /// Who the OAuth access token `token` stands for, checked against
    /// `audiences`: the `oidc` module verifies it
    /// against its provider's JWKS. `Ok(None)` is no opinion: no module
    /// verifies tokens. The token is a credential: never logged.
    ///
    /// # Errors
    ///
    /// [`Error::Unauthenticated`] for a token the module refuses (another
    /// issuer or audience, a bad signature, expired); when the module
    /// could not answer.
    fn verify_token(
        &self,
        _token: &str,
        _audiences: &[String],
    ) -> impl Future<Output = Result<Option<OAuthIdentity>, Error>> + Send {
        ready(Ok(None))
    }
    /// The panel `liftoff-web`: the `web` module
    /// answers it from its configuration. `Ok(None)` is no opinion: no
    /// panel, so no CORS and the browser cookie stays `SameSite=Lax`.
    ///
    /// # Errors
    ///
    /// When a module that keeps it could not answer.
    fn web_panel(&self) -> impl Future<Output = Result<Option<WebPanel>, Error>> + Send {
        ready(Ok(None))
    }
    /// The page replacing the core's page `page.kind` (its HTML or a
    /// redirect); `None` keeps the core's.
    fn render_page(&self, _page: &PageRequest) -> impl Future<Output = Option<ModulePage>> + Send {
        ready::<Option<ModulePage>>(None)
    }
    /// The route `method path` that `module` declares below
    /// `/api/v1/m/<module>` (its [`RouteSpec::public`] says whether it
    /// needs a session).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a module that is not installed or a route
    /// it does not declare (HOOK-06); [`Error::ModuleRequired`] when the
    /// module is disabled (HOOK-05). Without modules every module is
    /// unknown.
    fn module_route(
        &self,
        module: &ModuleName,
        _method: &str,
        _path: &str,
    ) -> impl Future<Output = Result<RouteSpec, Error>> + Send {
        ready(Err(Error::not_found(format!("module {module}"))))
    }
    /// Whether `module` is installed and enabled, so its hooks and routes
    /// take calls (`lof domain add --proxied` asks this of
    /// `dns.cloudflare` before it stores anything).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a module that is not installed (HOOK-06);
    /// [`Error::ModuleRequired`] when it is disabled (HOOK-05). Without
    /// modules every module is unknown.
    fn module_available(
        &self,
        module: &ModuleName,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(Err(Error::not_found(format!("module {module}"))))
    }
    /// Hands `request` to the route of `module` (`handle-route`) as
    /// `actor` (`None` on a public route); what it answered.
    ///
    /// # Errors
    ///
    /// As [`Hooks::module_route`], and whatever the module's call failed
    /// with.
    fn handle_route(
        &self,
        module: &ModuleName,
        _actor: Option<&User>,
        _request: ModuleRequest,
    ) -> impl Future<Output = Result<ModuleResponse, Error>> + Send {
        ready(Err(Error::not_found(format!("module {module}"))))
    }
    /// Calls the MCP tool `tool` of `module` (`handle-tool`) with the
    /// JSON `args` as `actor`; the module's JSON answer (the tools of modules are in the MCP server).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a module that is not installed or a tool
    /// it does not declare; [`Error::ModuleRequired`] when it is
    /// disabled; [`Error::Validation`] for `args` the tool's schema
    /// refuses; whatever the module's call failed with. Without modules
    /// every module is unknown.
    fn handle_tool(
        &self,
        module: &ModuleName,
        _actor: &User,
        _tool: &str,
        _args: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send {
        ready(Err(Error::not_found(format!("module {module}"))))
    }
}

/// The core without modules: every hook answers "no opinion".
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl Hooks for NoHooks {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::UserRole;
    use crate::fakes::World;

    mod hook_request {
        use super::*;

        // TC: LOGN-05 (secrets are never logged: a hook formatting the request)
        #[test]
        fn debug_hides_credential_headers_when_formatted() {
            let request = HookRequest {
                method: "GET".to_owned(),
                path: "/api/v1/me".to_owned(),
                headers: vec![
                    ("authorization".to_owned(), "Bearer oidc-secret".to_owned()),
                    (
                        "proxy-authorization".to_owned(),
                        "Basic proxy-secret".to_owned(),
                    ),
                    ("cookie".to_owned(), "lf_session=cookie-secret".to_owned()),
                    ("accept".to_owned(), "application/json".to_owned()),
                ],
            };
            let shown = format!("{request:?}");
            assert!(
                !shown.contains("oidc-secret")
                    && !shown.contains("proxy-secret")
                    && !shown.contains("cookie-secret")
                    && shown.contains("authorization")
                    && shown.contains("application/json")
                    && shown.contains("/api/v1/me"),
                "{shown}"
            );
        }
    }

    mod no_hooks {
        use super::*;

        fn request() -> HookRequest {
            HookRequest {
                method: "GET".to_owned(),
                path: "/".to_owned(),
                headers: Vec::new(),
            }
        }

        #[tokio::test]
        async fn continues_when_authenticate_called() {
            assert_eq!(NoHooks.authenticate(&request()).await, AuthResult::Continue);
        }

        // TC: MOD-05 (without modules `authorize` has no opinion on any action)
        #[tokio::test]
        async fn continues_when_authorize_called() {
            let world = World::new().await;
            let actor = world.records.user("carol", UserRole::User).unwrap();
            let mut answers = Vec::new();
            for action in Action::ALL {
                answers.push(
                    NoHooks
                        .authorize(&actor, action, &Resource::Environment(world.prod.id))
                        .await,
                );
            }
            assert!(
                answers.iter().all(|answer| *answer == HookResult::Continue),
                "{answers:?}"
            );
        }

        // TC: DEP-08 (without modules nothing denies a deploy)
        #[tokio::test]
        async fn continues_when_deploy_hooks_called() {
            let world = World::new().await;
            let deploy = world
                .records
                .deploy(
                    &world.prod,
                    1,
                    crate::domain::DeployStatus::Queued,
                    &world.owner,
                )
                .unwrap();
            assert_eq!(
                (
                    NoHooks.before_deploy(&world.prod, &deploy).await,
                    NoHooks.after_deploy(&world.prod, &deploy).await,
                ),
                (HookResult::Continue, HookResult::Continue)
            );
        }

        #[tokio::test]
        async fn continues_when_domain_hooks_called() {
            let world = World::new().await;
            assert_eq!(
                (
                    NoHooks.domain_created(&world.domain).await,
                    NoHooks.domain_removed(&world.domain).await,
                ),
                (HookResult::Continue, HookResult::Continue)
            );
        }

        #[tokio::test]
        async fn continues_when_gate_called() {
            let world = World::new().await;
            assert_eq!(
                NoHooks
                    .gate(&world.domain.host, &world.owner, &request())
                    .await,
                HookResult::Continue
            );
        }

        #[tokio::test]
        async fn keeps_core_page_when_render_page_called() {
            assert_eq!(
                (
                    NoHooks
                        .render_page(&PageRequest::of(PageKind::Unavailable))
                        .await,
                    NoHooks
                        .render_page(&PageRequest::of(PageKind::Offline))
                        .await,
                ),
                (None, None)
            );
        }
    }
}
