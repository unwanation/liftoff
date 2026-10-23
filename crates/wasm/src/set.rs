//! [`ModuleSet`]: the loaded modules the core's hook dispatcher calls.

use core::fmt::Write as _;
use std::sync::{Arc, PoisonError, RwLock};

use liftoff_core::Error;
use liftoff_core::domain::{
    Cpus, Login, MemoryMb, ModuleName, Origin, TenantId, TenantQuotas, TenantSlug, Timestamp, User,
    UserId, UserRole, WebUrl,
};
use liftoff_core::module::{CoreCalls, HookEvent, ModuleCalls, NoCoreCalls};
use liftoff_core::ports::{
    AuthResult, HookRequest, HookResult, McpResource, ModulePage, ModuleRequest, ModuleResponse,
    ModuleStore, OAuthIdentity, PageRequest, Resource, WebPanel,
};
use serde_json::{Value, json};

use crate::log::ModuleLog;
use crate::module::{Module, RouteRequest, RouteResponse};

/// The loaded modules by name, as the core's
/// [`ModuleHooks`](liftoff_core::module::ModuleHooks) calls them
/// ([`ModuleCalls`]).
///
/// The host translates each call to the WIT world `liftoff:module`:
///
/// - a [`HookEvent`] is `on-hook(<hook>, <payload>)` with a JSON payload:
///   `authorize` is `{"actor":{"id","login","role"},"action":"project.get",
///   "resource":{"type":"project"|"environment"|"domain","id"} |
///   {"type":"secret","scope":{"type":"project"|"environment","id"}} |
///   {"type":"tenant","id"} | {"type":"admin"}}`, and the module runs as
///   the actor; `before_deploy` and `after_deploy` are
///   `{"environment":{"id","project","name","limits":{"cpus_hundredths",
///   "memory_mb"}},"deploy":{"id","number"}}`; `domain_created` and
///   `domain_removed` are `{"domain":{"id","host","kind","proxied"}}`
///   (`kind` is `managed` or `external`); `gate` is
///   `{"host","actor":{"id","login","role"},"request":{"method","path",
///   "headers"}}`, and the module runs as the actor;
/// - `authenticate(request)` and `render_page(kind)` answer data, which
///   `hook-result` cannot carry, so they are `handle-route`
///   calls on paths below `/.hook/` that no manifest route may declare,
///   without an actor: `POST /.hook/authenticate` with the request as
///   JSON `{"method","path","headers"}` in the body answers `200` with
///   the actor's user ID ([`AuthResult::Actor`]), `401` or `403` with the
///   reason ([`AuthResult::Deny`]), anything else
///   [`AuthResult::Continue`]; `GET /.hook/render_page/<kind>`
///   ([`PageKind::as_str`](liftoff_core::ports::PageKind::as_str)), with
///   `?host=<host>&return=<url>` (percent-encoded, each when the page
///   knows it), answers `200` with the HTML, `302`, `303` or `307` with
///   the absolute `https` `location` the browser goes to (a
///   [`WebUrl`](liftoff_core::domain::WebUrl);), anything else no page; `POST /.hook/tenant_quotas` with `{"tenant"}` answers `200`
///   with `{"max_projects","env_cpus_hundredths","env_memory_mb"}` (each
///   a number or `null`) for the tenant's quotas, `404` for none, and
///   anything else is the module's failure; `POST /.hook/project_tenant`
///   with `{"actor":{"id","login","role"},"tenant"}` (a slug or `null`)
///   answers `200` with `{"tenant"}` (an id or `null`), or a problem
///   document whose status (`400`, `403`, `404`, `409`) and `detail` are
///   the refusal; `POST /.hook/mcp_resource` answers `200` with
///   `{"resource","audiences"}`, `404` for none; `POST
///   /.hook/verify_token` with `{"token","audiences"}` answers `200` with
///   `{"issuer","subject","login","email","role","expires_at"}` (Unix
///   seconds) for the person of the access token, `401` with a problem
///   whose `detail` is the refusal, `404` for no opinion; `POST /.hook/web_panel` answers `200` with `{"origin"}`
///   (an [`Origin`](liftoff_core::domain::Origin), `https://<host>[:<port>]`),
///   `404` for none;
/// - a module route is `handle-route` with the request and the actor's
///   ID;
/// - a tool call is `handle-tool(name, args)`, as the actor when there
///   is one (the forge calls of
///   [`ModuleGit`](liftoff_core::module::ModuleGit) have none).
///
/// A module the set does not hold is [`Error::NotFound`]; a module error
/// maps as the host's [`ModuleError`](crate::ModuleError) does.
pub struct ModuleSet<S: 'static, L: 'static, C: 'static = NoCoreCalls> {
    modules: Arc<RwLock<Modules<S, L, C>>>,
}

