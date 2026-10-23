//! `liftoffd`: the liftoff daemon.
//!
//! ```text
//! liftoffd [--config FILE] [serve]
//! liftoffd [--config FILE] import-artifacts DIR
//! liftoffd setup [--base-domain D] [--public-url U] [--public-ip IP]
//!                [--tls acme|edge] [--edge-port P] [--acme-email E] [--yes] [--force]
//! ```
//!
//! # Layout
//!
//! - [`config`]: figment configuration (defaults, `/etc/liftoff/liftoffd.toml`,
//!   `LIFTOFF_*`) and the secret files it names.
//! - [`daemon`]: the production composition root, the only place that builds
//!   real adapters.
//! - [`import_artifacts`]: `liftoffd import-artifacts <dir>`, the import
//!   of the standalone artifacts server while the daemon is stopped.
//! - [`listeners`]: the function listener on `[functions] bind` and the tunnel listener on `[tunnel] bind`.
//! - [`mcp_http`]: the Streamable HTTP transport of MCP at `/mcp`.
//! - [`modules`]: the registry key, the module runtime and the registry
//!   client of installing modules.
//! - [`queue`]: the deploy queue.
//! - [`reverify`]: the daily re-verification of external domains (plan
//!   2.7).
//! - [`setup`]: `liftoffd setup`, the installation on a host.
//! - [`sweep`]: the sweep of expired artifacts once a minute.
//! - [`system`]: the real clock and random source.
//! - [`system_compose`]: the compose project `lf_system` with Caddy.
//! - [`tunnels`]: the tunnel server behind `GET /api/v1/tunnel`.
//!
//! `--import` of a v1 database comes with.

mod args;
pub mod config;
pub mod daemon;
pub mod error;
pub mod functions;
pub mod import_artifacts;
pub mod listeners;
pub mod mcp_http;
pub mod modules;
pub mod queue;
pub mod reverify;
pub mod setup;
mod shutdown;
pub mod sweep;
pub mod system;
pub mod system_compose;
pub mod telemetry;
pub mod tunnels;

use std::process::ExitCode;

use clap::Parser as _;

use crate::args::{Args, RootCommand};
use crate::config::AppConfig;
use crate::error::AppError;

/// The whole binary: parse arguments, run, turn failure into an exit code.
pub async fn main_entry() -> ExitCode {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    ExitCode::from(run_cli(argv).await)
}

/// Runs `argv` (program name first) and returns the exit code.
///
/// `0`, `78` for a configuration error, `64` for missing setup answers,
/// `1` for a missing input of `import-artifacts`, `70` otherwise; a
/// malformed command line is clap's (`2`, help `0`).
pub async fn run_cli<I, T>(argv: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let parsed = match Args::try_parse_from(argv) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ignored: Result<(), std::io::Error> = error.print();
            return u8::try_from(error.exit_code()).unwrap_or(2);
        }
    };
    match run(parsed).await {
        Ok(()) => 0,
        Err(err) => {
            report(&err, telemetry::is_initialised());
            err.exit_code()
        }
    }
}

/// `err` and every cause below it, as `outer: inner: innermost`.
fn chain(err: &dyn core::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The `exiting` event: the whole chain in the `error` field.
fn log_exit(err: &AppError) {
    tracing::error!(error = %chain(err), "exiting");
}

/// Logs the failure chain. Falls back to stderr when tracing was never
/// initialised (`traced` false, e.g. the config itself failed to load).
fn report(err: &AppError, traced: bool) {
    if traced {
        log_exit(err);
    } else {
        use std::io::Write as _;
        let mut stderr = std::io::stderr().lock();
        let _ignored: Result<(), std::io::Error> = writeln!(stderr, "error: {err}");
        let mut source = core::error::Error::source(err);
        while let Some(cause) = source {
            let _ignored: Result<(), std::io::Error> = writeln!(stderr, "  caused by: {cause}");
            source = cause.source();
        }
    }
}

async fn run(args: Args) -> Result<(), AppError> {
    match args.command.unwrap_or_default() {
        RootCommand::Serve => {
            let file = args.config.or_else(daemon::default_config_file);
            let config = AppConfig::load(file.as_deref())?;
            telemetry::init(&config.log)?;
            tracing::debug!("starting");
            let shutdown = shutdown::Shutdown::new();
            let outcome = daemon::serve(config, shutdown.token()).await;
            shutdown.trigger();
            outcome
        }
        // Setup writes the configuration; it does not read one.
        RootCommand::Setup(setup) => setup::setup_host(setup).await.map_err(AppError::from),
        // It opens the store and the blobs of the configuration, never
        // the daemon's listeners.
        RootCommand::ImportArtifacts(import) => {
            let file = args.config.or_else(daemon::default_config_file);
            import_artifacts::run(file.as_deref(), &import).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod chain {
        use super::*;

        #[test]
        fn joins_the_cause_when_the_error_has_a_source() {
            let error = AppError::Bind {
                what: "REST",
                addr: "127.0.0.1:7000".parse().unwrap(),
                source: std::io::Error::other("address in use"),
            };
            assert_eq!(
                chain(&error),
                "failed to bind the REST listener on 127.0.0.1:7000: address in use"
            );
        }

        #[test]
        fn is_the_message_alone_when_there_is_no_source() {
            let error = std::io::Error::other("alone");
            assert_eq!(chain(&error), "alone");
        }
    }

    mod report {
        use std::sync::{Arc, Mutex};

        use super::*;

        /// A writer into a shared buffer.
        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .map_err(|_poisoned| std::io::Error::other("poisoned"))?
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        #[test]
        fn logs_the_whole_chain_in_the_error_field_when_the_error_has_a_source() {
            let buffer = Buffer::default();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer({
                    let buffer = buffer.clone();
                    move || buffer.clone()
                })
                .finish();
            let error = AppError::Bind {
                what: "REST",
                addr: "127.0.0.1:7000".parse().unwrap(),
                source: std::io::Error::other("address in use"),
            };
            tracing::subscriber::with_default(subscriber, || report(&error, true));
            let logged = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
            assert!(
                logged.contains(
                    "error=failed to bind the REST listener on 127.0.0.1:7000: address in use"
                ),
                "{logged}"
            );
        }
    }
}
