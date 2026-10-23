//! `tracing` setup. JSON lines by default for log shippers; `pretty` for humans.

use std::sync::atomic::{AtomicBool, Ordering};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::config::{LogConfig, LogFormat};

static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Telemetry could not be installed.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// The configured filter directive does not parse.
    #[error("invalid log filter {filter:?}")]
    Filter {
        /// The directive.
        filter: String,
        /// The parse error.
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },
    /// Another subscriber was installed first.
    #[error("a global tracing subscriber is already installed")]
    AlreadySet(#[from] tracing_subscriber::util::TryInitError),
}

/// Installs the global subscriber on stdout. `RUST_LOG`, when set, wins over
/// the config.
///
/// # Errors
///
/// [`InitError::Filter`] for a filter that does not parse,
/// [`InitError::AlreadySet`] when a subscriber is already installed.
pub fn init(config: &LogConfig) -> Result<(), InitError> {
    let filter = std::env::var(EnvFilter::DEFAULT_ENV)
        .map_or_else(
            |_| EnvFilter::try_new(&config.level),
            |env| EnvFilter::try_new(&env),
        )
        .map_err(|source| InitError::Filter {
            filter: config.level.clone(),
            source,
        })?;

    let writer = BoxMakeWriter::new(std::io::stdout);

    let registry = tracing_subscriber::registry().with(filter);
    match config.format {
        LogFormat::Json => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_target(true)
                    .with_writer(writer),
            )
            .try_init()?,
        LogFormat::Pretty => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .pretty()
                    .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
                    .with_writer(writer),
            )
            .try_init()?,
    }
    INITIALISED.store(true, Ordering::Release);
    Ok(())
}

/// Whether [`init`] has succeeded in this process.
pub fn is_initialised() -> bool {
    INITIALISED.load(Ordering::Acquire)
}

/// Installs a global subscriber of plain lines on stderr.
///
/// It serves the one-shot commands whose answer is their own
/// (`import-artifacts`). `RUST_LOG`, when set, wins over `level`. It leaves [`is_initialised`]
/// as it was, so a failure is still printed on stderr in full.
///
/// # Errors
///
/// As [`init`].
pub fn init_stderr(level: &str) -> Result<(), InitError> {
    let filter = std::env::var(EnvFilter::DEFAULT_ENV)
        .map_or_else(
            |_| EnvFilter::try_new(level),
            |env| EnvFilter::try_new(&env),
        )
        .map_err(|source| InitError::Filter {
            filter: level.to_owned(),
            source,
        })?;
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .without_time()
                .with_target(false)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
                .with_writer(std::io::stderr),
        )
        .try_init()?;
    Ok(())
}
