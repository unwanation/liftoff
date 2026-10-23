//! [`ModuleRegistry`]: the modules the daemon runs, what they declared
//! and whether they take calls.
//!
//! The module host registers each module it loads with its manifest and
//! reports the outcome of every call into it ([`ModuleRegistry::record`]).
//! The registry owns the rule of MOD-09: [`FAILURES_TO_DISABLE`] module
//! failures in a row (traps, timeouts, a fresh instance that does not
//! instantiate) disable the module with the last error; an answered call
//! starts the count again, and a failure of the daemon under the call
//! (its store) counts neither way. A disabled module's routes answer
//! [`Error::ModuleRequired`] (424) and its hooks, tools and commands are
//! left out until [`ModuleRegistry::enable`].
//!
//! The registry lives in memory; the `modules` table that keeps
//! `enabled` and `error` across restarts arrives with installation, which writes what [`ModuleRegistry::list`] shows.

use std::sync::{Mutex, MutexGuard, PoisonError};

use super::manifest::{CommandSpec, HookName, ModuleManifest, RouteSpec, ToolSpec};
use crate::domain::ModuleName;
use crate::error::Error;

/// How many module failures in a row disable a module (MOD-09).
pub const FAILURES_TO_DISABLE: u32 = 3;

/// Whether a module takes calls (`modules.enabled`, `modules.error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleStatus {
    /// `false` once it failed [`FAILURES_TO_DISABLE`] calls in a row
    /// (MOD-09) or was disabled.
    pub enabled: bool,
    /// Why it was disabled.
    pub error: Option<String>,
}

impl ModuleStatus {
    /// An enabled module.
    const ENABLED: Self = Self {
        enabled: true,
        error: None,
    };
}

/// How a call into a module ended, as the host reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome {
    /// The module answered, with its result or its own error.
    Answered,
    /// The module failed: it trapped, ran past its call timeout, or a
    /// fresh instance of it did not instantiate. The error says how.
    Failed(String),
    /// The daemon failed under the call (its store): not the module's
    /// fault, so it neither counts nor breaks a run of failures.
    HostFailed,
}

/// A registered module.
#[derive(Debug, Clone)]
struct Entry {
    name: ModuleName,
    manifest: ModuleManifest,
    status: ModuleStatus,
    failures_in_a_row: u32,
}

/// The modules the daemon runs, in installation order (the order hooks run in).
#[derive(Debug, Default)]
pub struct ModuleRegistry {
    modules: Mutex<Vec<Entry>>,
}

