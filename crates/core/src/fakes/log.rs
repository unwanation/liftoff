//! The call log every recording fake writes to, and scripted failures.

use core::time::Duration;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::domain::{
    ArtifactId, ArtifactPath, BlobHash, ComponentHash, ComposeProject, DeployId, DeviceCode,
    DomainId, EnvironmentId, Host, Limits, Login, RepoRef, SecretKey, ServiceName, SliceName,
    UserId, VersionNumber,
};
use crate::error::Error;
use crate::ports::{Action, PackageRef, PageKind, ProjectFile, Resource};
use crate::proxy_config::ProxyConfig;

/// Locks `mutex`, ignoring poisoning (a panicking test already failed).
pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One call a fake received, in the order it arrived.
///
/// Failed calls are logged too: the log records attempts, so a test can
/// assert that a step was tried and what happened next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// A mutating store method, by name (e.g. `"insert_domain"`). Reads
    /// are not logged.
    Store(&'static str),
    /// A [`Docker`](crate::ports::Docker) call.
    Docker(DockerCall),
    /// A mutating [`ProjectFiles`](crate::ports::ProjectFiles) call.
    Files(FilesCall),
    /// A [`Systemd`](crate::ports::Systemd) call.
    Systemd(SystemdCall),
    /// A deploy handed to the [`DeployQueue`](crate::ports::DeployQueue).
    Enqueue(DeployId),
    /// A configuration pushed through [`Proxy::apply`](crate::ports::Proxy::apply).
    Proxy(ProxyConfig),
    /// A [`Git`](crate::ports::Git) call.
    Git(GitCall),
    /// A [`Resolver`](crate::ports::Resolver) query.
    Resolve(ResolveCall),
    /// A [`Hooks`](crate::ports::Hooks) call.
    Hook(HookCall),
    /// A [`ModuleSource`](crate::ports::ModuleSource) call.
    Registry(RegistryCall),
    /// A [`ModuleRuntime`](crate::ports::ModuleRuntime) call (reads and
    /// `read` of a file are not logged).
    Runtime(RuntimeCall),
    /// A [`FunctionHost`](crate::ports::FunctionHost) call, or a mutating
    /// [`ComponentFiles`](crate::ports::ComponentFiles) one.
    Function(FunctionCall),
    /// A mutating [`Blobs`](crate::ports::Blobs) call (`read` is not logged).
    Blob(BlobCall),
}

/// A mutating [`Blobs`](crate::ports::Blobs) call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobCall {
    /// `put` of a blob.
    Put(BlobHash),
    /// `release` of a blob put.
    Release(BlobHash),
    /// `link` of a version's file to a blob.
    Link {
        /// The artifact.
        artifact: ArtifactId,
        /// Its version.
        version: VersionNumber,
        /// The file.
        name: ArtifactPath,
        /// The blob.
        hash: BlobHash,
    },
    /// `remove_version`.
    RemoveVersion {
        /// The artifact.
        artifact: ArtifactId,
        /// Its version.
        version: VersionNumber,
    },
    /// `remove_artifact`.
    RemoveArtifact(ArtifactId),
}

/// A [`FunctionHost`](crate::ports::FunctionHost) call, by component, or a
/// mutating [`ComponentFiles`](crate::ports::ComponentFiles) one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionCall {
    /// `check`.
    Check(ComponentHash),
    /// `precompile`.
    Precompile(ComponentHash),
    /// `activate`.
    Activate {
        /// The environment.
        environment: EnvironmentId,
        /// The component.
        component: ComponentHash,
    },
    /// `save` of a file.
    Save(ComponentHash),
    /// `remove` of a file.
    Remove(ComponentHash),
    /// `logs` of a function.
    Logs {
        /// The environment.
        environment: EnvironmentId,
        /// The last lines only.
        tail: Option<u32>,
        /// Keep streaming.
        follow: bool,
    },
}

/// A [`ModuleSource`](crate::ports::ModuleSource) call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryCall {
    /// `index`.
    Index,
    /// `download`, with the URL.
    Download(String),
}

/// A [`ModuleRuntime`](crate::ports::ModuleRuntime) call, by module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCall {
    /// `inspect`.
    Inspect(String),
    /// `save` of the file of a version.
    Save {
        /// The module.
        module: String,
        /// Its version.
        version: String,
    },
    /// `discard` of the file of a version.
    Discard {
        /// The module.
        module: String,
        /// Its version.
        version: String,
    },
    /// `load`, with the migration it ran from.
    Load {
        /// The module.
        module: String,
        /// `migrate(from)`, if it ran.
        migrate_from: Option<u32>,
    },
    /// `unload`.
    Unload(String),
    /// `purge` of its tables and keys.
    Purge(String),
}

