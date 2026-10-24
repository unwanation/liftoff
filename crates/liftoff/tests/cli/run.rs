//! The binary itself as a child of the test, with the config home of a
//! [`Cli`] (`LIFTOFF_CONFIG`, `HOME` in its tempdir, nothing else of the
//! test's environment but `PATH`): what it starts is a real process and
//! Ctrl-C is a real `SIGINT`.

use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::process::{Child, Command};

use crate::support::Cli;

/// How long a run may take before the test gives up.
const PATIENCE: Duration = Duration::from_secs(60);

/// The home directory of `cli`'s config (`<home>/.config/liftoff/config.toml`).
fn home_of(cli: &Cli) -> PathBuf {
    cli.config.ancestors().nth(3).unwrap().to_path_buf()
}

/// `liftoff <args>` started as a process with the config home of `cli`.
pub(crate) fn spawn(cli: &Cli, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_liftoff"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home_of(cli))
        .env("LIFTOFF_CONFIG", &cli.config)
        .current_dir(&cli.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("liftoff starts")
}

/// What a finished run of the binary printed and returned.
#[derive(Debug)]
pub(crate) struct Ran {
    pub(crate) code: Option<i32>,
    pub(crate) out: String,
    pub(crate) err: String,
}

impl From<Output> for Ran {
    fn from(output: Output) -> Self {
        Self {
            code: output.status.code(),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            err: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

/// Runs `liftoff <args>` to its end.
pub(crate) async fn lof(cli: &Cli, args: &[&str]) -> Ran {
    finish(spawn(cli, args)).await
}

/// The run of `child` to its end.
pub(crate) async fn finish(child: Child) -> Ran {
    tokio::time::timeout(PATIENCE, child.wait_with_output())
        .await
        .expect("liftoff ends")
        .unwrap()
        .into()
}
