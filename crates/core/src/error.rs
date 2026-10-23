//! Error vocabulary of the domain.
//!
//! [`Error`] is what every service and port returns. Variants are semantic
//! (what went wrong for the caller), never transport-specific; each adapter
//! maps them once:
//!
//! | Variant           | HTTP |
//! |-------------------|------|
//! | `Validation`      | 400  |
//! | `Unauthenticated` | 401  |
//! | `Forbidden`       | 403  |
//! | `NotFound`        | 404  |
//! | `Conflict`        | 409  |
//! | `Gone`            | 410  |
//! | `ModuleRequired`  | 424  |
//! | `Internal`        | 500  |
//! | `Upstream`        | 502  |
//! | `Unavailable`     | 503  |
//! | `Offline`         | 503  |
//!
//! [`ValidationError`] is what a constructor of a domain type returns; it
//! converts into [`Error::Validation`].

use core::fmt;
use std::sync::Arc;

/// Every failure a service or port reports.
///
/// Cheap to clone (the internal source sits behind an [`Arc`]) so fakes can
/// replay a scripted error.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
#[expect(
    clippy::error_impl_error,
    reason = "plan 1.2 names it `Error`; outside the crate it reads `liftoff_core::Error`"
)]
pub enum Error {
    /// The caller sent something invalid. Safe to show.
    #[error(transparent)]
    Validation(#[from] ValidationError),
    /// The thing does not exist (or the caller may know it does not).
    #[error("{0} not found")]
    NotFound(String),
    /// The caller did not prove who it is: the session or token is
    /// missing, unknown, expired, revoked or logged out (AUTH-08..AUTH-12).
    /// The message is safe to show and never contains the credential.
    #[error("{0}")]
    Unauthenticated(String),
    /// The caller may not do this. The message is safe to show and never
    /// reveals whether a resource the caller cannot see exists.
    #[error("{0}")]
    Forbidden(String),
    /// The request contradicts current state: a name is taken, a quota is
    /// full, a deploy is running.
    #[error("{0}")]
    Conflict(String),
    /// The thing existed but has expired for good, e.g. a one-time
    /// token. Unlike [`Error::NotFound`], retrying cannot help.
    #[error("{0}")]
    Gone(String),
    /// An external system answered with an error; `detail` is its text.
    #[error("{system}: {detail}")]
    Upstream {
        /// Which system failed.
        system: System,
        /// What it said.
        detail: String,
    },
    /// A resource is temporarily or permanently exhausted, e.g. host ports.
    #[error("{0}")]
    Unavailable(String),
    /// The request needs a module that is not installed; the module's name
    /// (424 "install the module...").
    #[error("install the module `{0}`")]
    ModuleRequired(String),
    /// A tunnel environment whose CLI is not connected.
    #[error("{0}")]
    Offline(String),
    /// A bug or an unexpected failure. The source is for logs only and is
    /// **never** sent to a caller.
    #[error("internal error")]
    Internal(#[source] Arc<dyn core::error::Error + Send + Sync + 'static>),
}

impl Error {
    /// Wraps any error as [`Error::Internal`].
    pub fn internal(source: impl core::error::Error + Send + Sync + 'static) -> Self {
        Self::Internal(Arc::new(source))
    }

    /// An [`Error::Internal`] from a message, for invariants that broke.
    pub fn internal_msg(message: impl Into<String>) -> Self {
        Self::Internal(Arc::new(InternalMessage(message.into())))
    }

    /// Shorthand for [`Error::NotFound`].
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }

    /// Shorthand for [`Error::Unauthenticated`].
    pub fn unauthenticated(why: impl Into<String>) -> Self {
        Self::Unauthenticated(why.into())
    }

    /// Shorthand for [`Error::Gone`].
    pub fn gone(what: impl Into<String>) -> Self {
        Self::Gone(what.into())
    }

    /// Shorthand for [`Error::Forbidden`].
    pub fn forbidden(why: impl Into<String>) -> Self {
        Self::Forbidden(why.into())
    }

    /// Shorthand for [`Error::Conflict`].
    pub fn conflict(why: impl Into<String>) -> Self {
        Self::Conflict(why.into())
    }

    /// Shorthand for [`Error::Unavailable`].
    pub fn unavailable(why: impl Into<String>) -> Self {
        Self::Unavailable(why.into())
    }

    /// Shorthand for [`Error::ModuleRequired`].
    pub fn module_required(name: impl Into<String>) -> Self {
        Self::ModuleRequired(name.into())
    }

    /// Shorthand for [`Error::Offline`].
    pub fn offline(why: impl Into<String>) -> Self {
        Self::Offline(why.into())
    }

    /// Shorthand for [`Error::Upstream`].
    pub fn upstream(system: System, detail: impl Into<String>) -> Self {
        Self::Upstream {
            system,
            detail: detail.into(),
        }
    }

    /// Shorthand for a free-text [`Error::Validation`] on `field`.
    pub fn invalid(field: &'static str, why: impl Into<String>) -> Self {
        Self::Validation(ValidationError::new(field, Problem::Invalid(why.into())))
    }
}

/// An internal failure described only by text.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct InternalMessage(String);

/// External systems liftoff talks to, for [`Error::Upstream`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum System {
    /// A git forge's API, as its `git.*` module reaches it.
    Forge,
    /// Public DNS.
    Dns,
    /// The Caddy admin API.
    Caddy,
    /// `docker` / `docker compose`.
    Docker,
    /// `systemctl`.
    Systemd,
    /// The module registry (`[modules] registry`): unreachable, or a release that does not verify.
    Registry,
    /// What a module talks to, when the module answers the error code
    /// `upstream`.
    Module,
}

