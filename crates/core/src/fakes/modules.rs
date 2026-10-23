//! [`FakeModules`].

use core::future::ready;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::log::lock;
use crate::domain::{ModuleName, TenantId, TenantQuotas, TenantSlug, User, UserId, WebUrl};
use crate::error::Error;
use crate::module::{HookEvent, HookName, ModuleCalls};
use crate::ports::{
    AuthResult, HookRequest, HookResult, McpResource, ModulePage, ModuleRequest, ModuleResponse,
    OAuthIdentity, PageKind, PageRequest, Resource, WebPanel,
};

/// A call [`FakeModules`] received, in the order it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleCall {
    /// `on-hook` of `module` for `hook`.
    Hook {
        /// The module asked.
        module: String,
        /// The hook.
        hook: HookName,
    },
    /// `authenticate` of `module`, with the request path.
    Authenticate {
        /// The module asked.
        module: String,
        /// The request path.
        path: String,
    },
    /// `render_page(kind)` of `module`.
    RenderPage {
        /// The module asked.
        module: String,
        /// The page.
        kind: PageKind,
    },
    /// `handle-route` of `module`.
    Route {
        /// The module asked.
        module: String,
        /// The actor the route ran as.
        actor: Option<UserId>,
        /// The method.
        method: String,
        /// The path below the module's prefix.
        path: String,
    },
    /// `handle-tool` of `module`.
    Tool {
        /// The module asked.
        module: String,
        /// The actor the tool ran as.
        actor: Option<UserId>,
        /// The tool.
        name: String,
        /// Its JSON arguments.
        args: String,
    },
}

#[derive(Debug, Default)]
struct Script {
    answers: HashMap<(String, HookName), HookResult>,
    denied: HashMap<String, Vec<(Resource, String)>>,
    actors: HashMap<String, AuthResult>,
    pages: HashMap<(String, PageKind), ModulePage>,
    responses: HashMap<String, ModuleResponse>,
    tools: HashMap<(String, String), Result<String, Error>>,
    failures: HashMap<String, Error>,
    resources: HashMap<String, McpResource>,
    panels: HashMap<String, WebPanel>,
    identities: HashMap<String, Result<Option<OAuthIdentity>, Error>>,
    calls: Vec<ModuleCall>,
    route_headers: Vec<Vec<(String, String)>>,
}

/// Modules in process, behind the same [`ModuleCalls`] as the module
/// host: a hand-written fake scripted per module.
///
/// - [`FakeModules::answer`]: every `on-hook` of a module for a hook
///   answers this (default [`HookResult::Continue`]);
/// - [`FakeModules::deny_resource`]: a `tenants`-like module whose
///   `authorize` denies one resource (a foreign project) and has no
///   opinion on the rest;
/// - [`FakeModules::authenticate_as`], [`FakeModules::page`],
///   [`FakeModules::respond`]: what `authenticate`, `render_page` and
///   every route of a module answer (defaults: `Continue`, `None`, `200`
///   with the actor's ID or `anonymous` as the body);
/// - [`FakeModules::tool`]: what a tool of a module answers (default:
///   [`Error::NotFound`], a tool the module does not have);
///   `tenant_quotas` and `project_tenant` answer no opinion;
/// - [`FakeModules::keep_resource`], [`FakeModules::verify_as`]: what
///   `mcp_resource` and `verify_token` of a module answer (default: no
///   opinion); [`FakeModules::keep_panel`] what its `web_panel` answers
///   (default: no opinion);
/// - [`FakeModules::fail`]: every call of a module fails (a trap, as the
///   host reports it), after it is logged;
/// - [`FakeModules::calls`]: every call, oldest first.
#[derive(Debug, Clone, Default)]
pub struct FakeModules {
    script: Arc<Mutex<Script>>,
}

impl FakeModules {
    /// Modules with no opinion on anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every `on-hook` of `module` for `hook` answers `answer`.
    pub fn answer(&self, module: &str, hook: HookName, answer: HookResult) {
        let _previous = lock(&self.script)
            .answers
            .insert((module.to_owned(), hook), answer);
    }

    /// `authorize` of `module` denies `resource` with `reason`, whoever
    /// asks; other resources get the module's [`FakeModules::answer`].
    pub fn deny_resource(&self, module: &str, resource: Resource, reason: &str) {
        lock(&self.script)
            .denied
            .entry(module.to_owned())
            .or_default()
            .push((resource, reason.to_owned()));
    }