/// The modules of a [`ModuleSet`], in insertion order.
type Modules<S, L, C> = Vec<Module<S, L, C>>;

impl<S: 'static, L: 'static, C> Clone for ModuleSet<S, L, C> {
    fn clone(&self) -> Self {
        Self {
            modules: Arc::clone(&self.modules),
        }
    }
}

impl<S: 'static, L: 'static, C> Default for ModuleSet<S, L, C> {
    fn default() -> Self {
        Self {
            modules: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl<S: 'static, L: 'static, C> core::fmt::Debug for ModuleSet<S, L, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ModuleSet").finish_non_exhaustive()
    }
}

impl<S: ModuleStore + 'static, L: ModuleLog, C: CoreCalls> ModuleSet<S, L, C> {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `module`, replacing a module of its name.
    pub fn insert(&self, module: Module<S, L, C>) {
        let mut modules = self.modules.write().unwrap_or_else(PoisonError::into_inner);
        modules.retain(|known| known.name() != module.name());
        modules.push(module);
    }

    /// Takes the module `name` out of the set; whether it held it. Its
    /// scheduler stops once the last clone of it (a call in progress)
    /// is dropped.
    pub fn remove(&self, name: &ModuleName) -> bool {
        let mut modules = self.modules.write().unwrap_or_else(PoisonError::into_inner);
        let before = modules.len();
        modules.retain(|known| known.name() != name);
        modules.len() != before
    }

    /// The module `name`, if the set holds it.
    #[must_use]
    pub fn get(&self, name: &ModuleName) -> Option<Module<S, L, C>> {
        self.modules
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|module| module.name() == name)
            .cloned()
    }
}

impl<S: ModuleStore + 'static, L: ModuleLog, C: CoreCalls> ModuleSet<S, L, C> {
    /// The module `name`, or [`Error::NotFound`].
    fn loaded(&self, name: &ModuleName) -> Result<Module<S, L, C>, Error> {
        self.get(name)
            .ok_or_else(|| Error::not_found(format!("module {name}")))
    }

    /// `handle-route` of `module` without an actor on a reserved
    /// `/.hook/` path.
    async fn hook_route(
        &self,
        module: &ModuleName,
        method: &str,
        path: String,
        body: Vec<u8>,
    ) -> Result<RouteResponse, Error> {
        let request = RouteRequest {
            method: method.to_owned(),
            path,
            headers: Vec::new(),
            body,
        };
        Ok(self.loaded(module)?.handle_route(None, request).await?)
    }
}

