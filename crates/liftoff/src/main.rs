//! The `liftoff` binary: wires the real terminal, clock and Ctrl-C into
//! [`liftoff::run`].

use std::io::{self, IsTerminal as _, Write as _};
use std::process::ExitCode;

use liftoff::{
    Env, GitCommand, ProcessHost, TerminalPrompter, TokioSleeper, default_config_path, run,
};
use tokio_util::sync::CancellationToken;

/// The filter of the log when `LIFTOFF_LOG` sets none.
const DEFAULT_LOG: &str = "warn";

/// The log on stderr (the CLI owns stdout), filtered by `LIFTOFF_LOG`.
fn init_log() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("LIFTOFF_LOG")
        .unwrap_or_else(|_unset| tracing_subscriber::EnvFilter::new(DEFAULT_LOG));
    let _already: Result<(), _> = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .without_time()
        .with_target(false)
        .try_init();
}

fn main() -> ExitCode {
    init_log();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ignored: io::Result<()> =
                writeln!(io::stderr(), "error: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(start());
    // A request stuck on the network must not keep the process alive.
    runtime.shutdown_background();
    code
}

/// The host name, sent as the name of the session `login` creates.
fn machine_name() -> Option<String> {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

async fn start() -> ExitCode {
    let Some(config_path) = default_config_path(
        std::env::var_os("LIFTOFF_CONFIG"),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    ) else {
        let mut stderr = io::stderr();
        let _ignored: io::Result<()> = writeln!(
            stderr,
            "error: cannot find the config directory; set HOME or LIFTOFF_CONFIG"
        );
        return ExitCode::FAILURE;
    };

    let cancel = CancellationToken::new();
    let on_ctrl_c = cancel.clone();
    let _watcher = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            let _ignored: io::Result<()> = writeln!(
                io::stderr(),
                "error: cannot read the working directory: {error}"
            );
            return ExitCode::FAILURE;
        }
    };
    let mut env = Env {
        config_path,
        cwd,
        out: Box::new(io::stdout()),
        err: Box::new(io::stderr()),
        tty: io::stdin().is_terminal(),
        prompter: Box::new(TerminalPrompter),
        git: Box::new(GitCommand),
        host: Box::new(ProcessHost),
        machine: machine_name(),
        sleeper: TokioSleeper,
        cancel,
        local: liftoff::local::Local::release(),
    };
    let argv: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    ExitCode::from(run(argv, &mut env).await)
}