    /// `authenticate` of `module` answers `answer`.
    pub fn authenticate_as(&self, module: &str, answer: AuthResult) {
        let _previous = lock(&self.script).actors.insert(module.to_owned(), answer);
    }

    /// `render_page(kind)` of `module` answers `html`.
    pub fn page(&self, module: &str, kind: PageKind, html: &str) {
        let _previous = lock(&self.script)
            .pages
            .insert((module.to_owned(), kind), ModulePage::Html(html.to_owned()));
    }

    /// `render_page(kind)` of `module` sends the browser to `url`.
    pub fn page_redirect(&self, module: &str, kind: PageKind, url: WebUrl) {
        let _previous = lock(&self.script)
            .pages
            .insert((module.to_owned(), kind), ModulePage::Redirect(url));
    }

    /// Every route of `module` answers `response`.
    pub fn respond(&self, module: &str, response: ModuleResponse) {
        let _previous = lock(&self.script)
            .responses
            .insert(module.to_owned(), response);
    }

    /// The tool `name` of `module` answers `answer`.
    pub fn tool(&self, module: &str, name: &str, answer: Result<String, Error>) {
        let _previous = lock(&self.script)
            .tools
            .insert((module.to_owned(), name.to_owned()), answer);
    }

    /// Every call of `module` fails with `error`.
    pub fn fail(&self, module: &str, error: Error) {
        let _previous = lock(&self.script).failures.insert(module.to_owned(), error);
    }

    /// The headers of every route request so far, oldest first.
    #[must_use]
    pub fn route_headers(&self) -> Vec<Vec<(String, String)>> {
        lock(&self.script).route_headers.clone()
    }

    /// `mcp_resource` of `module` answers `resource`.
    pub fn keep_resource(&self, module: &str, resource: McpResource) {
        let _previous = lock(&self.script)
            .resources
            .insert(module.to_owned(), resource);
    }

    /// `web_panel` of `module` answers `panel`.
    pub fn keep_panel(&self, module: &str, panel: WebPanel) {
        let _previous = lock(&self.script).panels.insert(module.to_owned(), panel);
    }

    /// `verify_token` of `module` answers `answer`, whatever the token.
    pub fn verify_as(&self, module: &str, answer: Result<Option<OAuthIdentity>, Error>) {
        let _previous = lock(&self.script)
            .identities
            .insert(module.to_owned(), answer);
    }

    /// The calls received so far, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<ModuleCall> {
        lock(&self.script).calls.clone()
    }

    /// The modules asked about `hook` so far, in order.
    #[must_use]
    pub fn asked(&self, hook: HookName) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                ModuleCall::Hook {
                    module,
                    hook: asked,
                } if asked == hook => Some(module),
                _ => None,
            })
            .collect()
    }
}

impl ModuleCalls for FakeModules {
    fn on_hook(
        &self,
        module: &ModuleName,
        event: &HookEvent<'_>,
    ) -> impl Future<Output = Result<HookResult, Error>> + Send {
        let hook = event.hook();
        let mut script = lock(&self.script);
        script.calls.push(ModuleCall::Hook {
            module: module.to_string(),
            hook,
        });
        if let Some(error) = script.failures.get(module.as_str()).cloned() {
            return ready(Err(error));
        }
        let denied = match event {
            HookEvent::Authorize { resource, .. } => script
                .denied
                .get(module.as_str())
                .and_then(|denied| denied.iter().find(|(known, _)| known == *resource))
                .map(|(_, reason)| HookResult::Deny(reason.clone())),
            _ => None,
        };
        let answer = denied.unwrap_or_else(|| {
            script
                .answers
                .get(&(module.to_string(), hook))
                .cloned()
                .unwrap_or(HookResult::Continue)
        });
        drop(script);
        ready(Ok(answer))
    }

    fn authenticate(
        &self,
        module: &ModuleName,
        request: &HookRequest,
    ) -> impl Future<Output = Result<AuthResult, Error>> + Send {
        let mut script = lock(&self.script);
        script.calls.push(ModuleCall::Authenticate {
            module: module.to_string(),
            path: request.path.clone(),
        });
        if let Some(error) = script.failures.get(module.as_str()).cloned() {
            return ready(Err(error));
        }
        ready(Ok(script
            .actors
            .get(module.as_str())
            .cloned()
            .unwrap_or(AuthResult::Continue)))
    }

