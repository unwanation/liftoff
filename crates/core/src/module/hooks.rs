//! [`ModuleHooks`]: the core's [`Hooks`] answered by the installed modules.
//!
//! The rules are those of [`ModuleHooks`].

use core::future::ready;
use std::sync::Arc;

use super::manifest::{HookName, RouteSpec};
use super::registry::ModuleRegistry;
use crate::domain::{
    Deploy, Domain, Environment, Host, ModuleName, TenantId, TenantQuotas, TenantSlug, User, WebUrl,
};
use crate::error::Error;
use crate::ports::{
    Action, AuthResult, HookRequest, HookResult, Hooks, McpResource, ModulePage, ModuleRequest,
    ModuleResponse, OAuthIdentity, PageRequest, Resource, WebPanel,
};
use crate::service::module::check_schema;
use crate::service::{MCP_MODULE, WEB_MODULE};

/// What a hook is about, as the dispatcher hands it to one module.
#[derive(Debug, Clone, Copy)]
pub enum HookEvent<'a> {
    /// `authorize(actor, action, resource)`.
    Authorize {
        /// Who acts.
        actor: &'a User,
        /// What they ask.
        action: Action,
        /// What it is about.
        resource: &'a Resource,
    },
    /// `before_deploy(environment, deploy)`.
    BeforeDeploy {
        /// The environment deployed to.
        environment: &'a Environment,
        /// The deploy.
        deploy: &'a Deploy,
    },
    /// `after_deploy(environment, deploy)`.
    AfterDeploy {
        /// The environment deployed to.
        environment: &'a Environment,
        /// The deploy.
        deploy: &'a Deploy,
    },
    /// `domain_created(domain)`.
    DomainCreated(&'a Domain),
    /// `domain_removed(domain)`.
    DomainRemoved(&'a Domain),
    /// `gate(host, request)` for the signed-in `actor`.
    Gate {
        /// The application host.
        host: &'a Host,
        /// Who asks to pass (the module runs as them).
        actor: &'a User,
        /// The request.
        request: &'a HookRequest,
    },
}

impl HookEvent<'_> {
    /// The hook this event is raised on.
    #[must_use]
    pub const fn hook(&self) -> HookName {
        match self {
            Self::Authorize { .. } => HookName::Authorize,
            Self::BeforeDeploy { .. } => HookName::BeforeDeploy,
            Self::AfterDeploy { .. } => HookName::AfterDeploy,
            Self::DomainCreated(_) => HookName::DomainCreated,
            Self::DomainRemoved(_) => HookName::DomainRemoved,
            Self::Gate { .. } => HookName::Gate,
        }
    }
}

