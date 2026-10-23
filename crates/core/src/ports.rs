//! Driven ports: the traits through which services reach the outside world.
//!
//! Every method returns `impl Future<Output = Result<_, Error>> + Send` (or
//! is synchronous when it is pure CPU work: [`Clock`], [`Rng`], [`Secrets`]),
//! so services are generic over the ports with static dispatch. Adapters live
//! in `liftoff-store`, `liftoff-host`, `liftoff-proxy` and `liftoff-wasm`;
//! hand-written fakes live in `crate::fakes` behind the `test-support`
//! feature.
//!
//! [`Ports`] bundles one implementation of each so a service needs a single
//! type parameter.

mod blobs;
mod clock;
mod docker;
mod files;
mod functions;
mod git;
mod hooks;
mod module_runtime;
mod module_source;
mod module_store;
mod proxy;
mod queue;
mod resolver;
mod store;
mod systemd;

pub use blobs::Blobs;
pub use clock::{Clock, Rng, Secrets};
pub use docker::{CommandOutput, Docker, OutputLine, OutputStream, ServiceStatus};
pub use files::{ProjectFile, ProjectFiles};
pub use functions::{ComponentFiles, ComponentsHold, ComponentsLock, FunctionHost};
pub use git::{DeviceGrant, DeviceStart, Git, GitAccess, NewRepo, PackageRef, RepoRole, Template};
pub use hooks::{
    Action, AuthResult, HookRequest, HookResult, Hooks, McpResource, ModulePage, ModuleRequest,
    ModuleResponse, NoHooks, OAuthIdentity, PageKind, PageRequest, Resource, WebPanel,
};
pub use module_runtime::ModuleRuntime;
pub use module_source::ModuleSource;
pub use module_store::ModuleStore;
pub use proxy::Proxy;
pub use queue::DeployQueue;
pub use resolver::Resolver;
pub use store::Store;
pub use systemd::Systemd;

/// One implementation of every port, as a single type parameter for
/// services (`DeployService<P: Ports>`).
///
/// `liftoffd` implements it over the real adapters; tests use
/// `FakePorts` (feature `test-support`).
pub trait Ports: Send + Sync + 'static {
    /// Every record.
    type Store: Store;
    /// `docker compose`.
    type Docker: Docker;
    /// Files of an environment's compose project.
    type Files: ProjectFiles;
    /// Slices.
    type Systemd: Systemd;
    /// The reverse proxy.
    type Proxy: Proxy;
    /// Git forges, as the user's own connection.
    type Git: Git;
    /// Public DNS.
    type Resolver: Resolver;
    /// Wall clock.
    type Clock: Clock;
    /// Identifiers and secrets.
    type Rng: Rng;
    /// Encryption with the master key.
    type Secrets: Secrets;
    /// The deploy queue.
    type Queue: DeployQueue;
    /// Extension points (modules).
    type Hooks: Hooks;
    /// The module registry.
    type Registry: ModuleSource;
    /// The module host as installation drives it.
    type Modules: ModuleRuntime;
    /// The runtime of functions as a deploy drives it.
    type Functions: FunctionHost;
    /// The kept `.wasm` files of function deploys.
    type Components: ComponentFiles;
    /// The files of artifact versions.
    type Blobs: Blobs;

    /// Every record.
    fn store(&self) -> &Self::Store;
    /// `docker compose`.
    fn docker(&self) -> &Self::Docker;
    /// Files of an environment's compose project.
    fn files(&self) -> &Self::Files;
    /// Slices.
    fn systemd(&self) -> &Self::Systemd;
    /// The reverse proxy.
    fn proxy(&self) -> &Self::Proxy;
    /// Git forges, as the user's own connection.
    fn git(&self) -> &Self::Git;
    /// Public DNS.
    fn resolver(&self) -> &Self::Resolver;
    /// Wall clock.
    fn clock(&self) -> &Self::Clock;
    /// Identifiers and secrets.
    fn rng(&self) -> &Self::Rng;
    /// Encryption with the master key.
    fn secrets(&self) -> &Self::Secrets;
    /// The deploy queue.
    fn queue(&self) -> &Self::Queue;
    /// Extension points.
    fn hooks(&self) -> &Self::Hooks;
    /// The module registry.
    fn registry(&self) -> &Self::Registry;
    /// The module host as installation drives it.
    fn modules(&self) -> &Self::Modules;
    /// The runtime of functions as a deploy drives it.
    fn functions(&self) -> &Self::Functions;
    /// The kept `.wasm` files of function deploys.
    fn components(&self) -> &Self::Components;
    /// The files of artifact versions.
    fn blobs(&self) -> &Self::Blobs;
}