    fn mcp_resource(
        &self,
        module: &ModuleName,
    ) -> impl Future<Output = Result<Option<McpResource>, Error>> + Send {
        let script = lock(&self.script);
        ready(script.failures.get(module.as_str()).map_or_else(
            || Ok(script.resources.get(module.as_str()).cloned()),
            |error| Err(error.clone()),
        ))
    }

    fn web_panel(
        &self,
        module: &ModuleName,
    ) -> impl Future<Output = Result<Option<WebPanel>, Error>> + Send {
        let script = lock(&self.script);
        ready(script.failures.get(module.as_str()).map_or_else(
            || Ok(script.panels.get(module.as_str()).cloned()),
            |error| Err(error.clone()),
        ))
    }

    fn verify_token(
        &self,
        module: &ModuleName,
        _token: &str,
        _audiences: &[String],
    ) -> impl Future<Output = Result<Option<OAuthIdentity>, Error>> + Send {
        let script = lock(&self.script);
        ready(script.failures.get(module.as_str()).map_or_else(
            || {
                script
                    .identities
                    .get(module.as_str())
                    .cloned()
                    .unwrap_or(Ok(None))
            },
            |error| Err(error.clone()),
        ))
    }

    fn tenant_quotas(
        &self,
        module: &ModuleName,
        _tenant: TenantId,
    ) -> impl Future<Output = Result<Option<TenantQuotas>, Error>> + Send {
        let script = lock(&self.script);
        ready(
            script
                .failures
                .get(module.as_str())
                .cloned()
                .map_or(Ok(None), Err),
        )
    }

    fn project_tenant(
        &self,
        module: &ModuleName,
        _actor: &User,
        _tenant: Option<&TenantSlug>,
    ) -> impl Future<Output = Result<Option<TenantId>, Error>> + Send {
        let script = lock(&self.script);
        ready(
            script
                .failures
                .get(module.as_str())
                .cloned()
                .map_or(Ok(None), Err),
        )
    }

    fn render_page(
        &self,
        module: &ModuleName,
        page: &PageRequest,
    ) -> impl Future<Output = Result<Option<ModulePage>, Error>> + Send {
        let kind = page.kind;
        let mut script = lock(&self.script);
        script.calls.push(ModuleCall::RenderPage {
            module: module.to_string(),
            kind,
        });
        if let Some(error) = script.failures.get(module.as_str()).cloned() {
            return ready(Err(error));
        }
        ready(Ok(script.pages.get(&(module.to_string(), kind)).cloned()))
    }

    fn handle_route(
        &self,
        module: &ModuleName,
        actor: Option<&User>,
        request: ModuleRequest,
    ) -> impl Future<Output = Result<ModuleResponse, Error>> + Send {
        let mut script = lock(&self.script);
        script.route_headers.push(request.headers);
        script.calls.push(ModuleCall::Route {
            module: module.to_string(),
            actor: actor.map(|actor| actor.id),
            method: request.method,
            path: request.path,
        });
        if let Some(error) = script.failures.get(module.as_str()).cloned() {
            return ready(Err(error));
        }
        let response = script.responses.get(module.as_str()).cloned();
        drop(script);
        ready(Ok(response.unwrap_or_else(|| ModuleResponse {
            status: 200,
            headers: Vec::new(),
            body: actor
                .map_or_else(|| "anonymous".to_owned(), |actor| actor.id.to_string())
                .into_bytes(),
        })))
    }

    fn handle_tool(
        &self,
        module: &ModuleName,
        actor: Option<&User>,
        name: &str,
        args: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send {
        let mut script = lock(&self.script);
        script.calls.push(ModuleCall::Tool {
            module: module.to_string(),
            actor: actor.map(|actor| actor.id),
            name: name.to_owned(),
            args: args.to_owned(),
        });
        if let Some(error) = script.failures.get(module.as_str()).cloned() {
            return ready(Err(error));
        }
        let answer = script
            .tools
            .get(&(module.to_string(), name.to_owned()))
            .cloned()
            .unwrap_or_else(|| Err(Error::not_found(format!("tool {name} of module {module}"))));
        drop(script);
        ready(answer)
    }
}