/// Calls into the exports of one loaded module, as the module host makes
/// them.
///
/// `liftoff-wasm`'s `ModuleSet` in the daemon, a hand-written fake in
/// tests. The host translates to the WIT world and back; the rules of the
/// dispatch live in [`ModuleHooks`].
pub trait ModuleCalls: Send + Sync + 'static {
    /// `on-hook` of `module` for `event`: its decision.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a module the host has not loaded; whatever
    /// its call failed with (a trap, a timeout, its own error).
    fn on_hook(
        &self,
        module: &ModuleName,
        event: &HookEvent<'_>,
    ) -> impl Future<Output = Result<HookResult, Error>> + Send;

    /// `authenticate(request)` of `module`.
    ///
    /// # Errors
    ///
    /// As [`ModuleCalls::on_hook`].
    fn authenticate(
        &self,
        module: &ModuleName,
        request: &HookRequest,
    ) -> impl Future<Output = Result<AuthResult, Error>> + Send;

    /// `render_page(page)` of `module`: its HTML or where it sends the
    /// browser, `None` for no opinion.
    ///
    /// # Errors
    ///
    /// As [`ModuleCalls::on_hook`].
    fn render_page(
        &self,
        module: &ModuleName,
        page: &PageRequest,
    ) -> impl Future<Output = Result<Option<ModulePage>, Error>> + Send;

    /// `tenant_quotas(tenant)` of `module`: the quotas it keeps for
    /// `tenant`, `None` for no opinion. A host
    /// that cannot ask has none.
    ///
    /// # Errors
    ///
    /// As [`ModuleCalls::on_hook`].
    fn tenant_quotas(
        &self,
        _module: &ModuleName,
        _tenant: TenantId,
    ) -> impl Future<Output = Result<Option<TenantQuotas>, Error>> + Send {
        ready(Ok(None))
    }

    /// `project_tenant(actor, tenant)` of `module`: the tenant of a new
    /// project of `actor`, `None` for no opinion. A host that cannot ask has none.
    ///
    /// # Errors
    ///
    /// As [`ModuleCalls::on_hook`], and what the module refused with.
    fn project_tenant(
        &self,
        _module: &ModuleName,
        _actor: &User,
        _tenant: Option<&TenantSlug>,
    ) -> impl Future<Output = Result<Option<TenantId>, Error>> + Send {
        ready(Ok(None))
    }

    /// `mcp_resource()` of `module`: the protected resource of `/mcp` it
    /// keeps, `None` for no opinion. A host that
    /// cannot ask has none.
    ///
    /// # Errors
    ///
    /// As [`ModuleCalls::on_hook`].
    fn mcp_resource(
        &self,
        _module: &ModuleName,
    ) -> impl Future<Output = Result<Option<McpResource>, Error>> + Send {
        ready(Ok(None))
    }

    /// `verify_token(token, audiences)` of `module`: who the OAuth access
    /// token stands for, `None` for no opinion.
    /// A host that cannot ask has none.
    ///
    /// # Errors
    ///
    /// [`Error::Unauthenticated`] for a token the module refuses; as
    /// [`ModuleCalls::on_hook`].
    fn verify_token(
        &self,
        _module: &ModuleName,
        _token: &str,
        _audiences: &[String],
    ) -> impl Future<Output = Result<Option<OAuthIdentity>, Error>> + Send {
        ready(Ok(None))
    }

    /// `web_panel()` of `module`: the panel `liftoff-web` it keeps, `None`
    /// for no opinion. A host that cannot ask
    /// has none.
    ///
    /// # Errors
    ///
    /// As [`ModuleCalls::on_hook`].
    fn web_panel(
        &self,
        _module: &ModuleName,
    ) -> impl Future<Output = Result<Option<WebPanel>, Error>> + Send {
        ready(Ok(None))
    }

    /// `handle-route(request)` of `module` as `actor` (`None` on a public
    /// route).
    ///
    /// # Errors
    ///
    /// As [`ModuleCalls::on_hook`].
    fn handle_route(
        &self,
        module: &ModuleName,
        actor: Option<&User>,
        request: ModuleRequest,
    ) -> impl Future<Output = Result<ModuleResponse, Error>> + Send;

    /// `handle-tool(name, args)` of `module` as `actor` (`None` for a
    /// call the core makes on its own behalf, such as the
    /// [`ModuleGit`](super::ModuleGit) forge calls): its JSON answer.
    ///
    /// # Errors
    ///
    /// As [`ModuleCalls::on_hook`]; the module's own error maps by its
    /// code.
    fn handle_tool(
        &self,
        module: &ModuleName,
        actor: Option<&User>,
        name: &str,
        args: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send;
}

/// The core's [`Hooks`] answered by the modules of a [`ModuleRegistry`]
/// through `M`.
///
/// The dispatcher asks every enabled module subscribed to a hook
/// ([`ModuleRegistry::subscribers`]), in installation order, through a
/// [`ModuleCalls`] (the module host's; the core stays free of wasmtime):
///
/// - `authorize`, `before_deploy`, `after_deploy`, `domain_created`,
///   `domain_removed`, `gate`: the first [`HookResult::Deny`] wins and no
///   later module is asked; an [`HookResult::Allow`] does not stop a
///   later module from denying ("the first `deny` wins"); with
///   no deny, any allow is [`HookResult::Allow`], else
///   [`HookResult::Continue`] (HOOK-01);
/// - `authenticate`: the first module that names an actor
///   ([`AuthResult::Actor`]) or refuses ([`AuthResult::Deny`]) decides;
///   the core then does not check the bearer (HOOK-02);
/// - `render_page(page)`: the first module that answers a page (HTML or
///   a redirect) replaces the core's page; none keeps it (HOOK-04);
/// - `web_panel()` is asked of the module named `web` alone (another
///   subscriber could otherwise open CORS with credentials for an origin
///   of its choosing), and a failed call fails the question;
/// - `mcp_resource()` is asked of the module named `mcp` alone, and
///   `verify_token(token, audiences)` of the modules that replace `login`
///   alone (the one allowed to open sessions): another subscriber has no
///   say; a refusal of `verify_token` is [`Error::Unauthenticated`], a
///   failed call [`Error::Unavailable`];
/// - `tenant_quotas(tenant)` and `project_tenant(actor, tenant)`: the
///   first module that answers decides; a module whose call fails fails
///   the operation that asked ([`Error::Unavailable`] naming it, the
///   cause logged; fail closed: a quota is never skipped);
/// - module routes (`/api/v1/m/<module>/...`): the route the module
///   declares, [`Error::NotFound`] for a module that is not installed
///   (HOOK-06) and [`Error::ModuleRequired`] for a disabled one, whose
///   hooks are skipped too (HOOK-05);
/// - module tools (`POST /api/v1/modules/<module>/tools/<tool>`): a tool
///   the module declares, run as the caller; the same refusals,
///   [`Error::NotFound`] for a tool it does not declare, and
///   [`Error::Validation`] of `arguments.<path>` for arguments the tool's
///   declared `schema` refuses: modules are untrusted, so the
///   core checks at the boundary and the module never sees them.
#[derive(Debug)]
pub struct ModuleHooks<M> {
    registry: Arc<ModuleRegistry>,
    calls: M,
}