/// A [`Git`](crate::ports::Git) call, by the account it acted as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitCall {
    /// `provider_of`.
    ProviderOf {
        /// The forge host asked.
        host: Host,
    },
    /// `account` (the token is not logged).
    Account {
        /// The forge host asked.
        host: Host,
    },
    /// `repo_role`.
    RepoRole {
        /// The connection's account.
        login: Login,
        /// Repository.
        repo: RepoRef,
    },
    /// `image_exists`.
    ImageExists {
        /// The connection's account.
        login: Login,
        /// The image.
        image: PackageRef,
    },
    /// `put_ci_secret` (the value is not logged).
    PutCiSecret {
        /// The connection's account.
        login: Login,
        /// Repository.
        repo: RepoRef,
        /// Secret name.
        key: SecretKey,
    },
    /// `list_templates`.
    ListTemplates {
        /// The connection's account.
        login: Login,
        /// The repositories asked about.
        repos: Vec<RepoRef>,
    },
    /// `generate_from_template`.
    GenerateFromTemplate {
        /// The connection's account.
        login: Login,
        /// The template.
        template: RepoRef,
        /// `owner/name` of the repository asked for.
        target: String,
    },
    /// `device_start`.
    DeviceStart {
        /// The forge host asked.
        host: Host,
    },
    /// `device_poll`.
    DevicePoll {
        /// The forge host asked.
        host: Host,
        /// The device code polled.
        device_code: DeviceCode,
    },
}

/// A [`Resolver`](crate::ports::Resolver) query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveCall {
    /// `cname`.
    Cname(Host),
    /// `a`.
    A(Host),
}

/// A [`Hooks`](crate::ports::Hooks) call, by what it was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookCall {
    /// `authenticate`, with the request path.
    Authenticate(String),
    /// `authorize`.
    Authorize {
        /// The actor.
        actor: UserId,
        /// The action.
        action: Action,
        /// The resource.
        resource: Resource,
    },
    /// `before_deploy`.
    BeforeDeploy {
        /// The environment.
        environment: EnvironmentId,
        /// The deploy.
        deploy: DeployId,
    },
    /// `after_deploy`.
    AfterDeploy {
        /// The environment.
        environment: EnvironmentId,
        /// The deploy.
        deploy: DeployId,
    },
    /// `domain_created`.
    DomainCreated {
        /// The domain.
        domain: DomainId,
        /// Its host.
        host: Host,
    },
    /// `domain_removed`.
    DomainRemoved {
        /// The domain.
        domain: DomainId,
        /// Its host.
        host: Host,
    },
    /// `gate`, with the request path.
    Gate {
        /// The application host.
        host: Host,
        /// The request path.
        path: String,
    },
    /// `render_page`.
    RenderPage(PageKind),
}

/// A [`Docker`](crate::ports::Docker) call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerCall {
    /// `docker login`.
    Login {
        /// Registry host.
        registry: Host,
        /// User.
        user: Login,
    },
    /// `compose pull`.
    Pull(ComposeProject),
    /// `compose up`.
    Up {
        /// Project.
        project: ComposeProject,
        /// `--wait-timeout`.
        wait_timeout: Duration,
    },
    /// `compose down`.
    Down {
        /// Project.
        project: ComposeProject,
        /// `-v`.
        volumes: bool,
    },
    /// `compose ps`.
    Ps(ComposeProject),
    /// `compose logs`.
    Logs {
        /// Project.
        project: ComposeProject,
        /// One service, or all.
        service: Option<ServiceName>,
        /// `--tail`.
        tail: Option<u32>,
        /// `-f`.
        follow: bool,
    },
}

/// A mutating [`ProjectFiles`](crate::ports::ProjectFiles) call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesCall {
    /// A file was written.
    Write {
        /// Project.
        project: ComposeProject,
        /// Which file.
        file: ProjectFile,
        /// The content.
        content: String,
    },
    /// The project directory was removed.
    RemoveAll(ComposeProject),
}

/// A [`Systemd`](crate::ports::Systemd) call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemdCall {
    /// `set-property`.
    SetSliceLimits {
        /// Slice.
        slice: SliceName,
        /// Limits.
        limits: Limits,
    },
    /// `stop`.
    StopSlice(SliceName),
}

/// An ordered log of calls, shared by every fake built with
/// [`CallLog::clone`], so one test can assert cross-system order.
#[derive(Debug, Clone, Default)]
pub struct CallLog(Arc<Mutex<Vec<Call>>>);

