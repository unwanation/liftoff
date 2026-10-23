//! [`AppError`]: why the daemon exits non-zero, and with which code.

/// Anything that can make the binary exit non-zero.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The layered configuration did not load.
    #[error("invalid configuration")]
    Config(#[from] Box<figment::Error>),
    /// A setting the services need is missing or malformed.
    #[error("invalid configuration")]
    Settings(#[source] liftoff_core::Error),
    /// The master key file is missing or not a key.
    #[error("cannot load the master key from {}", path.display())]
    MasterKey {
        /// The key file.
        path: std::path::PathBuf,
        /// Why not.
        #[source]
        source: liftoff_core::Error,
    },
    /// Telemetry could not be installed.
    #[error("failed to initialise telemetry")]
    Telemetry(#[from] crate::telemetry::InitError),
    /// An adapter could not be built or the database not opened.
    #[error("failed to set up {what}")]
    Setup {
        /// What was being set up.
        what: String,
        /// Why not.
        #[source]
        source: liftoff_core::Error,
    },
    /// A listener address could not be bound.
    #[error("failed to bind the {what} listener on {addr}")]
    Bind {
        /// Which listener, e.g. `"REST"`.
        what: &'static str,
        /// The configured address.
        addr: core::net::SocketAddr,
        /// Why not.
        #[source]
        source: std::io::Error,
    },
    /// `liftoffd setup` stopped.
    #[error(transparent)]
    Install(#[from] crate::setup::SetupError),
    /// `liftoffd import-artifacts` stopped.
    #[error(transparent)]
    ImportArtifacts(#[from] crate::import_artifacts::ImportArtifactsError),
    /// The REST listener failed.
    #[error(transparent)]
    Rest(#[from] liftoff_rest::ServeError),
}

impl AppError {
    /// `sysexits.h`-style exit code.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) | Self::Settings(_) | Self::MasterKey { .. } => 78, // EX_CONFIG
            Self::Install(error) => error.exit_code(),
            Self::ImportArtifacts(error) => error.exit_code(),
            _ => 70, // EX_SOFTWARE
        }
    }
}