impl<M: ModuleCalls> ModuleHooks<M> {
    /// The dispatcher over the modules of `registry`, called through
    /// `calls`.
    #[must_use]
    pub const fn new(registry: Arc<ModuleRegistry>, calls: M) -> Self {
        Self { registry, calls }
    }

    /// The registry the dispatcher reads.
    #[must_use]
    pub const fn registry(&self) -> &Arc<ModuleRegistry> {
        &self.registry
    }

    /// The calls into the modules.
    #[must_use]
    pub const fn calls(&self) -> &M {
        &self.calls
    }
}

/// The reason a deciding hook denies with when `module`'s call failed
/// (the dispatcher fails closed; see [`ModuleHooks`]).
#[must_use]
pub fn failed_to_answer(module: &ModuleName) -> String {
    format!("module {module} failed to answer")
}

impl<M: ModuleCalls> ModuleHooks<M> {
    /// `event` dispatched to the subscribers of its hook: the first deny
    /// wins and stops the dispatch, else any allow, else continue.
    ///
    /// A module whose call fails (a trap, a timeout, its own error) is
    /// asked a decision it could not give: on a hook that decides
    /// (`authorize`, `before_deploy`, `gate`) the failure denies, so a
    /// broken `tenants` never opens a foreign project; on a notification
    /// (`after_deploy`, `domain_*`) it is logged and skipped. The host
    /// counts the failure toward disabling the module (MOD-09), after
    /// which its hooks are skipped.
    async fn dispatch(&self, event: HookEvent<'_>) -> HookResult {
        let hook = event.hook();
        let decides = matches!(
            hook,
            HookName::Authorize | HookName::BeforeDeploy | HookName::Gate
        );
        let mut answer = HookResult::Continue;
        for module in self.registry.subscribers(hook) {
            match self.calls.on_hook(&module, &event).await {
                Ok(HookResult::Deny(reason)) => return HookResult::Deny(reason),
                Ok(HookResult::Allow) => answer = HookResult::Allow,
                // Disabled since the dispatch began (MOD-09): its hooks
                // are skipped (HOOK-05).
                Ok(HookResult::Continue) | Err(Error::ModuleRequired(_)) => {}
                Err(error) => {
                    tracing::error!(%module, hook = hook.as_str(), %error, "a module failed a hook");
                    if decides {
                        return HookResult::Deny(failed_to_answer(&module));
                    }
                }
            }
        }
        answer
    }

    /// Whether `module`'s page redirect to `url` may be followed: only
    /// the module `web`'s, and only to the origin of the panel it keeps
    /// now ([`Hooks::web_panel`]).
    async fn on_panel(&self, module: &ModuleName, url: &WebUrl) -> bool {
        if module.as_str() != WEB_MODULE {
            return false;
        }
        matches!(
            Hooks::web_panel(self).await,
            Ok(Some(panel)) if panel.origin == url.origin()
        )
    }

