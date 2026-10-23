use core::time::Duration;

use futures_core::Stream;

use crate::domain::{ComposeProject, Host, Login, ServiceName};
use crate::error::Error;

/// Which pipe a line of command output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One line of command output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    /// The pipe.
    pub stream: OutputStream,
    /// The line, without its newline.
    pub text: String,
}

/// What a finished command printed and how it exited.
///
/// A command that ran and exited non-zero is an `Ok(CommandOutput)` whose
/// [`CommandOutput::success`] is false: the deploy log needs its lines and
/// the deploy error its last stderr line (DEP-09). `Err` means the command
/// could not run at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code; `None` when killed by a signal.
    pub exit_code: Option<i32>,
    /// Interleaved output, in order.
    pub lines: Vec<OutputLine>,
}

impl CommandOutput {
    /// A successful run that printed `stdout`.
    #[must_use]
    pub fn ok<'a>(stdout: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            exit_code: Some(0),
            lines: stdout
                .into_iter()
                .map(|text| OutputLine {
                    stream: OutputStream::Stdout,
                    text: text.to_owned(),
                })
                .collect(),
        }
    }

    /// A run that exited with `exit_code` after printing `stderr`.
    #[must_use]
    pub fn failed<'a>(exit_code: i32, stderr: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            exit_code: Some(exit_code),
            lines: stderr
                .into_iter()
                .map(|text| OutputLine {
                    stream: OutputStream::Stderr,
                    text: text.to_owned(),
                })
                .collect(),
        }
    }

    /// Whether the command exited with code 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// The last non-empty stderr line, the short reason of a failure.
    #[must_use]
    pub fn last_stderr_line(&self) -> Option<&str> {
        self.lines
            .iter()
            .rev()
            .filter(|line| line.stream == OutputStream::Stderr)
            .map(|line| line.text.trim())
            .find(|text| !text.is_empty())
    }
}

/// One service of a compose project, from `docker compose ps --format json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    /// Compose service name.
    pub service: String,
    /// Container state, e.g. `running`, `exited`.
    pub state: String,
    /// Health, e.g. `healthy`, when the service has a healthcheck.
    pub health: Option<String>,
}

/// `docker` and `docker compose` for an environment's compose project.
///
/// Compose commands run in the project's directory with
/// `-p <project> -f compose.yaml -f liftoff.override.yaml`; the adapter
/// derives the directory from the project name and its configured root.
///
/// # Errors
///
/// `Err` only when the command could not run; see [`CommandOutput`].
pub trait Docker: Send + Sync {
    /// `docker login <registry>` into liftoff's docker config.
    fn login(
        &self,
        registry: &Host,
        user: &Login,
        token: &str,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send;
    /// `docker compose ... pull`.
    fn compose_pull(
        &self,
        project: &ComposeProject,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send;
    /// `docker compose ... up -d --remove-orphans --wait --wait-timeout <s>`.
    fn compose_up(
        &self,
        project: &ComposeProject,
        wait_timeout: Duration,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send;
    /// `docker compose ... down`, with `-v` when `volumes`.
    fn compose_down(
        &self,
        project: &ComposeProject,
        volumes: bool,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send;
    /// `docker compose -p <project> ps --format json`; empty when nothing
    /// runs.
    fn compose_ps(
        &self,
        project: &ComposeProject,
    ) -> impl Future<Output = Result<Vec<ServiceStatus>, Error>> + Send;
    /// `docker compose ... logs --no-color [--tail N] [-f] [service]`, one
    /// item per line.
    fn logs(
        &self,
        project: &ComposeProject,
        service: Option<&ServiceName>,
        tail: Option<u32>,
        follow: bool,
    ) -> impl Future<
        Output = Result<impl Stream<Item = Result<String, Error>> + Send + 'static, Error>,
    > + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    mod command_output {
        use super::*;

        #[test]
        fn success_when_exit_zero() {
            assert!(CommandOutput::ok(["done"]).success(), "exit 0 is success");
        }

        // TC: v1 DEP-09 (error is the last stderr line)
        #[test]
        fn skips_stdout_and_blank_lines_when_last_stderr_line() {
            let mut output = CommandOutput::failed(1, ["pulling", "boom: no such image", ""]);
            output.lines.push(OutputLine {
                stream: OutputStream::Stdout,
                text: "after".to_owned(),
            });
            assert_eq!(output.last_stderr_line(), Some("boom: no such image"));
        }

        #[test]
        fn last_stderr_line_is_none_when_only_stdout() {
            assert_eq!(CommandOutput::ok(["a"]).last_stderr_line(), None);
        }
    }
}