impl<S: ModuleStore + 'static, L: ModuleLog, C: CoreCalls> ModuleCalls for ModuleSet<S, L, C> {
    async fn on_hook(
        &self,
        module: &ModuleName,
        event: &HookEvent<'_>,
    ) -> Result<HookResult, Error> {
        let loaded = self.loaded(module)?;
        let actor = match event {
            HookEvent::Authorize { actor, .. } | HookEvent::Gate { actor, .. } => Some(*actor),
            _ => None,
        };
        let payload = payload(event).to_string();
        Ok(loaded.on_hook(actor, event.hook(), &payload).await?)
    }

    async fn authenticate(
        &self,
        module: &ModuleName,
        request: &HookRequest,
    ) -> Result<AuthResult, Error> {
        let body = json!({
            "method": request.method,
            "path": request.path,
            "headers": request.headers,
        })
        .to_string()
        .into_bytes();
        let response = self
            .hook_route(module, "POST", AUTHENTICATE.to_owned(), body)
            .await?;
        let text = String::from_utf8_lossy(&response.body);
        Ok(match response.status {
            200 => AuthResult::Actor(UserId::parse(text.trim()).map_err(|invalid| {
                tracing::error!(%module, %invalid, "a module's authenticate named no user ID");
                Error::internal_msg(format!(
                    "module {module} answered authenticate with no user ID"
                ))
            })?),
            401 | 403 => AuthResult::Deny(text.into_owned()),
            _ => AuthResult::Continue,
        })
    }

    async fn render_page(
        &self,
        module: &ModuleName,
        page: &PageRequest,
    ) -> Result<Option<ModulePage>, Error> {
        let response = self
            .hook_route(module, "GET", render_page_path(page), Vec::new())
            .await?;
        page_answer(response.status, &response.headers, &response.body).map_err(|why| {
            Error::internal_msg(format!("module {module} answered render_page: {why}"))
        })
    }

    async fn tenant_quotas(
        &self,
        module: &ModuleName,
        tenant: TenantId,
    ) -> Result<Option<TenantQuotas>, Error> {
        let body = json!({ "tenant": tenant.to_string() })
            .to_string()
            .into_bytes();
        let response = self
            .hook_route(module, "POST", TENANT_QUOTAS.to_owned(), body)
            .await?;
        match response.status {
            200 => quotas(&response.body).map(Some).map_err(|why| {
                Error::internal_msg(format!("module {module} answered tenant_quotas: {why}"))
            }),
            404 => Ok(None),
            status => Err(Error::internal_msg(format!(
                "module {module} answered tenant_quotas with {status}"
            ))),
        }
    }

    async fn project_tenant(
        &self,
        module: &ModuleName,
        actor: &User,
        tenant: Option<&TenantSlug>,
    ) -> Result<Option<TenantId>, Error> {
        let body = json!({
            "actor": actor_json(actor),
            "tenant": tenant.map(TenantSlug::as_str),
        })
        .to_string()
        .into_bytes();
        let response = self
            .hook_route(module, "POST", PROJECT_TENANT.to_owned(), body)
            .await?;
        let detail = || problem_detail(&response.body);
        match response.status {
            200 => chosen_tenant(&response.body).map_err(|why| {
                Error::internal_msg(format!("module {module} answered project_tenant: {why}"))
            }),
            400 => Err(Error::invalid("tenant", detail())),
            403 => Err(Error::forbidden(detail())),
            404 => Err(Error::not_found(detail())),
            409 => Err(Error::conflict(detail())),
            status => Err(Error::internal_msg(format!(
                "module {module} answered project_tenant with {status}"
            ))),
        }
    }

    async fn mcp_resource(&self, module: &ModuleName) -> Result<Option<McpResource>, Error> {
        let response = self
            .hook_route(module, "POST", MCP_RESOURCE.to_owned(), b"{}".to_vec())
            .await?;
        match response.status {
            200 => mcp_resource(&response.body).map(Some).map_err(|why| {
                Error::internal_msg(format!("module {module} answered mcp_resource: {why}"))
            }),
            404 => Ok(None),
            status => Err(Error::internal_msg(format!(
                "module {module} answered mcp_resource with {status}"
            ))),
        }
    }

    async fn web_panel(&self, module: &ModuleName) -> Result<Option<WebPanel>, Error> {
        let response = self
            .hook_route(module, "POST", WEB_PANEL.to_owned(), b"{}".to_vec())
            .await?;
        match response.status {
            200 => web_panel(&response.body).map(Some).map_err(|why| {
                Error::internal_msg(format!("module {module} answered web_panel: {why}"))
            }),
            404 => Ok(None),
            status => Err(Error::internal_msg(format!(
                "module {module} answered web_panel with {status}"
            ))),
        }
    }

    async fn verify_token(
        &self,
        module: &ModuleName,
        token: &str,
        audiences: &[String],
    ) -> Result<Option<OAuthIdentity>, Error> {
        let body = json!({ "token": token, "audiences": audiences })
            .to_string()
            .into_bytes();
        let response = self
            .hook_route(module, "POST", VERIFY_TOKEN.to_owned(), body)
            .await?;
        match response.status {
            200 => oauth_identity(&response.body).map(Some).map_err(|why| {
                Error::internal_msg(format!("module {module} answered verify_token: {why}"))
            }),
            401 => Err(Error::unauthenticated(problem_detail(&response.body))),
            404 => Ok(None),
            status => Err(Error::internal_msg(format!(
                "module {module} answered verify_token with {status}"
            ))),
        }
    }

    async fn handle_route(
        &self,
        module: &ModuleName,
        actor: Option<&User>,
        request: ModuleRequest,
    ) -> Result<ModuleResponse, Error> {
        let loaded = self.loaded(module)?;
        let request = RouteRequest {
            method: request.method,
            path: request.path,
            headers: request.headers,
            body: request.body,
        };
        let response = loaded.handle_route(actor, request).await?;
        Ok(ModuleResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }

    async fn handle_tool(
        &self,
        module: &ModuleName,
        actor: Option<&User>,
        name: &str,
        args: &str,
    ) -> Result<String, Error> {
        let loaded = self.loaded(module)?;
        Ok(match actor {
            Some(actor) => loaded.handle_tool_as(actor, name, args).await?,
            None => loaded.handle_tool(name, args).await?,
        })
    }
}

