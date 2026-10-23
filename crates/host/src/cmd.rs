//! A process invocation as data, and the runner that executes it.

use std::ffi::OsString;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use futures_core::Stream;
use liftoff_core::ports::{CommandOutput, OutputLine, OutputStream};
use liftoff_core::{Error, System};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// What to run: program, arguments, extra environment, directory and stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cmd {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(OsString, OsString)>,
    /// Start from an empty environment instead of the daemon's own.
    pub(crate) clear_env: bool,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) stdin: Option<String>,
}

impl Cmd {
    pub(crate) fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            clear_env: false,
            cwd: None,
            stdin: None,
        }
    }

    pub(crate) fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub(crate) fn env(mut self, key: &str, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The child gets only what [`Cmd::env`] adds, not the daemon's variables.
    pub(crate) const fn env_clear(mut self) -> Self {
        self.clear_env = true;
        self
    }

    pub(crate) fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub(crate) fn stdin(mut self, input: impl Into<String>) -> Self {
        self.stdin = Some(input.into());
        self
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        if self.clear_env {
            command.env_clear();
        }
        command
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        command
    }

    /// Starts the process; its output lines arrive on the channel of the
    /// result, which closes when both pipes are done.
    fn spawn(&self, system: System) -> Result<Running, Error> {
        let mut child = self.command().spawn().map_err(|source| {
            Error::upstream(
                system,
                format!("cannot run {}: {source}", self.program.to_string_lossy()),
            )
        })?;
        let (tx, rx) = mpsc::channel(256);
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(pump(stdout, OutputStream::Stdout, tx.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(pump(stderr, OutputStream::Stderr, tx));
        }
        if let (Some(input), Some(mut stdin)) = (self.stdin.clone(), child.stdin.take()) {
            tokio::spawn(async move {
                // A process that stops reading early is reported by its exit code.
                let _written = stdin.write_all(input.as_bytes()).await;
            });
        }
        Ok(Running { child, rx })
    }

    /// Runs to completion. A non-zero exit is an `Ok` with the lines; `Err`
    /// only when the process could not be started or waited for.
    pub(crate) async fn run(&self, system: System) -> Result<CommandOutput, Error> {
        let Running { mut child, mut rx } = self.spawn(system)?;
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        let status = child.wait().await.map_err(|source| {
            Error::upstream(
                system,
                format!(
                    "cannot wait for {}: {source}",
                    self.program.to_string_lossy()
                ),
            )
        })?;
        Ok(CommandOutput {
            exit_code: status.code(),
            lines,
        })
    }

    /// Runs to completion and turns a failed run into
    /// [`Error::Upstream`] carrying the process's output.
    pub(crate) async fn run_checked(&self, system: System) -> Result<CommandOutput, Error> {
        let output = self.run(system).await?;
        if output.success() {
            Ok(output)
        } else {
            Err(Error::upstream(system, failure_detail(&output)))
        }
    }

    /// Starts the process and streams its output lines (both pipes) until it
    /// ends. Dropping the stream kills the process.
    pub(crate) fn stream(&self, system: System) -> Result<LineStream, Error> {
        let Running { child, rx } = self.spawn(system)?;
        Ok(LineStream { rx, _child: child })
    }
}

/// The text of a failed run: its stderr lines, else all lines, else the exit.
pub(crate) fn failure_detail(output: &CommandOutput) -> String {
    let text = |only_stderr: bool| {
        output
            .lines
            .iter()
            .filter(|line| !only_stderr || line.stream == OutputStream::Stderr)
            .map(|line| line.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let detail = text(true);
    if !detail.is_empty() {
        return detail;
    }
    let detail = text(false);
    if !detail.is_empty() {
        return detail;
    }
    output.exit_code.map_or_else(
        || "killed by a signal".to_owned(),
        |code| format!("exited with status {code}"),
    )
}

struct Running {
    child: Child,
    rx: mpsc::Receiver<OutputLine>,
}

async fn pump(pipe: impl AsyncRead + Unpin, stream: OutputStream, tx: mpsc::Sender<OutputLine>) {
    // Raw bytes: a line that is not UTF-8 must not stop the pump, or the
    // process would get a broken pipe on its next write.
    let mut reader = BufReader::new(pipe);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        if tx.send(OutputLine { stream, text }).await.is_err() {
            break;
        }
    }
}

/// Output lines of a running process as a [`Stream`]; owns the process.
pub(crate) struct LineStream {
    rx: mpsc::Receiver<OutputLine>,
    _child: Child,
}

impl Stream for LineStream {
    type Item = Result<String, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx
            .poll_recv(cx)
            .map(|line| line.map(|line| Ok(line.text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod runner {
        use super::*;

        #[tokio::test]
        async fn succeeds_when_command_exits_zero() {
            let output = Cmd::new("true").run(System::Docker).await;
            assert!(output.is_ok_and(|o| o.success()), "`true` should succeed");
        }

        #[tokio::test]
        async fn captures_both_pipes_when_command_prints() {
            let cmd = Cmd::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]);
            let output = cmd.run(System::Docker).await.expect("sh runs");
            assert_eq!(output.exit_code, Some(3));
            assert!(output.lines.contains(&OutputLine {
                stream: OutputStream::Stdout,
                text: "out".to_owned()
            }));
            assert_eq!(output.last_stderr_line(), Some("err"));
        }

        #[tokio::test]
        async fn keeps_reading_when_a_line_is_not_utf8() {
            let cmd = Cmd::new("sh").args(["-c", "printf 'a\\377b\\nnext\\n'"]);
            let output = cmd.run(System::Docker).await.expect("sh runs");
            let texts: Vec<&str> = output.lines.iter().map(|l| l.text.as_str()).collect();
            assert_eq!(texts, ["a\u{fffd}b", "next"]);
        }

        #[tokio::test]
        async fn passes_env_and_stdin_when_set() {
            let cmd = Cmd::new("sh")
                .args(["-c", "read line; echo \"$GREETING $line\""])
                .env("GREETING", "hello")
                .stdin("world\n");
            let output = cmd.run(System::Docker).await.expect("sh runs");
            assert_eq!(
                output.lines.first().map(|l| l.text.as_str()),
                Some("hello world")
            );
        }

        #[tokio::test]
        async fn hides_the_parent_environment_when_cleared() {
            // Cargo sets this variable for every test process.
            let script = concat!(
                "if [ -n \"$CARGO_MANIFEST_DIR\" ]; then a=set; else a=unset; fi; ",
                "if [ -n \"$KEPT\" ]; then b=set; else b=unset; fi; echo \"$a $b\""
            );
            let inherited = Cmd::new("sh").args(["-c", script]);
            let cleared = Cmd::new("sh")
                .args(["-c", script])
                .env_clear()
                .env("KEPT", "yes");
            let first = |output: CommandOutput| output.lines.first().map(|l| l.text.clone());
            let inherited = first(inherited.run(System::Docker).await.expect("runs"));
            let cleared = first(cleared.run(System::Docker).await.expect("runs"));
            assert_ne!(inherited.as_deref(), Some("unset unset"));
            assert_eq!(cleared.as_deref(), Some("unset set"));
        }

        #[tokio::test]
        async fn upstream_error_when_program_is_missing() {
            let err = Cmd::new("/nonexistent/liftoff-test")
                .run(System::Systemd)
                .await
                .expect_err("cannot run");
            assert!(
                matches!(
                    err,
                    Error::Upstream {
                        system: System::Systemd,
                        ..
                    }
                ),
                "got {err:?}"
            );
        }

        #[tokio::test]
        async fn upstream_error_carries_stderr_when_checked_run_fails() {
            let cmd = Cmd::new("sh").args(["-c", "echo bad directive >&2; exit 1"]);
            let err = cmd.run_checked(System::Systemd).await.expect_err("fails");
            assert_eq!(err.to_string(), "systemd: bad directive");
        }
    }

    mod failure_detail {
        use super::*;

        #[test]
        fn names_the_exit_code_when_nothing_was_printed() {
            let output = CommandOutput {
                exit_code: Some(2),
                lines: Vec::new(),
            };
            assert_eq!(failure_detail(&output), "exited with status 2");
        }
    }
}
