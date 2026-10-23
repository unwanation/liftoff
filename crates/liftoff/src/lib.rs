//! `liftoff`: the command-line client for the liftoff deploy service.
//!
//! An HTTP client over the `liftoffd` API, on `liftoff-api` for the wire
//! types; without a server, local mode ([`local`]) runs the
//! core services of `liftoff-core` over local adapters and the `git.*`
//! modules in the CLI's own `liftoff-wasm` host. It never depends on the
//! daemon's adapters (`liftoff-store`, `liftoff-host`, `liftoff-proxy`,
//! `liftoff-rest`).
//!
//! Everything a run touches (config path, stdout/stderr/stdin, the clock,
//! whether stdin is a terminal, Ctrl-C) is passed in through [`Env`], so the
//! binary is a few lines and the tests drive [`run`] against a local stub.
//!
//! Exit codes: `0` ok, `1` API error / not logged in, `64` wrong or missing
//! arguments, `69` the server cannot be reached, `78` bad config.
//!
//! Two modes ([`Mode`]): on a terminal a command asks for what
//! is missing; without one, or with `--no-input`, it asks nothing and a
//! missing argument ends the run with exit code 64 and the list of what is
//! missing. Either way the same arguments send the same requests.

mod args;
mod client;
mod commands;
mod config;
mod dynamic;
mod error;
mod host;
pub mod local;
mod mode;
mod project;
mod render;
mod sse;

use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser as _;
use clap::error::ErrorKind;
use tokio_util::sync::CancellationToken;

use crate::args::Args;
use crate::commands::raw;
use crate::error::{CliError, EXIT_USAGE, LOGIN_HINT};
pub use crate::host::{HostCommand, INSTALL_SCRIPT, ProcessHost};
pub use crate::mode::{Mode, PromptError, Prompter, TerminalPrompter};
pub use crate::project::{GitCommand, GitRemote};

/// Pause between two looks at a deploy that is still running.
pub const WAIT_INTERVAL: Duration = Duration::from_secs(2);

/// A clock. Tests inject one that does not wait.
pub trait Sleeper: Send + Sync {
    /// Completes after `duration`.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// Everything a run reads from or writes to besides the network.
pub struct Env<S> {
    /// The config file (`~/.config/liftoff/config.toml` in production).
    pub config_path: PathBuf,
    /// The working directory: where `.liftoff` and `liftoff.toml` are
    /// looked up and `git remote` runs.
    pub cwd: PathBuf,
    /// Standard output.
    pub out: Box<dyn Write + Send>,
    /// Standard error.
    pub err: Box<dyn Write + Send>,
    /// Whether standard input is a terminal; with `--no-input` it decides
    /// the [`Mode`].
    pub tty: bool,
    /// Asks the questions of [`Mode::Interactive`]; never called in
    /// [`Mode::Batch`].
    pub prompter: Box<dyn Prompter>,
    /// Reads `git remote get-url origin` for project detection.
    pub git: Box<dyn GitRemote>,
    /// Runs the installer and `systemctl` for `server upgrade`.
    pub host: Box<dyn HostCommand>,
    /// Name of this machine, sent as the session name by `login`.
    pub machine: Option<String>,
    /// The clock used between polls.
    pub sleeper: S,
    /// Cancelled on Ctrl-C.
    pub cancel: CancellationToken,
    /// What local mode needs: the module registry and the environment of
    /// the `git` children.
    pub local: local::Local,
}

impl<S> fmt::Debug for Env<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("config_path", &self.config_path)
            .field("cwd", &self.cwd)
            .field("tty", &self.tty)
            .field("machine", &self.machine)
            .finish_non_exhaustive()
    }
}

/// Where the config lives: `$LIFTOFF_CONFIG`, else
/// `$XDG_CONFIG_HOME/liftoff/config.toml`, else `$HOME/.config/liftoff/config.toml`.
#[must_use]
pub fn default_config_path(
    explicit: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    let non_empty = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    if let Some(path) = non_empty(explicit) {
        return Some(PathBuf::from(path));
    }
    let base = match (non_empty(xdg_config_home), non_empty(home)) {
        (Some(xdg), _) => PathBuf::from(xdg),
        (None, Some(home)) => PathBuf::from(home).join(".config"),
        (None, None) => return None,
    };
    Some(base.join("liftoff").join("config.toml"))
}

/// Runs `argv` (including the program name) and returns the exit code.
///
/// Failures are written to `env.err` as `error: <message>`; on a terminal
/// a rejected session adds the next step. With `--json` a refused request
/// also prints the problem object to `env.out` as received.
pub async fn run<S: Sleeper>(words: impl IntoIterator<Item = String>, env: &mut Env<S>) -> u8 {
    let args = match Args::try_parse_from(words) {
        Ok(args) => args,
        Err(error) => return report_parse_error(&error, env),
    };
    let mode = Mode::detect(env.tty, args.no_input);
    let json = args.json;
    match commands::dispatch(args, env, mode).await {
        Ok(()) => 0,
        Err(error) => {
            report(&error, env, mode, json);
            error.exit_code()
        }
    }
}

fn report<S>(error: &CliError, env: &mut Env<S>, mode: Mode, json: bool) {
    if json && let Some(body) = error.body() {
        // Nothing sensible is left to do when stdout itself is gone.
        drop(env.out.write_all(raw(body).as_bytes()));
        drop(env.out.flush());
    }
    let hint = match error {
        CliError::Unauthorized { .. } if mode == Mode::Interactive => format!("; {LOGIN_HINT}"),
        _ => String::new(),
    };
    drop(writeln!(env.err, "error: {error}{hint}"));
}

/// Help and version go to stdout with `0`; real mistakes to stderr with `64`.
fn report_parse_error<S>(error: &clap::Error, env: &mut Env<S>) -> u8 {
    let text = error.render().to_string();
    let informational = matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    );
    if informational {
        drop(write!(env.out, "{text}"));
        0
    } else {
        drop(write!(env.err, "{text}"));
        EXIT_USAGE
    }
}
