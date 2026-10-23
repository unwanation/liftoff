//! [`FakeHooks`].

use core::future::ready;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use super::log::{Call, CallLog, HookCall, lock};
use crate::domain::{
    Deploy, Domain, Environment, Host, ModuleName, TenantId, TenantQuotas, TenantSlug, User, WebUrl,
};
use crate::error::Error;
use crate::ports::{
    Action, AuthResult, HookRequest, HookResult, Hooks, ModulePage, ModuleRequest, ModuleResponse,
    PageKind, PageRequest, Resource, WebPanel,
};

#[derive(Debug, Default)]
struct Script {
    answers: HashMap<&'static str, VecDeque<HookResult>>,
    actors: VecDeque<AuthResult>,
    requests: Vec<HookRequest>,
    pages: HashMap<PageKind, ModulePage>,
    available: HashSet<String>,
    routes: VecDeque<Result<ModuleResponse, Error>>,
    route_requests: Vec<(ModuleName, ModuleRequest)>,
    quotas: VecDeque<Result<Option<TenantQuotas>, Error>>,
    tenants: VecDeque<Result<Option<TenantId>, Error>>,
    tenant_requests: Vec<Option<TenantSlug>>,
    panel: Option<Result<Option<WebPanel>, Error>>,
    panel_calls: usize,
}

/// Hooks that log every call and answer [`HookResult::Continue`] unless a
/// test scripted otherwise.
///
/// - [`FakeHooks::answer_next`] queues the answer of the next call of a
///   hook by method name (`"before_deploy"`, `"domain_created"`, ...).
/// - [`FakeHooks::authenticate_next`] queues the next `authenticate`
///   answer (LOGN-05).
/// - [`FakeHooks::authenticate_requests`] are the requests `authenticate`
///   saw, whole (the call log keeps only their paths).
/// - [`FakeHooks::render`] replaces a page.
/// - [`FakeHooks::make_available`] makes `module_available` answer `Ok`
///   for a module (default: not installed).
/// - [`FakeHooks::answer_route`] queues the answer of the next
///   `handle_route` (default: the module is not installed);
///   [`FakeHooks::route_requests`] are the requests it saw, whole.
/// - [`FakeHooks::answer_quotas`] and [`FakeHooks::answer_project_tenant`]
///   queue the answers of the next `tenant_quotas` and `project_tenant`
///   (default: no opinion); [`FakeHooks::project_tenant_requests`] are
///   the slugs `project_tenant` saw. Neither is in the call log.
/// - [`FakeHooks::keep_panel`] sets what every `web_panel` answers from
///   now on (default: no opinion, no panel); [`FakeHooks::panel_calls`]
///   counts them. Not in the call log.
#[derive(Debug, Clone, Default)]
pub struct FakeHooks {
    script: Arc<Mutex<Script>>,
    log: CallLog,
}

impl FakeHooks {
    /// A fake with its own call log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake writing to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            ..Self::default()
        }
    }

    /// The next call of `hook` answers `answer`.
    pub fn answer_next(&self, hook: &'static str, answer: HookResult) {
        lock(&self.script)
            .answers
            .entry(hook)
            .or_default()
            .push_back(answer);
    }

    /// The next `authenticate` answers `answer`.
    pub fn authenticate_next(&self, answer: AuthResult) {
        lock(&self.script).actors.push_back(answer);
    }

    /// The requests `authenticate` received so far, oldest first.
    #[must_use]
    pub fn authenticate_requests(&self) -> Vec<HookRequest> {
        lock(&self.script).requests.clone()
    }

    /// `render_page(kind)` answers `html`.
    pub fn render(&self, kind: PageKind, html: &str) {
        let _previous = lock(&self.script)
            .pages
            .insert(kind, ModulePage::Html(html.to_owned()));
    }

    /// `render_page(kind)` sends the browser to `url` (the panel takes the page over).
    pub fn redirect(&self, kind: PageKind, url: WebUrl) {
        let _previous = lock(&self.script)
            .pages
            .insert(kind, ModulePage::Redirect(url));
    }

    /// `module_available(module)` answers `Ok` from now on.
    pub fn make_available(&self, module: &str) {
        let _new = lock(&self.script).available.insert(module.to_owned());
    }

    /// The next `handle_route` answers `answer`, whatever the module.
    pub fn answer_route(&self, answer: Result<ModuleResponse, Error>) {
        lock(&self.script).routes.push_back(answer);
    }

    /// The next `tenant_quotas` answers `answer`.
    pub fn answer_quotas(&self, answer: Result<Option<TenantQuotas>, Error>) {
        lock(&self.script).quotas.push_back(answer);
    }

    /// The next `project_tenant` answers `answer`.
    pub fn answer_project_tenant(&self, answer: Result<Option<TenantId>, Error>) {
        lock(&self.script).tenants.push_back(answer);
    }

    /// Every `web_panel` answers `answer` from now on.
    pub fn keep_panel(&self, answer: Result<Option<WebPanel>, Error>) {
        lock(&self.script).panel = Some(answer);
    }

    /// How many times `web_panel` was asked so far.
    #[must_use]
    pub fn panel_calls(&self) -> usize {
        lock(&self.script).panel_calls
    }

    /// The slug of every `project_tenant` so far, oldest first.
    #[must_use]
    pub fn project_tenant_requests(&self) -> Vec<Option<TenantSlug>> {
        lock(&self.script).tenant_requests.clone()
    }

    /// The module and request of every `handle_route` so far, oldest
    /// first.
    #[must_use]
    pub fn route_requests(&self) -> Vec<(ModuleName, ModuleRequest)> {
        lock(&self.script).route_requests.clone()
    }

    /// The hook calls received so far, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<HookCall> {
        self.log
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Hook(call) => Some(call),
                _ => None,
            })
            .collect()
    }

    fn answer(&self, hook: &'static str, call: HookCall) -> HookResult {
        self.log.push(Call::Hook(call));
        lock(&self.script)
            .answers
            .get_mut(hook)
            .and_then(VecDeque::pop_front)
            .unwrap_or(HookResult::Continue)
    }
}

