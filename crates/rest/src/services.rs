//! The use-case services the `/api/v1` routes call, over one [`Ports`].

use std::sync::Arc;

use liftoff_core::ports::Ports;
use liftoff_core::service::{
    Access, ArtifactsService, AuthService, BindingService, CreateService, DeployService,
    DomainService, EnvironmentService, GateListener, GitService, LinksService, McpAuthService,
    ModuleRouteService, ModuleService, ProjectService, Reconciler, SecretSync, SecretsService,
    Settings, TemplateCatalog, TunnelsService, VisibilityService, WebPanelService,
};

/// Every service of the API over the same ports and settings.
///
/// The ports themselves serve the one check a route makes without a
/// service (the proxy half of `/readyz`), the settings what `GET
/// /server` shows.
///
/// Cheap to clone: every service holds `Arc`s.
#[derive(Debug)]
pub struct Services<P> {
    pub(crate) ports: Arc<P>,
    pub(crate) settings: Arc<Settings>,
    pub(crate) auth: AuthService<P>,
    pub(crate) git: GitService<P>,
    pub(crate) templates: TemplateCatalog<P>,
    pub(crate) projects: ProjectService<P>,
    pub(crate) environments: EnvironmentService<P>,
    pub(crate) domains: DomainService<P>,
    pub(crate) bindings: BindingService<P>,
    pub(crate) deploys: DeployService<P>,
    pub(crate) secrets: SecretsService<P>,
    pub(crate) secret_sync: SecretSync<P>,
    pub(crate) create: CreateService<P>,
    pub(crate) reconciler: Reconciler<P>,
    pub(crate) module_routes: ModuleRouteService<P>,
    pub(crate) modules: ModuleService<P>,
    pub(crate) gate: GateListener<P>,
    pub(crate) links: LinksService<P>,
    pub(crate) visibility: VisibilityService<P>,
    pub(crate) tunnels: TunnelsService<P>,
    pub(crate) artifacts: ArtifactsService<P>,
    pub(crate) access: Access<P>,
    pub(crate) mcp_auth: McpAuthService<P>,
    pub(crate) web: WebPanelService<P>,
}

impl<P> Clone for Services<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
            auth: self.auth.clone(),
            git: self.git.clone(),
            templates: self.templates.clone(),
            projects: self.projects.clone(),
            environments: self.environments.clone(),
            domains: self.domains.clone(),
            bindings: self.bindings.clone(),
            deploys: self.deploys.clone(),
            secrets: self.secrets.clone(),
            secret_sync: self.secret_sync.clone(),
            create: self.create.clone(),
            reconciler: self.reconciler.clone(),
            module_routes: self.module_routes.clone(),
            modules: self.modules.clone(),
            gate: self.gate.clone(),
            links: self.links.clone(),
            visibility: self.visibility.clone(),
            tunnels: self.tunnels.clone(),
            artifacts: self.artifacts.clone(),
            access: self.access.clone(),
            mcp_auth: self.mcp_auth.clone(),
            web: self.web.clone(),
        }
    }
}

impl<P: Ports> Services<P> {
    /// Every service over `ports`, configured by `settings`.
    #[must_use]
    pub fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self {
            auth: AuthService::new(Arc::clone(&ports)),
            git: GitService::new(Arc::clone(&ports)),
            templates: TemplateCatalog::new(Arc::clone(&ports), Arc::clone(&settings)),
            projects: ProjectService::new(Arc::clone(&ports), Arc::clone(&settings)),
            environments: EnvironmentService::new(Arc::clone(&ports), Arc::clone(&settings)),
            domains: DomainService::new(Arc::clone(&ports), Arc::clone(&settings)),
            bindings: BindingService::new(Arc::clone(&ports), Arc::clone(&settings)),
            deploys: DeployService::new(Arc::clone(&ports), Arc::clone(&settings)),
            secrets: SecretsService::new(Arc::clone(&ports)),
            secret_sync: SecretSync::new(Arc::clone(&ports)),
            create: CreateService::new(Arc::clone(&ports), Arc::clone(&settings)),
            module_routes: ModuleRouteService::new(Arc::clone(&ports)),
            modules: ModuleService::new(Arc::clone(&ports), Arc::clone(&settings)),
            gate: GateListener::new(Arc::clone(&ports), Arc::clone(&settings)),
            links: LinksService::new(Arc::clone(&ports), Arc::clone(&settings)),
            visibility: VisibilityService::new(Arc::clone(&ports), Arc::clone(&settings)),
            tunnels: TunnelsService::new(Arc::clone(&ports), Arc::clone(&settings)),
            artifacts: ArtifactsService::new(Arc::clone(&ports), Arc::clone(&settings)),
            access: Access::new(Arc::clone(&ports), Arc::clone(&settings)),
            mcp_auth: McpAuthService::new(Arc::clone(&ports)),
            web: WebPanelService::new(Arc::clone(&ports)),
            reconciler: Reconciler::new(Arc::clone(&ports), Arc::clone(&settings)),
            settings,
            ports,
        }
    }
    /// These services with `tunnels` in place of a [`TunnelsService`] of
    /// their own: the daemon hands the one its tunnel server reports
    /// connections to, so `GET /api/v1/tunnel` and the connections' ends
    /// share what the service knows of connections.
    #[must_use]
    pub fn with_tunnels(mut self, tunnels: TunnelsService<P>) -> Self {
        self.tunnels = tunnels;
        self
    }
}