impl CallLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a call.
    pub fn push(&self, call: Call) {
        lock(&self.0).push(call);
    }

    /// Every call so far, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        lock(&self.0).clone()
    }

    /// Forgets every call (e.g. after arranging a test).
    pub fn clear(&self) {
        lock(&self.0).clear();
    }

    /// The calls that touch something outside the store, oldest first:
    /// neither [`Call::Store`] nor the `authorize` question every owner
    /// check asks first.
    #[must_use]
    pub fn external(&self) -> Vec<Call> {
        self.filter(|call| {
            !matches!(
                call,
                Call::Store(_) | Call::Hook(HookCall::Authorize { .. })
            )
        })
    }

    /// Whether the only call so far is one `authorize` question: a refused
    /// check that changed and sent nothing (SEC-10; reads are not logged).
    #[must_use]
    pub fn only_authorized(&self) -> bool {
        matches!(
            self.calls().as_slice(),
            [Call::Hook(HookCall::Authorize { .. })]
        )
    }

    /// The `authorize` questions so far, oldest first: who asked to do
    /// what on which resource (MOD-04).
    #[must_use]
    pub fn authorized(&self) -> Vec<(UserId, Action, Resource)> {
        lock(&self.0)
            .iter()
            .filter_map(|call| match call {
                Call::Hook(HookCall::Authorize {
                    actor,
                    action,
                    resource,
                }) => Some((*actor, *action, resource.clone())),
                _ => None,
            })
            .collect()
    }

    /// The calls matching `keep`, oldest first.
    #[must_use]
    pub fn filter(&self, keep: impl Fn(&Call) -> bool) -> Vec<Call> {
        lock(&self.0)
            .iter()
            .filter(|call| keep(call))
            .cloned()
            .collect()
    }
}

#[derive(Debug, Default)]
struct FaultPlan {
    next: VecDeque<Error>,
    always: Option<Error>,
}

/// Scripted failures by operation name (the port method, e.g.
/// `"compose_up"`, `"insert_domain"`, `"validate"`).
#[derive(Debug, Clone, Default)]
pub struct Faults(Arc<Mutex<HashMap<&'static str, FaultPlan>>>);

impl Faults {
    /// The next call of `op` fails with `error`; queued failures are used in
    /// order.
    pub fn fail_next(&self, op: &'static str, error: Error) {
        lock(&self.0).entry(op).or_default().next.push_back(error);
    }

    /// Every call of `op` fails with `error` until [`Faults::clear`].
    pub fn fail_always(&self, op: &'static str, error: Error) {
        lock(&self.0).entry(op).or_default().always = Some(error);
    }

    /// Removes every scripted failure of `op`.
    pub fn clear(&self, op: &'static str) {
        let _removed: Option<FaultPlan> = lock(&self.0).remove(op);
    }

    /// `Err` when a failure is scripted for `op`; consumes a `fail_next`.
    pub(super) fn check(&self, op: &'static str) -> Result<(), Error> {
        lock(&self.0).get_mut(op).map_or(Ok(()), |plan| {
            plan.next
                .pop_front()
                .or_else(|| plan.always.clone())
                .map_or(Ok(()), Err)
        })
    }
}

/// Adds `fail_next`, `fail_always` and `clear_failures` to a fake with a
/// `faults: Faults` field.
macro_rules! scriptable {
    ($name:ident) => {
        impl $name {
            /// The next call of `op` (a port method name) fails with `error`.
            pub fn fail_next(&self, op: &'static str, error: $crate::error::Error) {
                self.faults.fail_next(op, error);
            }

            /// Every call of `op` fails with `error` until cleared.
            pub fn fail_always(&self, op: &'static str, error: $crate::error::Error) {
                self.faults.fail_always(op, error);
            }

            /// Removes the scripted failures of `op`.
            pub fn clear_failures(&self, op: &'static str) {
                self.faults.clear(op);
            }
        }
    };
}
pub(super) use scriptable;

#[cfg(test)]
mod tests {
    use super::*;

    mod faults {
        use super::*;

        #[test]
        fn fails_once_when_fail_next() {
            let faults = Faults::default();
            faults.fail_next("op", Error::conflict("x"));
            assert!(faults.check("op").is_err(), "first call fails");
            assert!(faults.check("op").is_ok(), "second call passes");
        }

        #[test]
        fn fails_until_cleared_when_fail_always() {
            let faults = Faults::default();
            faults.fail_always("op", Error::conflict("x"));
            assert!(faults.check("op").is_err() && faults.check("op").is_err());
            faults.clear("op");
            assert!(faults.check("op").is_ok(), "cleared");
        }

        #[test]
        fn passes_when_another_op_scripted() {
            let faults = Faults::default();
            faults.fail_always("op", Error::conflict("x"));
            assert!(faults.check("other").is_ok(), "other op passes");
        }
    }

    mod call_log {
        use super::*;

        #[test]
        fn shares_one_log_when_cloned() {
            let log = CallLog::new();
            let other = log.clone();
            other.push(Call::Store("insert_user"));
            assert_eq!(log.calls(), vec![Call::Store("insert_user")]);
        }

        #[test]
        fn drops_store_calls_when_external() {
            let log = CallLog::new();
            log.push(Call::Store("insert_user"));
            log.push(Call::Enqueue(DeployId::new(ulid::Ulid::nil())));
            assert_eq!(
                log.external(),
                vec![Call::Enqueue(DeployId::new(ulid::Ulid::nil()))]
            );
        }
    }
}