impl Hooks for FakeHooks {
    fn authenticate(&self, request: &HookRequest) -> impl Future<Output = AuthResult> + Send {
        self.log
            .push(Call::Hook(HookCall::Authenticate(request.path.clone())));
        let mut script = lock(&self.script);
        script.requests.push(request.clone());
        ready(script.actors.pop_front().unwrap_or(AuthResult::Continue))
    }

    fn authorize(
        &self,
        actor: &User,
        action: Action,
        resource: &Resource,
    ) -> impl Future<Output = HookResult> + Send {
        ready(self.answer(
            "authorize",
            HookCall::Authorize {
                actor: actor.id,
                action,
                resource: resource.clone(),
            },
        ))
    }

    fn before_deploy(
        &self,
        environment: &Environment,
        deploy: &Deploy,
    ) -> impl Future<Output = HookResult> + Send {
        ready(self.answer(
            "before_deploy",
            HookCall::BeforeDeploy {
                environment: environment.id,
                deploy: deploy.id,
            },
        ))
    }

    fn after_deploy(
        &self,
        environment: &Environment,
        deploy: &Deploy,
    ) -> impl Future<Output = HookResult> + Send {
        ready(self.answer(
            "after_deploy",
            HookCall::AfterDeploy {
                environment: environment.id,
                deploy: deploy.id,
            },
        ))
    }

    fn domain_created(&self, domain: &Domain) -> impl Future<Output = HookResult> + Send {
        ready(self.answer(
            "domain_created",
            HookCall::DomainCreated {
                domain: domain.id,
                host: domain.host.clone(),
            },
        ))
    }

    fn domain_removed(&self, domain: &Domain) -> impl Future<Output = HookResult> + Send {
        ready(self.answer(
            "domain_removed",
            HookCall::DomainRemoved {
                domain: domain.id,
                host: domain.host.clone(),
            },
        ))
    }

    fn gate(
        &self,
        host: &Host,
        _actor: &User,
        request: &HookRequest,
    ) -> impl Future<Output = HookResult> + Send {
        ready(self.answer(
            "gate",
            HookCall::Gate {
                host: host.clone(),
                path: request.path.clone(),
            },
        ))
    }

    fn tenant_quotas(
        &self,
        _tenant: TenantId,
    ) -> impl Future<Output = Result<Option<TenantQuotas>, Error>> + Send {
        ready(lock(&self.script).quotas.pop_front().unwrap_or(Ok(None)))
    }

    fn project_tenant(
        &self,
        _actor: &User,
        tenant: Option<&TenantSlug>,
    ) -> impl Future<Output = Result<Option<TenantId>, Error>> + Send {
        let mut script = lock(&self.script);
        script.tenant_requests.push(tenant.cloned());
        ready(script.tenants.pop_front().unwrap_or(Ok(None)))
    }

    fn web_panel(&self) -> impl Future<Output = Result<Option<WebPanel>, Error>> + Send {
        let mut script = lock(&self.script);
        script.panel_calls = script.panel_calls.saturating_add(1);
        ready(script.panel.clone().unwrap_or(Ok(None)))
    }

    fn render_page(&self, page: &PageRequest) -> impl Future<Output = Option<ModulePage>> + Send {
        self.log.push(Call::Hook(HookCall::RenderPage(page.kind)));
        ready(lock(&self.script).pages.get(&page.kind).cloned())
    }

    fn module_available(
        &self,
        module: &ModuleName,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(if lock(&self.script).available.contains(module.as_str()) {
            Ok(())
        } else {
            Err(Error::not_found(format!("module {module}")))
        })
    }

    fn handle_route(
        &self,
        module: &ModuleName,
        _actor: Option<&User>,
        request: ModuleRequest,
    ) -> impl Future<Output = Result<ModuleResponse, Error>> + Send {
        let mut script = lock(&self.script);
        script.route_requests.push((module.clone(), request));
        ready(
            script
                .routes
                .pop_front()
                .unwrap_or_else(|| Err(Error::not_found(format!("module {module}")))),
        )
    }
}
