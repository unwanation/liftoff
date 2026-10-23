//! [`ModuleError`]: why a call into a module failed, and how wasmtime's
//! failures and the core see it.

use liftoff_core::domain::ModuleName;

use crate::bindings::liftoff::module::host;
use crate::imports::HostFault;

/// Why a call into a module failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModuleError {
    /// The component did not compile, instantiate or link against the
    /// world `liftoff:module/liftoff-module`.
    #[error("module {module} does not load: {reason}")]
    Load {
        /// The module.
        module: String,
        /// What went wrong.
        reason: String,
    },
    /// The module answered with an error of its own (the WIT
    /// `host.error`), e.g. a `store-query` it passed on.
    #[error("{code}: {message}")]
    Module {
        /// Its code (`forbidden`, `validation`, `not_found`, ...).
        code: String,
        /// Its message.
        message: String,
    },
    /// The module trapped. The full error is logged; this carries its
    /// short reason only.
    #[error("module {module} trapped: {reason}")]
    Trap {
        /// The module.
        module: String,
        /// The trap code (`wasm trap: ...`) or the host's own message
        /// (a cap, a failed store), without wasmtime's backtrace.
        reason: String,
    },
    /// The call ran longer than [`Limits::call_timeout`](crate::Limits::call_timeout) and was
    /// interrupted (MOD-08).
    #[error("module {module} timed out")]
    Timeout {
        /// The module.
        module: String,
    },
    /// The daemon failed under the call (the module store behind
    /// `store-get`/`store-set`): the call is lost, but the failure is not
    /// the module's and does not count toward disabling it (MOD-09).
    #[error("the host of module {module} failed: {reason}")]
    HostFailed {
        /// The module.
        module: String,
        /// What failed, without its source (logged).
        reason: String,
    },
    /// The module was called from inside its own call in progress (a
    /// `core-call` of it that reached it again); the call did not enter
    /// it, so it does not count toward disabling it.
    #[error("module {module} is called from inside its own call")]
    Reentered {
        /// The module.
        module: String,
    },
    /// The module is disabled: it failed three calls in a row (MOD-09) or
    /// is not registered; the call did not enter it.
    #[error("module {module} is disabled: {error}")]
    Disabled {
        /// The module.
        module: String,
        /// Why: the last failure.
        error: String,
    },
}

/// The core's view of a failed module call: a trap or a timeout is
/// [`Error::Internal`](liftoff_core::Error::Internal), a
/// disabled module is
/// [`Error::ModuleRequired`](liftoff_core::Error::ModuleRequired) (424,
/// MOD-09), the module's own error maps by its code (`upstream` is
/// [`Error::Upstream`](liftoff_core::Error::Upstream) of
/// [`System::Module`](liftoff_core::System::Module)).
impl From<ModuleError> for liftoff_core::Error {
    fn from(error: ModuleError) -> Self {
        match error {
            ModuleError::Module { code, message } => match code.as_str() {
                "validation" => Self::invalid("module", message),
                "unauthenticated" => Self::Unauthenticated(message),
                "forbidden" => Self::Forbidden(message),
                "not_found" => Self::NotFound(
                    message
                        .strip_suffix(" not found")
                        .map_or_else(|| message.clone(), ToOwned::to_owned),
                ),
                "conflict" => Self::Conflict(message),
                "gone" => Self::Gone(message),
                "unavailable" => Self::Unavailable(message),
                "module_required" => Self::ModuleRequired(message),
                // What the module talks to failed (a forge that keeps
                // answering 5xx, GITF-04): 502 with the module's text.
                "upstream" => Self::upstream(liftoff_core::System::Module, message),
                _ => {
                    tracing::error!(%code, %message, "a module answered an error outside the core's vocabulary");
                    Self::internal_msg(format!("module error {code}: {message}"))
                }
            },
            ModuleError::Disabled { module, .. } => Self::ModuleRequired(module),
            internal @ (ModuleError::Load { .. }
            | ModuleError::Trap { .. }
            | ModuleError::Timeout { .. }
            | ModuleError::Reentered { .. }
            | ModuleError::HostFailed { .. }) => Self::internal(internal),
        }
    }
}

impl From<host::Error> for ModuleError {
    fn from(error: host::Error) -> Self {
        Self::Module {
            code: error.code,
            message: error.message,
        }
    }
}

/// A failure of wasmtime during a call of `module`: an epoch
/// interruption past the call's deadline is a [`ModuleError::Timeout`]
/// (MOD-08), anything else a trap, logged in full and answered with its
/// short reason.
pub(crate) fn failure(module: &ModuleName, error: &wasmtime::Error) -> ModuleError {
    if let Some(fault) = error.downcast_ref::<HostFault>() {
        return ModuleError::HostFailed {
            module: module.to_string(),
            reason: fault.to_string(),
        };
    }
    if error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt) {
        tracing::error!(%module, "a module call ran past its call timeout");
        return ModuleError::Timeout {
            module: module.to_string(),
        };
    }
    tracing::error!(%module, error = ?error, "a module trapped");
    let reason = error
        .downcast_ref::<wasmtime::Trap>()
        .map_or_else(|| error.root_cause().to_string(), ToString::to_string);
    ModuleError::Trap {
        module: module.to_string(),
        reason,
    }
}

/// The error of a module that does not load.
pub(crate) fn load_error(name: &ModuleName, error: &wasmtime::Error) -> ModuleError {
    ModuleError::Load {
        module: name.to_string(),
        reason: format!("{error:#}"),
    }
}