    /// Whether `module` takes route calls: [`Error::NotFound`] for a
    /// module that is not installed (HOOK-06), [`Error::ModuleRequired`]
    /// for a disabled one (HOOK-05).
    fn routable(&self, module: &ModuleName) -> Result<(), Error> {
        match self.registry.status(module) {
            None => Err(Error::not_found(format!("module {module}"))),
            Some(status) if !status.enabled => Err(Error::module_required(module.as_str())),
            Some(_) => Ok(()),
        }
    }
}

impl<M: ModuleCalls> Hooks for ModuleHooks<M> {
    /// The first module that names an actor or refuses decides; a module
    /// whose call fails is skipped (the core still checks the bearer).
    async fn authenticate(&self, request: &HookRequest) -> AuthResult {
        for module in self.registry.subscribers(HookName::Authenticate) {
            match self.calls.authenticate(&module, request).await {
                Ok(AuthResult::Continue) => {}
                Ok(decided) => return decided,
                Err(error) => {
                    tracing::error!(%module, %error, "a module failed the authenticate hook");
                }
            }
        }
        AuthResult::Continue
    }

    async fn authorize(&self, actor: &User, action: Action, resource: &Resource) -> HookResult {
        self.dispatch(HookEvent::Authorize {
            actor,
            action,
            resource,
        })
        .await
    }

    async fn before_deploy(&self, environment: &Environment, deploy: &Deploy) -> HookResult {
        self.dispatch(HookEvent::BeforeDeploy {
            environment,
            deploy,
        })
        .await
    }

    async fn after_deploy(&self, environment: &Environment, deploy: &Deploy) -> HookResult {
        self.dispatch(HookEvent::AfterDeploy {
            environment,
            deploy,
        })
        .await
    }

    async fn domain_created(&self, domain: &Domain) -> HookResult {
        self.dispatch(HookEvent::DomainCreated(domain)).await
    }

    async fn domain_removed(&self, domain: &Domain) -> HookResult {
        self.dispatch(HookEvent::DomainRemoved(domain)).await
    }

    async fn gate(&self, host: &Host, actor: &User, request: &HookRequest) -> HookResult {
        self.dispatch(HookEvent::Gate {
            host,
            actor,
            request,
        })
        .await
    }