impl ModuleRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The entries, whatever a panic left in them.
    fn entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.modules.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers the module `name` (the name it is installed and loaded
    /// as) with its `manifest`, enabled. A module registered again
    /// (reloaded) keeps its place and starts enabled with its new
    /// manifest.
    pub fn register(&self, name: ModuleName, manifest: ModuleManifest) {
        let entry = Entry {
            name,
            manifest,
            status: ModuleStatus::ENABLED,
            failures_in_a_row: 0,
        };
        let mut entries = self.entries();
        match entries.iter_mut().find(|known| known.name == entry.name) {
            Some(known) => *known = entry,
            None => entries.push(entry),
        }
    }

    /// Forgets `module`; whether it was registered.
    pub fn remove(&self, module: &ModuleName) -> bool {
        let mut entries = self.entries();
        let before = entries.len();
        entries.retain(|entry| &entry.name != module);
        entries.len() != before
    }

    /// `change` applied to the entry of `module`, if it is registered.
    fn change<R>(&self, module: &ModuleName, change: impl FnOnce(&mut Entry) -> R) -> Option<R> {
        self.entries()
            .iter_mut()
            .find(|entry| &entry.name == module)
            .map(change)
    }

    /// Counts the `outcome` of a call into `module` (MOD-09) and returns
    /// its status after it; `None` for a module that is not registered.
    pub fn record(&self, module: &ModuleName, outcome: CallOutcome) -> Option<ModuleStatus> {
        self.change(module, |entry| {
            match outcome {
                CallOutcome::Answered => entry.failures_in_a_row = 0,
                CallOutcome::HostFailed => {}
                CallOutcome::Failed(error) => {
                    entry.failures_in_a_row = entry.failures_in_a_row.saturating_add(1);
                    if entry.failures_in_a_row >= FAILURES_TO_DISABLE && entry.status.enabled {
                        tracing::error!(%module, %error, failures = entry.failures_in_a_row, "a module failed too many calls in a row and is disabled");
                        entry.status = ModuleStatus {
                            enabled: false,
                            error: Some(error),
                        };
                    }
                }
            }
            entry.status.clone()
        })
    }

    /// Disables `module` with `error` (a module that failed to load at start); whether it was registered.
    pub fn disable(&self, module: &ModuleName, error: impl Into<String>) -> bool {
        self.change(module, |entry| {
            entry.status = ModuleStatus {
                enabled: false,
                error: Some(error.into()),
            };
        })
        .is_some()
    }

    /// Enables `module` again (`lof module enable` after a fix): its
    /// error is cleared and the count of failures starts again.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a module that is not registered.
    pub fn enable(&self, module: &ModuleName) -> Result<(), Error> {
        self.change(module, |entry| {
            entry.status = ModuleStatus::ENABLED;
            entry.failures_in_a_row = 0;
        })
        .ok_or_else(|| not_registered(module))
    }

    /// The status of `module`; `None` for a module that is not
    /// registered.
    #[must_use]
    pub fn status(&self, module: &ModuleName) -> Option<ModuleStatus> {
        self.entries()
            .iter()
            .find(|entry| &entry.name == module)
            .map(|entry| entry.status.clone())
    }

    /// The manifest `module` declared.
    #[must_use]
    pub fn manifest(&self, module: &ModuleName) -> Option<ModuleManifest> {
        self.entries()
            .iter()
            .find(|entry| &entry.name == module)
            .map(|entry| entry.manifest.clone())
    }

    /// Every module with its manifest and status, in installation order
    /// (`lof module ls`, `GET /modules`).
    #[must_use]
    pub fn list(&self) -> Vec<(ModuleName, ModuleManifest, ModuleStatus)> {
        self.entries()
            .iter()
            .map(|entry| {
                (
                    entry.name.clone(),
                    entry.manifest.clone(),
                    entry.status.clone(),
                )
            })
            .collect()
    }

    /// The route `method path` of `module` (below `/api/v1/m/<module>`),
    /// as [`RouteSpec::matches`] matches it: the first declared route
    /// that does.
    ///
    /// # Errors
    ///
    /// [`Error::ModuleRequired`] (424) when the module is disabled
    /// (MOD-09); [`Error::NotFound`] for a module that is not registered
    /// or a route it does not declare.
    pub fn route(&self, module: &ModuleName, method: &str, path: &str) -> Result<RouteSpec, Error> {
        let entry = self
            .change(module, |entry| entry.clone())
            .ok_or_else(|| not_registered(module))?;
        if !entry.status.enabled {
            return Err(Error::module_required(module.as_str()));
        }
        entry
            .manifest
            .routes
            .into_iter()
            .find(|route| route.matches(method, path))
            .ok_or_else(|| Error::not_found(format!("route {method} {path} of module {module}")))
    }

    /// The enabled modules subscribed to `hook`, in installation order.
    #[must_use]
    pub fn subscribers(&self, hook: HookName) -> Vec<ModuleName> {
        self.entries()
            .iter()
            .filter(|entry| entry.status.enabled && entry.manifest.hooks.contains(&hook))
            .map(|entry| entry.name.clone())
            .collect()
    }

    /// The MCP tools of the enabled modules, each with its module.
    #[must_use]
    pub fn tools(&self) -> Vec<(ModuleName, ToolSpec)> {
        self.enabled(|manifest| manifest.tools.clone())
    }

    /// The CLI commands of the enabled modules, each with its module.
    #[must_use]
    pub fn commands(&self) -> Vec<(ModuleName, CommandSpec)> {
        self.enabled(|manifest| manifest.commands.clone())
    }

    /// What `declared` takes from each enabled module's manifest, with
    /// the module.
    fn enabled<T>(&self, declared: impl Fn(&ModuleManifest) -> Vec<T>) -> Vec<(ModuleName, T)> {
        self.entries()
            .iter()
            .filter(|entry| entry.status.enabled)
            .flat_map(|entry| {
                declared(&entry.manifest)
                    .into_iter()
                    .map(|spec| (entry.name.clone(), spec))
            })
            .collect()
    }
}