/// The path `authenticate(request)` is called on.
const AUTHENTICATE: &str = "/.hook/authenticate";

/// The prefix `render_page(kind)` is called on.
const RENDER_PAGE: &str = "/.hook/render_page/";

/// The path `tenant_quotas(tenant)` is called on.
const TENANT_QUOTAS: &str = "/.hook/tenant_quotas";

/// The path `project_tenant(actor, tenant)` is called on.
const PROJECT_TENANT: &str = "/.hook/project_tenant";

/// The path `mcp_resource()` is called on.
const MCP_RESOURCE: &str = "/.hook/mcp_resource";

/// The path `verify_token(token, audiences)` is called on.
const VERIFY_TOKEN: &str = "/.hook/verify_token";

/// The path `web_panel()` is called on.
const WEB_PANEL: &str = "/.hook/web_panel";

/// The path `render_page(page)` is called on:
/// `/.hook/render_page/<kind>`, with `?host=<host>&return=<url>` for
/// what the page knows of them.
fn render_page_path(page: &PageRequest) -> String {
    let mut path = format!("{RENDER_PAGE}{}", page.kind.as_str());
    let params: Vec<String> = [
        ("host", page.host.as_ref().map(ToString::to_string)),
        ("return", page.return_to.as_ref().map(ToString::to_string)),
    ]
    .into_iter()
    .filter_map(|(name, value)| Some(format!("{name}={}", query_component(&value?))))
    .collect();
    if !params.is_empty() {
        path.push('?');
        path.push_str(&params.join("&"));
    }
    path
}

/// `text` percent-encoded as a query component (RFC 3986 unreserved
/// characters kept).
fn query_component(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _written = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// The answer of `render_page`: `200` is the page's HTML; `302`, `303`
/// or `307` a redirect to its `location` ([`redirect_location`]); any
/// other status no opinion. A redirect whose `location` is not a
/// [`WebUrl`] is an error (logged by the dispatcher, the daemon's page
/// stays).
fn page_answer(
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Option<ModulePage>, String> {
    match status {
        200 => Ok(Some(ModulePage::Html(
            String::from_utf8_lossy(body).into_owned(),
        ))),
        302 | 303 | 307 => redirect_location(headers).map(|url| Some(ModulePage::Redirect(url))),
        _ => Ok(None),
    }
}

/// The `location` of a module's redirect: a [`WebUrl`], absolute
/// `https` without whitespace or control characters (modules are
/// untrusted: no relative, `javascript:` or cleartext target).
fn redirect_location(headers: &[(String, String)]) -> Result<WebUrl, String> {
    let location = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("location"))
        .map(|(_, value)| value.trim())
        .ok_or("a redirect without a location")?;
    WebUrl::new(location).map_err(|_invalid| format!("a location that is not {}", WebUrl::PATTERN))
}

/// The answer of `web_panel`: `{"origin"}`, an [`Origin`] (`https`, no
/// path), kept in lower case as browsers send it.
fn web_panel(body: &[u8]) -> Result<WebPanel, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        origin: String,
    }
    let answer: Answer = serde_json::from_slice(body).map_err(|why| why.to_string())?;
    let origin = Origin::new(answer.origin.trim())
        .map_err(|_invalid| format!("an origin that is not {}", Origin::PATTERN))?;
    Ok(WebPanel { origin })
}