    /// The first module that answers quotas; a module whose call fails
    /// fails the call.
    async fn tenant_quotas(&self, tenant: TenantId) -> Result<Option<TenantQuotas>, Error> {
        for module in self.registry.subscribers(HookName::TenantQuotas) {
            match self.calls.tenant_quotas(&module, tenant).await {
                Ok(Some(quotas)) => return Ok(Some(quotas)),
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(%module, %tenant, %error, "a module failed to answer a tenant's quotas");
                    return Err(Error::unavailable(format!(
                        "module {module} failed to answer the tenant's quotas"
                    )));
                }
            }
        }
        Ok(None)
    }

    /// The first module that names a tenant; a module that refuses or
    /// fails fails the call.
    async fn project_tenant(
        &self,
        actor: &User,
        tenant: Option<&TenantSlug>,
    ) -> Result<Option<TenantId>, Error> {
        for module in self.registry.subscribers(HookName::ProjectTenant) {
            match self.calls.project_tenant(&module, actor, tenant).await {
                Ok(Some(chosen)) => return Ok(Some(chosen)),
                Ok(None) => {}
                Err(
                    refused @ (Error::NotFound(_)
                    | Error::Validation(_)
                    | Error::Forbidden(_)
                    | Error::Conflict(_)),
                ) => return Err(refused),
                Err(error) => {
                    tracing::error!(%module, %error, "a module failed to name the tenant of a new project");
                    return Err(Error::unavailable(format!(
                        "module {module} failed to name the tenant of the project"
                    )));
                }
            }
        }
        Ok(None)
    }

    /// The answer of the module named `mcp` alone (another module that
    /// subscribes has no say over the resource); a failed call fails the
    /// question.
    async fn mcp_resource(&self) -> Result<Option<McpResource>, Error> {
        for module in self.registry.subscribers(HookName::McpResource) {
            if module.as_str() != MCP_MODULE {
                continue;
            }
            return self.calls.mcp_resource(&module).await.map_err(|error| {
                tracing::error!(%module, %error, "a module failed to answer the resource of /mcp");
                Error::unavailable(format!(
                    "module {module} failed to answer the resource of /mcp"
                ))
            });
        }
        Ok(None)
    }

    /// The first module that replaces `login` (the one that may open
    /// sessions, as `auth.create_session` requires) and answers; its
    /// refusal is [`Error::Unauthenticated`], its failure
    /// [`Error::Unavailable`].
    async fn verify_token(
        &self,
        token: &str,
        audiences: &[String],
    ) -> Result<Option<OAuthIdentity>, Error> {
        for module in self.registry.subscribers(HookName::VerifyToken) {
            let logs_in = self
                .registry
                .manifest(&module)
                .is_some_and(|manifest| manifest.replaces.iter().any(|command| command == "login"));
            if !logs_in {
                continue;
            }
            match self.calls.verify_token(&module, token, audiences).await {
                Ok(Some(identity)) => return Ok(Some(identity)),
                Ok(None) => {}
                Err(Error::Unauthenticated(why)) => {
                    tracing::info!(%module, reason = %why, "a module refused an access token");
                    return Err(Error::unauthenticated(why));
                }
                Err(error) => {
                    tracing::error!(%module, %error, "a module failed to verify an access token");
                    return Err(Error::unavailable(format!(
                        "module {module} failed to verify the access token"
                    )));
                }
            }
        }
        Ok(None)
    }

    /// The answer of the module named `web` alone; a failed call fails
    /// the question.
    async fn web_panel(&self) -> Result<Option<WebPanel>, Error> {
        for module in self.registry.subscribers(HookName::WebPanel) {
            if module.as_str() != WEB_MODULE {
                continue;
            }
            return self.calls.web_panel(&module).await.map_err(|error| {
                tracing::error!(%module, %error, "a module failed to answer the panel");
                Error::unavailable(format!("module {module} failed to answer the panel"))
            });
        }
        Ok(None)
    }

    /// The first module that answers a page, the module `web` asked
    /// before the others (`take_pages` is the admin's choice of the panel
    /// over another module's page, `oidc`'s login among them); a module
    /// whose call fails is skipped. A redirect counts only from `web` and
    /// only to the origin of the panel it keeps now: any other is
    /// dropped (logged), so no module sends the browser elsewhere.
    async fn render_page(&self, page: &PageRequest) -> Option<ModulePage> {
        let mut subscribers = self.registry.subscribers(HookName::RenderPage);
        subscribers.sort_by_key(|module| module.as_str() != WEB_MODULE);
        for module in subscribers {
            match self.calls.render_page(&module, page).await {
                Ok(Some(ModulePage::Redirect(url))) => {
                    if self.on_panel(&module, &url).await {
                        return Some(ModulePage::Redirect(url));
                    }
                    tracing::warn!(%module, kind = page.kind.as_str(), origin = %url.origin(), "a module's page redirect is not to the panel; ignored");
                }
                Ok(Some(answer)) => return Some(answer),
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(%module, kind = page.kind.as_str(), %error, "a module failed to render a page");
                }
            }
        }
        None
    }

    fn module_route(
        &self,
        module: &ModuleName,
        method: &str,
        path: &str,
    ) -> impl Future<Output = Result<RouteSpec, Error>> + Send {
        ready(self.registry.route(module, method, path))
    }

    fn module_available(
        &self,
        module: &ModuleName,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(self.routable(module))
    }

    async fn handle_route(
        &self,
        module: &ModuleName,
        actor: Option<&User>,
        request: ModuleRequest,
    ) -> Result<ModuleResponse, Error> {
        self.routable(module)?;
        self.calls.handle_route(module, actor, request).await
    }

    async fn handle_tool(
        &self,
        module: &ModuleName,
        actor: &User,
        tool: &str,
        args: &str,
    ) -> Result<String, Error> {
        self.routable(module)?;
        let schema = self
            .registry
            .manifest(module)
            .and_then(|manifest| {
                manifest
                    .tools
                    .iter()
                    .find(|spec| spec.name == tool)
                    .map(|spec| spec.schema.clone())
            })
            .ok_or_else(|| Error::not_found(format!("tool {tool} of module {module}")))?;
        let arguments: serde_json::Value = serde_json::from_str(args)
            .map_err(|_error| Error::invalid("arguments", "the arguments are not JSON"))?;
        check_schema("arguments", "the tool's schema", &schema, &arguments)?;
        self.calls
            .handle_tool(module, Some(actor), tool, args)
            .await
    }
}

#[cfg(test)]
mod tests;
