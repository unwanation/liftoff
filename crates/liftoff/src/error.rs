//! What can go wrong in a run, and the exit code each failure ends it with.

use crate::mode::PromptError;

/// Exit code: the API refused, nobody is logged in, or a check failed.
pub(crate) const EXIT_FAILURE: u8 = 1;
/// Exit code: wrong or missing arguments (`EX_USAGE`).
pub(crate) const EXIT_USAGE: u8 = 64;
/// Exit code: the server cannot be reached (`EX_UNAVAILABLE`).
pub(crate) const EXIT_UNAVAILABLE: u8 = 69;
/// Exit code: the config file or `.liftoff` is wrong (`EX_CONFIG`).
pub(crate) const EXIT_CONFIG: u8 = 78;
/// Exit code: stopped by Ctrl-C (`128 + SIGINT`).
pub(crate) const EXIT_INTERRUPTED: u8 = 130;

/// What the next step is after a rejected session, shown on a terminal.
pub(crate) const LOGIN_HINT: &str = "run `liftoff login <url> --token <t>`";

/// A failed run. `Display` is what follows `error: ` on stderr.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// No session for the chosen server.
    #[error("not logged in; run `liftoff login <url> --token <t>`")]
    NotLoggedIn,
    /// The API answered with an error status: its `detail` and the body as
    /// received (what `--json` prints).
    #[error("{message}")]
    Api {
        /// `detail`, else `title`, else the status line.
        message: String,
        /// The body of the answer.
        body: String,
    },
    /// The server refused the session (HTTP 401).
    #[error("{message}")]
    Unauthorized {
        /// `detail`, else `title`, else the status line.
        message: String,
        /// The body of the answer.
        body: String,
    },
    /// The server answered with something the CLI cannot read.
    #[error("unexpected answer from the server: {0}")]
    Unexpected(String),
    /// The server did not answer.
    #[error("cannot reach the server: {0}")]
    Network(String),
    /// Ctrl-C during a request or a wait.
    #[error("interrupted")]
    Interrupted,
    /// Required arguments that batch mode cannot ask for, in order.
    #[error("missing required arguments: {}", .0.join(", "))]
    Missing(Vec<String>),
    /// A module the one being installed requires is not installed
    /// (checked before anything is sent).
    #[error("install the module `{0}`")]
    ModuleRequired(String),
    /// An argument that does not fit.
    #[error("{0}")]
    Usage(String),
    /// The config file or `.liftoff` cannot be used.
    #[error("{0}")]
    Config(String),
    /// The user said no, or gave up at a question.
    #[error("{0}")]
    Aborted(String),
    /// A deploy that was waited for failed.
    #[error("{0}")]
    DeployFailed(String),
    /// `doctor` found red checks.
    #[error("{0}")]
    ChecksFailed(String),
    /// `secret sync` could not send some keys; the others were sent.
    #[error("{0}")]
    SyncFailed(String),
    /// A local file or the terminal could not be used.
    #[error("{0}")]
    Local(String),
}

impl CliError {
    /// The process exit code of this failure.
    pub(crate) const fn exit_code(&self) -> u8 {
        match self {
            Self::NotLoggedIn
            | Self::Api { .. }
            | Self::Unauthorized { .. }
            | Self::Unexpected(_)
            | Self::Aborted(_)
            | Self::ModuleRequired(_)
            | Self::DeployFailed(_)
            | Self::ChecksFailed(_)
            | Self::SyncFailed(_)
            | Self::Local(_) => EXIT_FAILURE,
            Self::Missing(_) | Self::Usage(_) => EXIT_USAGE,
            Self::Network(_) => EXIT_UNAVAILABLE,
            Self::Config(_) => EXIT_CONFIG,
            Self::Interrupted => EXIT_INTERRUPTED,
        }
    }

    /// The body of a refused request, for `--json`.
    pub(crate) fn body(&self) -> Option<&str> {
        match self {
            Self::Api { body, .. } | Self::Unauthorized { body, .. } => Some(body),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::Local(error.to_string())
    }
}

impl From<PromptError> for CliError {
    fn from(error: PromptError) -> Self {
        match error {
            PromptError::Cancelled => Self::Aborted("cancelled".to_owned()),
            PromptError::Terminal(text) => Self::Local(format!("cannot ask: {text}")),
        }
    }
}

/// A core service of local mode refused, or a local adapter failed: exit
/// 1, as a refusal of the API is in server mode.
impl From<liftoff_core::Error> for CliError {
    fn from(error: liftoff_core::Error) -> Self {
        match error {
            liftoff_core::Error::ModuleRequired(name) => Self::ModuleRequired(name),
            liftoff_core::Error::Internal(source) => {
                // The cause goes to the log on stderr (`LIFTOFF_LOG`),
                // never into the answer, as the daemon keeps it.
                tracing::error!(error = %source, "internal error of local mode");
                Self::Local("internal error of local mode (its cause is logged above)".to_owned())
            }
            other => Self::Local(other.to_string()),
        }
    }
}
