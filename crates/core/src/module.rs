//! What the core knows of modules.
//!
//! The [`ModuleManifest`] a module declares, the [`ModuleRegistry`] of
//! the loaded modules and their status (MOD-09), the hook dispatcher
//! [`ModuleHooks`] that answers the core's hooks and module routes through
//! a host's [`ModuleCalls`], the [`Git`](crate::ports::Git)
//! port served by the forge modules ([`ModuleGit`]), and the
//! `core-call` bridge ([`CoreCalls`], [`CoreBridge`]) through which a
//! module calls a core service as the actor of its current call.
//!
//! The registry's `index.json`, its [`Release`]s and the [`RegistryKey`]
//! that signs them live here too; installing is
//! [`ModuleService`](crate::service::ModuleService).
//!
//! The wasmtime host (`liftoff-wasm`) parses `manifest()` with
//! [`ModuleManifest::parse`] and answers `core-call` through a
//! [`CoreCalls`]; the core stays free of wasmtime.

mod core_call;
mod git;
mod hooks;
mod manifest;
mod registry;
mod registry_remote;

pub use core_call::{Caller, CoreBridge, CoreCalls, NoCoreCalls};
pub use git::ModuleGit;
pub use hooks::{HookEvent, ModuleCalls, ModuleHooks, failed_to_answer};
pub use manifest::{
    ArgKind, ArgSpec, CommandSpec, GitProviderSpec, HookName, HttpAllow, ModuleManifest, RouteSpec,
    ToolSpec,
};
pub use registry::{CallOutcome, FAILURES_TO_DISABLE, ModuleRegistry, ModuleStatus};
pub(crate) use registry_remote::unpinned;
pub use registry_remote::{
    AvailableModule, REGISTRY_PUBLIC_KEY, RegistryKey, Release, WIT_VERSION, parse_index,
    sha256_hex,
};