/// The answer of `mcp_resource`: `{"resource","audiences"}`.
fn mcp_resource(body: &[u8]) -> Result<McpResource, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        resource: String,
        audiences: Vec<String>,
    }
    let answer: Answer = serde_json::from_slice(body).map_err(|why| why.to_string())?;
    if answer.resource.is_empty() || answer.audiences.is_empty() {
        return Err("an empty resource or no audience".to_owned());
    }
    Ok(McpResource {
        resource: answer.resource,
        audiences: answer.audiences,
    })
}

/// The answer of `verify_token`: `{"issuer","subject","login","email",
/// "role","expires_at"}` (`expires_at` in Unix seconds).
fn oauth_identity(body: &[u8]) -> Result<OAuthIdentity, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        issuer: String,
        subject: String,
        login: String,
        #[serde(default)]
        email: String,
        role: String,
        expires_at: i64,
    }
    let answer: Answer = serde_json::from_slice(body).map_err(|why| why.to_string())?;
    Ok(OAuthIdentity {
        issuer: answer.issuer,
        subject: answer.subject,
        login: Login::new(&answer.login).map_err(|why| why.to_string())?,
        email: answer.email,
        role: UserRole::parse(&answer.role).map_err(|why| why.to_string())?,
        expires_at: Timestamp::from_unix_timestamp(answer.expires_at)
            .map_err(|why| why.to_string())?,
    })
}

/// The answer of `project_tenant`: `{"tenant"}`, an id or `null`.
fn chosen_tenant(body: &[u8]) -> Result<Option<TenantId>, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        tenant: Option<String>,
    }
    let answer: Answer = serde_json::from_slice(body).map_err(|why| why.to_string())?;
    answer
        .tenant
        .map(|id| TenantId::parse(&id).map_err(|why| why.to_string()))
        .transpose()
}

/// The `detail` of a module's problem document, else its body as text.
fn problem_detail(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|problem| {
            problem
                .get("detail")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
}

/// The answer of `tenant_quotas`: `{"max_projects","env_cpus_hundredths",
/// "env_memory_mb"}`, each a number or `null`.
fn quotas(body: &[u8]) -> Result<TenantQuotas, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        #[serde(default)]
        max_projects: Option<u32>,
        #[serde(default)]
        env_cpus_hundredths: Option<u32>,
        #[serde(default)]
        env_memory_mb: Option<u32>,
    }
    let answer: Answer = serde_json::from_slice(body).map_err(|why| why.to_string())?;
    Ok(TenantQuotas {
        max_projects: answer.max_projects,
        env_cpus: answer
            .env_cpus_hundredths
            .map(Cpus::from_hundredths)
            .transpose()
            .map_err(|why| why.to_string())?,
        env_memory_mb: answer
            .env_memory_mb
            .map(MemoryMb::new)
            .transpose()
            .map_err(|why| why.to_string())?,
    })
}

/// The JSON payload of `on-hook` for `event` (see [`ModuleSet`]).
fn payload(event: &HookEvent<'_>) -> Value {
    match event {
        HookEvent::Authorize {
            actor,
            action,
            resource,
        } => json!({
            "actor": actor_json(actor),
            "action": action.as_str(),
            "resource": resource_json(resource),
        }),
        HookEvent::BeforeDeploy {
            environment,
            deploy,
        }
        | HookEvent::AfterDeploy {
            environment,
            deploy,
        } => json!({
            "environment": {
                "id": environment.id.to_string(),
                "project": environment.project_id.to_string(),
                "name": environment.name.to_string(),
                "limits": {
                    "cpus_hundredths": environment.limits.cpus.hundredths(),
                    "memory_mb": environment.limits.memory_mb.get(),
                },
            },
            "deploy": {
                "id": deploy.id.to_string(),
                "number": deploy.number.to_string(),
            },
        }),
        HookEvent::DomainCreated(domain) | HookEvent::DomainRemoved(domain) => json!({
            "domain": {
                "id": domain.id.to_string(),
                "host": domain.host.to_string(),
                "kind": domain.kind.as_str(),
                "proxied": domain.proxied,
            },
        }),
        HookEvent::Gate {
            host,
            actor,
            request,
        } => json!({
            "host": host.to_string(),
            "actor": actor_json(actor),
            "request": {
                "method": request.method,
                "path": request.path,
                "headers": request.headers,
            },
        }),
    }
}

/// An actor as hooks see it: `{"id","login","role"}`.
fn actor_json(actor: &User) -> Value {
    json!({
        "id": actor.id.to_string(),
        "login": actor.login.to_string(),
        "role": actor.role.as_str(),
    })
}

