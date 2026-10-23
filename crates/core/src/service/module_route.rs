//! [`ModuleRouteService`]: `/api/v1/m/{module}/...` and `/api/v1/modules/{module}/tools/{tool}`, the
//! MCP tools of modules.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::domain::{Bearer, ModuleName, User};
use crate::error::Error;
use crate::ports::{HookRequest, Hooks as _, ModuleRequest, ModuleResponse, Ports};
use crate::service::web_panel::with_same_site;
use crate::service::{AuthService, WebPanelService};

/// Hands a request below `/api/v1/m/<module>` to the module's route.
///
/// The route is the one the module declares
/// ([`Hooks::module_route`](crate::ports::Hooks::module_route)): a module
/// that is not installed or a route it does not declare is
/// [`Error::NotFound`] (HOOK-06), a disabled module
/// [`Error::ModuleRequired`] (HOOK-05). A route marked `public` runs
/// without an actor; any other runs as the actor
/// [`AuthService::authenticate`](crate::service::AuthService::authenticate)
/// finds (the `authenticate` hook, then the bearer session) and answers
/// [`Error::Unauthenticated`] without one (HOOK-03).
///
/// Modules are untrusted, and the browser session on the API
/// host is the core's: only the browser login module
/// ([`ModuleRequest::BROWSER_LOGIN_MODULE`]) sees the browser's login
/// states ([`ModuleRequest::LOGIN_STATE_HEADER`]) and may set the cookies
/// `__Host-liftoff_session` and `__Host-liftoff_login_*`. Every other
/// `Set-Cookie` in the core's namespace (`__Host-liftoff_*`, any case)
/// that a module answers is dropped, so no module fixes a browser's
/// session for the gate or plants a login state.
#[derive(Debug)]
pub struct ModuleRouteService<P> {
    ports: Arc<P>,
}

impl<P> Clone for ModuleRouteService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
        }
    }
}

impl<P: Ports> ModuleRouteService<P> {
    /// The service over `ports`.
    #[must_use]
    pub const fn new(ports: Arc<P>) -> Self {
        Self { ports }
    }

    /// Hands `request` to the route of `module`; `hook` and `bearer` are
    /// what authenticates it (as for every other route).
    ///
    /// The route is matched on the request's path without its query;
    /// the module sees the query (an OIDC callback's `code` and
    /// `state`).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::ModuleRequired`],
    /// [`Error::Unauthenticated`] as above; whatever the module's call
    /// failed with.
    pub async fn handle(
        &self,
        module: &ModuleName,
        request: ModuleRequest,
        hook: &HookRequest,
        bearer: Option<&Bearer>,
    ) -> Result<ModuleResponse, Error> {
        let login = module.as_str() == ModuleRequest::BROWSER_LOGIN_MODULE;
        let mut request = request;
        if !login {
            request
                .headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case(ModuleRequest::LOGIN_STATE_HEADER));
        }
        let mut response = self.route(module, request, hook, bearer).await?;
        response.headers.retain(|(name, value)| {
            let kept = !name.eq_ignore_ascii_case("set-cookie") || may_set(login, value);
            if !kept {
                tracing::warn!(%module, "dropped a cookie of the core a module tried to set");
            }
            kept
        });
        if login {
            self.follow_panel(&mut response).await;
        }
        Ok(response)
    }

    /// The browser session's cookie the login module sets, with the
    /// `SameSite` of the core's own ([`WebPanelService::same_site`]: `None` while the panel `liftoff-web` is kept).
    async fn follow_panel(&self, response: &mut ModuleResponse) {
        let sets_session = |name: &str, value: &str| {
            name.eq_ignore_ascii_case("set-cookie") && cookie_name(value) == session_cookie_name()
        };
        if !response
            .headers
            .iter()
            .any(|(name, value)| sets_session(name, value))
        {
            return;
        }
        let same_site = WebPanelService::new(Arc::clone(&self.ports))
            .same_site()
            .await;
        for (name, value) in &mut response.headers {
            if sets_session(name, value) {
                *value = with_same_site(value, same_site);
            }
        }
    }

    /// Calls the MCP tool `tool` of `module` with `arguments` as `actor`
    /// (`lof mcp` and any other client of the API).
    ///
    /// The tool is one the module declares and runs as `actor`, as a
    /// module route behind a session does: the module's `core-call`s
    /// carry the actor's rights, so the core asks no `authorize` of its
    /// own here. The module's answer must be JSON.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a module that is not installed or a tool
    /// it does not declare, [`Error::ModuleRequired`] for a disabled
    /// module, whatever the module's call failed with, and
    /// [`Error::Internal`] for an answer that is not JSON.
    pub async fn call_tool(
        &self,
        actor: &User,
        module: &ModuleName,
        tool: &str,
        arguments: &Map<String, Value>,
    ) -> Result<Value, Error> {
        let args = Value::Object(arguments.clone()).to_string();
        let answer = self
            .ports
            .hooks()
            .handle_tool(module, actor, tool, &args)
            .await?;
        serde_json::from_str(&answer).map_err(|error| {
            tracing::error!(%module, tool, %error, "a module answered a tool call that is not JSON");
            Error::internal_msg(format!(
                "module {module} answered tool {tool} with something that is not JSON"
            ))
        })
    }

    /// [`ModuleRouteService::handle`] without the cookie rules.
    async fn route(
        &self,
        module: &ModuleName,
        request: ModuleRequest,
        hook: &HookRequest,
        bearer: Option<&Bearer>,
    ) -> Result<ModuleResponse, Error> {
        let path = request
            .path
            .split_once('?')
            .map_or(request.path.as_str(), |(path, _query)| path);
        let route = self
            .ports
            .hooks()
            .module_route(module, &request.method, path)
            .await?;
        let hooks = self.ports.hooks();
        if route.public {
            return hooks.handle_route(module, None, request).await;
        }
        let principal = AuthService::new(Arc::clone(&self.ports))
            .authenticate(hook, bearer)
            .await?;
        hooks
            .handle_route(module, Some(principal.user()), request)
            .await
    }
}

/// The name of the cookie a `Set-Cookie` `value` sets, in lower case.
fn cookie_name(value: &str) -> String {
    value
        .split_once('=')
        .map_or(value, |(name, _)| name)
        .trim()
        .to_ascii_lowercase()
}

/// The name of the browser session's cookie, in lower case.
fn session_cookie_name() -> String {
    ModuleRequest::SESSION_COOKIE.to_ascii_lowercase()
}

/// The prefix of the core's cookies on the API host.
const CORE_COOKIE_PREFIX: &str = "__host-liftoff_";

/// Whether a module may answer the `Set-Cookie` `value`: any cookie
/// outside the core's namespace; in it, the browser session and the
/// login states, for the browser login module (`login`) alone.
fn may_set(login: bool, value: &str) -> bool {
    let name = cookie_name(value);
    if !name.starts_with(CORE_COOKIE_PREFIX) {
        return true;
    }
    login
        && (name == session_cookie_name()
            || name.starts_with(&ModuleRequest::LOGIN_STATE_COOKIE_PREFIX.to_ascii_lowercase()))
}