impl System {
    /// Lower-case name, as shown in error details.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Forge => "forge",
            Self::Dns => "dns",
            Self::Caddy => "caddy",
            Self::Docker => "docker",
            Self::Systemd => "systemd",
            Self::Registry => "registry",
            Self::Module => "module",
        }
    }
}

impl fmt::Display for System {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A value that failed validation, with the field it came from.
///
/// ```
/// use liftoff_core::domain::ProjectName;
///
/// let err = ProjectName::new("Shop").unwrap_err();
/// assert_eq!(err.field, "project");
/// assert_eq!(err.to_string(), "project: must match ^[a-z0-9][a-z0-9-]{0,31}$");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field}{}: {problem}", .path.as_ref().map_or_else(String::new, |path| format!(".{path}")))]
pub struct ValidationError {
    /// The input field (or path) the value came from, e.g. `project`.
    pub field: &'static str,
    /// Where below `field` the value sits, for structured input whose
    /// shape is not the core's (a module's configuration: `greeting` of
    /// `config`, shown as `config.greeting`).
    pub path: Option<String>,
    /// What is wrong with it.
    pub problem: Problem,
}

impl ValidationError {
    /// Builds an error for `field`.
    #[must_use]
    pub const fn new(field: &'static str, problem: Problem) -> Self {
        Self {
            field,
            path: None,
            problem,
        }
    }

    /// Builds an error for `path` below `field` (`config.greeting`).
    #[must_use]
    pub fn at(field: &'static str, path: impl Into<String>, problem: Problem) -> Self {
        Self {
            field,
            path: Some(path.into()),
            problem,
        }
    }

    /// The same problem reported against another field name, for adapters
    /// whose input names differ from the domain's defaults.
    #[must_use]
    pub fn with_field(self, field: &'static str) -> Self {
        Self { field, ..self }
    }
}

/// Why a value is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Problem {
    /// The value was empty.
    Empty,
    /// The value is longer than allowed.
    TooLong {
        /// Maximum length in characters.
        max: usize,
        /// Length supplied.
        actual: usize,
    },
    /// The value does not have the required shape.
    Pattern {
        /// Human-readable description of the shape (usually a regex).
        expected: &'static str,
    },
    /// A number is outside its allowed range.
    OutOfRange {
        /// Smallest allowed value.
        min: u64,
        /// Largest allowed value.
        max: u64,
    },
    /// The value is on a reserved list.
    Reserved,
    /// Any other rule, explained in words.
    Invalid(String),
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("must not be empty"),
            Self::TooLong { max, actual } => {
                write!(f, "must be at most {max} characters, got {actual}")
            }
            Self::Pattern { expected } => write!(f, "must match {expected}"),
            Self::OutOfRange { min, max } => write!(f, "must be between {min} and {max}"),
            Self::Reserved => f.write_str("is reserved"),
            Self::Invalid(why) => f.write_str(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod error {
        use super::*;

        #[test]
        fn names_the_field_when_validation() {
            let err = Error::from(ValidationError::new("slug", Problem::Empty));
            assert_eq!(err.to_string(), "slug: must not be empty");
        }

        // TC: REG-04 (a field path below the field: `config.greeting`)
        #[test]
        fn names_the_path_below_the_field_when_validation_has_a_path() {
            let err = Error::from(ValidationError::at(
                "config",
                "greeting",
                Problem::Invalid("value is not of type \"string\"".to_owned()),
            ));
            assert_eq!(
                err.to_string(),
                "config.greeting: value is not of type \"string\""
            );
        }

        #[test]
        fn hides_source_when_internal() {
            let err = Error::internal(std::io::Error::other("db password is hunter2"));
            assert_eq!(err.to_string(), "internal error");
        }

        // TC: v1 DOM-06 (Upstream{caddy} carries the validator text)
        #[test]
        fn system_and_detail_when_upstream() {
            let err = Error::upstream(System::Caddy, "unrecognized directive");
            assert_eq!(err.to_string(), "caddy: unrecognized directive");
        }

        #[test]
        fn keeps_internal_source_when_cloned() {
            let original = Error::internal_msg("boom");
            let copy = original.clone();
            drop(original);
            let source = core::error::Error::source(&copy).map(ToString::to_string);
            assert_eq!(source.as_deref(), Some("boom"));
        }

        // TC: v1 AUTH-08, v1 AUTH-12 (401 is its own variant, distinct from 403)
        #[test]
        fn distinct_from_forbidden_when_unauthenticated() {
            let err = Error::unauthenticated("session expired");
            assert!(
                matches!(&err, Error::Unauthenticated(why) if why == "session expired"),
                "{err:?}"
            );
        }

        #[test]
        fn message_is_the_reason_when_gone() {
            assert_eq!(
                Error::gone("device code expired").to_string(),
                "device code expired"
            );
        }

        #[test]
        fn names_the_thing_when_not_found() {
            assert_eq!(
                Error::not_found("project shop").to_string(),
                "project shop not found"
            );
        }
    }

    mod validation_error {
        use super::*;

        #[test]
        fn names_field_first_when_displayed() {
            let err = ValidationError::new("label", Problem::Reserved);
            assert_eq!(err.to_string(), "label: is reserved");
        }

        #[test]
        fn keeps_problem_when_field_renamed() {
            let err = ValidationError::new("name", Problem::Empty).with_field("target");
            assert_eq!(err, ValidationError::new("target", Problem::Empty));
        }
    }
}