/// A [`Resource`] of the `authorize` payload.
fn resource_json(resource: &Resource) -> Value {
    match resource {
        Resource::Project(id) => json!({ "type": "project", "id": id.to_string() }),
        Resource::Environment(id) => json!({ "type": "environment", "id": id.to_string() }),
        Resource::Domain(id) => json!({ "type": "domain", "id": id.to_string() }),
        Resource::Secret(scope) => json!({
            "type": "secret",
            "scope": { "type": scope.kind().as_str(), "id": scope.id().to_string() },
        }),
        Resource::Tenant(id) => json!({ "type": "tenant", "id": id.to_string() }),
        Resource::Artifact(id) => json!({ "type": "artifact", "id": id.to_string() }),
        Resource::Admin => json!({ "type": "admin" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(value: &str) -> Vec<(String, String)> {
        vec![("Location".to_owned(), value.to_owned())]
    }

    mod page_answer {
        use super::*;

        // TC: WEB-03
        #[test]
        fn redirect_when_module_answers_found_with_absolute_url() {
            assert_eq!(
                page_answer(302, &location("https://panel.test/login?return=x"), b""),
                Ok(Some(ModulePage::Redirect(
                    WebUrl::new("https://panel.test/login?return=x").unwrap()
                )))
            );
        }

        // TC: WEB-03 (303 and 307 redirect too)
        #[test]
        fn redirect_when_module_answers_see_other_or_temporary() {
            let url = WebUrl::new("https://panel.test/login").unwrap();
            assert_eq!(
                (
                    page_answer(303, &location(url.as_str()), b""),
                    page_answer(307, &location(url.as_str()), b"")
                ),
                (
                    Ok(Some(ModulePage::Redirect(url.clone()))),
                    Ok(Some(ModulePage::Redirect(url)))
                )
            );
        }

        // TC: WEB-03 (a permanent redirect would be cached by the
        // browser: no page)
        #[test]
        fn no_page_when_module_answers_moved_permanently() {
            assert_eq!(
                page_answer(301, &location("https://panel.test/login"), b""),
                Ok(None)
            );
        }

        // TC: WEB-03 (modules are untrusted: no relative, script or
        // cleartext target, no header injection)
        #[test]
        fn fails_when_location_is_not_absolute_https() {
            let accepted: Vec<&str> = [
                "/login",
                "//evil.test/",
                "javascript:alert(1)",
                "http://evil.test/",
                "https://panel.test/a\r\nset-cookie: x",
                "https://a@panel.test/",
            ]
            .into_iter()
            .filter(|value| page_answer(302, &location(value), b"").is_ok())
            .collect();
            assert_eq!(accepted, Vec::<&str>::new());
        }

        // TC: WEB-03
        #[test]
        fn fails_when_redirect_has_no_location() {
            assert!(page_answer(302, &[], b"").is_err());
        }

        // TC: WEB-03 (an HTML page, as before)
        #[test]
        fn html_when_module_answers_ok() {
            assert_eq!(
                page_answer(200, &[], b"<h1>Sign in</h1>"),
                Ok(Some(ModulePage::Html("<h1>Sign in</h1>".to_owned())))
            );
        }
    }

    mod web_panel {
        use super::*;

        fn answer(origin: &str) -> Result<String, String> {
            web_panel(json!({ "origin": origin }).to_string().as_bytes())
                .map(|panel| panel.origin.to_string())
        }

        // TC: WEB-01 (kept as browsers send it)
        #[test]
        fn lower_cases_origin_when_module_answers_upper_case() {
            assert_eq!(
                answer("HTTPS://Panel.Test"),
                Ok("https://panel.test".to_owned())
            );
        }

        // TC: WEB-01 (never a value Access-Control-Allow-Origin cannot
        // carry, never cleartext)
        #[test]
        fn fails_when_origin_has_path_userinfo_or_is_cleartext() {
            let accepted: Vec<&str> = [
                "https://panel.test/path",
                "https://a@b.test",
                "https://panel.test?x",
                "http://panel.test",
                "null",
                "*",
            ]
            .into_iter()
            .filter(|origin| answer(origin).is_ok())
            .collect();
            assert_eq!(accepted, Vec::<&str>::new());
        }
    }
}
