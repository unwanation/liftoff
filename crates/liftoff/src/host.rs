//! Programs `server upgrade` runs on this machine.

use std::io::Write as _;
use std::process::{Command, Stdio};

/// The installer of the release (`scripts/install.sh`): finds the latest
/// release on the forge, downloads `liftoffd` and `liftoff`, checks them
/// against its signed `SHA256SUMS` and replaces the binaries.
pub const INSTALL_SCRIPT: &str = include_str!("../../../scripts/install.sh");

/// Runs a program on this machine. Its output goes to standard error: the
/// CLI owns standard output.
pub trait HostCommand: Send {
    /// Runs `program` with `args` to completion, `stdin` written to its
    /// standard input.
    ///
    /// # Errors
    ///
    /// What went wrong, when it cannot be started or exits non-zero.
    fn run(&mut self, program: &str, args: &[&str], stdin: Option<&str>) -> Result<(), String>;
}

/// Child processes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessHost;

impl HostCommand for ProcessHost {
    fn run(&mut self, program: &str, args: &[&str], stdin: Option<&str>) -> Result<(), String> {
        let shown = || {
            std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::from(std::io::stderr()))
            .spawn()
            .map_err(|error| format!("cannot run `{}`: {error}", shown()))?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input.as_bytes())
                .map_err(|error| format!("cannot feed `{}`: {error}", shown()))?;
        }
        let status = child
            .wait()
            .map_err(|error| format!("cannot wait for `{}`: {error}", shown()))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("`{}` failed ({status})", shown()))
        }
    }
}