/// The refusal of a module that is not registered.
fn not_registered(module: &ModuleName) -> Error {
    Error::not_found(format!("module {module}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(raw: &str) -> ModuleName {
        ModuleName::new(raw).unwrap()
    }

    /// The manifest of `module` with a route `GET /hello`, a tool, a
    /// command and the hook `authorize`.
    fn manifest(module: &str) -> ModuleManifest {
        ModuleManifest::parse(&format!(
            r#"{{"name":"{module}","version":"0.1.0",
                "routes":[{{"method":"GET","path":"/hello"}}],
                "tools":[{{"name":"look","description":"Looks","schema":{{}}}}],
                "commands":[{{"name":"greet","description":"Greets"}}],
                "hooks":["authorize"]}}"#
        ))
        .unwrap()
    }

    /// A registry with `modules` registered in order.
    fn registry(modules: &[&str]) -> ModuleRegistry {
        let registry = ModuleRegistry::new();
        for module in modules {
            registry.register(name(module), manifest(module));
        }
        registry
    }

    /// `times` failures of `module`.
    fn fail(registry: &ModuleRegistry, module: &str, times: usize) {
        for attempt in 0..times {
            let _status = registry.record(
                &name(module),
                CallOutcome::Failed(format!("module {module} trapped: {attempt}")),
            );
        }
    }

    mod register {
        use super::*;

        // TC: MOD-09 (a loaded module starts enabled, in installation order)
        #[test]
        fn lists_modules_enabled_in_install_order_when_registered() {
            let registry = registry(&["oidc", "tenants"]);
            assert_eq!(
                registry
                    .list()
                    .into_iter()
                    .map(|(module, _, status)| (module.to_string(), status))
                    .collect::<Vec<_>>(),
                vec![
                    ("oidc".to_owned(), ModuleStatus::ENABLED),
                    ("tenants".to_owned(), ModuleStatus::ENABLED)
                ]
            );
        }

        // TC: MOD-09 (a reload keeps the place and starts enabled)
        #[test]
        fn enables_module_in_its_place_when_registered_again() {
            let registry = registry(&["oidc", "tenants"]);
            fail(&registry, "oidc", 3);
            registry.register(name("oidc"), manifest("oidc"));
            assert_eq!(
                registry
                    .list()
                    .into_iter()
                    .map(|(module, _, status)| (module.to_string(), status.enabled))
                    .collect::<Vec<_>>(),
                vec![("oidc".to_owned(), true), ("tenants".to_owned(), true)]
            );
        }

        // TC: MOD-09
        #[test]
        fn forgets_module_when_removed() {
            let registry = registry(&["oidc"]);
            assert_eq!(
                (
                    registry.remove(&name("oidc")),
                    registry.status(&name("oidc"))
                ),
                (true, None)
            );
        }
    }

    mod record {
        use super::*;

        // TC: MOD-09
        #[test]
        fn disables_module_with_last_error_when_three_calls_in_a_row_fail() {
            let registry = registry(&["trap"]);
            fail(&registry, "trap", 3);
            assert_eq!(
                registry.status(&name("trap")),
                Some(ModuleStatus {
                    enabled: false,
                    error: Some("module trap trapped: 2".to_owned())
                })
            );
        }

        // TC: MOD-09
        #[test]
        fn keeps_module_enabled_when_two_calls_fail() {
            let registry = registry(&["trap"]);
            fail(&registry, "trap", 2);
            assert_eq!(registry.status(&name("trap")), Some(ModuleStatus::ENABLED));
        }

        // TC: MOD-09 (in a row: an answer starts the count again)
        #[test]
        fn keeps_module_enabled_when_answer_breaks_failure_run() {
            let registry = registry(&["trap"]);
            fail(&registry, "trap", 2);
            let _status = registry.record(&name("trap"), CallOutcome::Answered);
            fail(&registry, "trap", 2);
            assert_eq!(registry.status(&name("trap")), Some(ModuleStatus::ENABLED));
        }

        // TC: MOD-09 (the daemon's own failures are not the module's)
        #[test]
        fn keeps_module_enabled_when_host_fails_three_times() {
            let registry = registry(&["trap"]);
            for _ in 0..3 {
                let _status = registry.record(&name("trap"), CallOutcome::HostFailed);
            }
            assert_eq!(registry.status(&name("trap")), Some(ModuleStatus::ENABLED));
        }

        // TC: MOD-09 (a host failure does not break a run either)
        #[test]
        fn disables_module_when_host_failure_falls_inside_failure_run() {
            let registry = registry(&["trap"]);
            fail(&registry, "trap", 2);
            let _status = registry.record(&name("trap"), CallOutcome::HostFailed);
            fail(&registry, "trap", 1);
            assert_eq!(
                registry.status(&name("trap")).map(|status| status.enabled),
                Some(false)
            );
        }

        // TC: MOD-09
        #[test]
        fn answers_none_when_module_is_not_registered() {
            let registry = registry(&[]);
            assert_eq!(registry.record(&name("trap"), CallOutcome::Answered), None);
        }
    }

    mod disable {
        use super::*;

        // TC: MOD-09 (a module that fails to load is disabled)
        #[test]
        fn disables_module_with_error_when_disabled() {
            let registry = registry(&["oidc"]);
            let disabled = registry.disable(&name("oidc"), "does not load");
            assert_eq!(
                (disabled, registry.status(&name("oidc"))),
                (
                    true,
                    Some(ModuleStatus {
                        enabled: false,
                        error: Some("does not load".to_owned())
                    })
                )
            );
        }

        // TC: MOD-09 (`lof module enable` after a fix)
        #[test]
        fn enables_module_and_restarts_count_when_enabled() {
            let registry = registry(&["trap"]);
            fail(&registry, "trap", 3);
            registry.enable(&name("trap")).unwrap();
            fail(&registry, "trap", 2);
            assert_eq!(registry.status(&name("trap")), Some(ModuleStatus::ENABLED));
        }

        // TC: MOD-09
        #[test]
        fn refuses_enable_when_module_is_not_registered() {
            let refused = registry(&[]).enable(&name("trap"));
            assert!(matches!(refused, Err(Error::NotFound(_))), "{refused:?}");
        }
    }

    mod route {
        use super::*;

        // TC: MOD-09 (REST of a disabled module → 424)
        #[test]
        fn answers_module_required_when_module_is_disabled() {
            let registry = registry(&["trap"]);
            fail(&registry, "trap", 3);
            let refused = registry.route(&name("trap"), "GET", "/hello");
            assert!(
                matches!(&refused, Err(Error::ModuleRequired(module)) if module == "trap"),
                "{refused:?}"
            );
        }

        // TC: MOD-09
        #[test]
        fn finds_route_when_enabled_module_declares_it() {
            let found = registry(&["trap"]).route(&name("trap"), "GET", "/hello");
            assert_eq!(
                found.map(|route| route.path).ok(),
                Some("/hello".to_owned())
            );
        }

        // TC: MOD-09
        #[test]
        fn answers_not_found_when_route_is_not_declared() {
            let refused = registry(&["trap"]).route(&name("trap"), "POST", "/hello");
            assert!(matches!(refused, Err(Error::NotFound(_))), "{refused:?}");
        }

        // TC: MOD-09
        #[test]
        fn answers_not_found_when_module_is_not_registered() {
            let refused = registry(&[]).route(&name("trap"), "GET", "/hello");
            assert!(matches!(refused, Err(Error::NotFound(_))), "{refused:?}");
        }
    }

    mod declared {
        use super::*;

        // TC: MOD-09 (a disabled module's hooks do not run)
        #[test]
        fn lists_enabled_subscribers_in_install_order_when_hook_asked() {
            let registry = registry(&["tenants", "trap", "oidc"]);
            fail(&registry, "trap", 3);
            assert_eq!(
                registry.subscribers(HookName::Authorize),
                vec![name("tenants"), name("oidc")]
            );
        }

        // TC: MOD-09
        #[test]
        fn lists_tools_and_commands_of_enabled_modules_only_when_asked() {
            let registry = registry(&["oidc", "trap"]);
            fail(&registry, "trap", 3);
            assert_eq!(
                (
                    registry
                        .tools()
                        .into_iter()
                        .map(|(module, tool)| (module.to_string(), tool.name))
                        .collect::<Vec<_>>(),
                    registry
                        .commands()
                        .into_iter()
                        .map(|(module, command)| (module.to_string(), command.name))
                        .collect::<Vec<_>>(),
                ),
                (
                    vec![("oidc".to_owned(), "look".to_owned())],
                    vec![("oidc".to_owned(), "greet".to_owned())]
                )
            );
        }
    }
}
