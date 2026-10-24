//! The process group of a child the CLI leads (the `[function] build` of
//! `lof deploy`), and the signals the CLI watches while it runs: the
//! child leads its own group, so Ctrl-C (the CLI's `SIGINT`), and
//! `SIGTERM` or `SIGHUP` of the CLI (`kill`, the terminal closed), end
//! the whole group: `SIGTERM`, then `SIGKILL` for what of the group is
//! left after a grace period.

use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use tokio::process::Child;
use tokio::signal::unix::{Signal as Watch, SignalKind, signal as watch};

use crate::error::CliError;

/// How long a child's group has to end after `SIGTERM` before
/// `SIGKILL`.
const GRACE: Duration = Duration::from_secs(5);

/// How often [`stop_group`] looks whether the group has ended.
const POLL: Duration = Duration::from_millis(50);

/// Starts watching the signal `kind`.
pub(super) fn watching(kind: SignalKind) -> Result<Watch, CliError> {
    watch(kind).map_err(|error| CliError::Local(format!("cannot watch signals: {error}")))
}

/// The process group `child` leads (it was spawned with
/// `process_group(0)`); `None` once it has been waited for, so a caller
/// that waits for it first takes the group before.
pub(super) fn group_of(child: &Child) -> Option<Pid> {
    child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
}

/// Ends the group `group` that `child` led, also after `child` itself
/// has ended: `SIGTERM`, then `SIGKILL` for what of the group outlives
/// [`GRACE`] (a grandchild that ignores `SIGTERM`, or one still cleaning
/// up). The grace period is the group's: `sh` itself ends at once, what
/// it started may not.
pub(super) async fn stop_group(group: Option<Pid>, child: &mut Child) {
    if let Some(group) = group {
        signal(group, Signal::TERM);
        let ended = async {
            while group_runs(group, child) {
                tokio::time::sleep(POLL).await;
            }
        };
        let outlived = tokio::time::timeout(GRACE, ended).await.is_err();
        if outlived && group_runs(group, child) {
            signal(group, Signal::KILL);
        }
    }
    if let Err(error) = child.kill().await {
        tracing::debug!(%error, "the child is already gone");
    }
}

/// Whether a process of the group `group` is left. Reaps `child` first:
/// the zombie of the shell would count as one.
fn group_runs(group: Pid, child: &mut Child) -> bool {
    if let Err(error) = child.try_wait() {
        tracing::debug!(%error, "cannot reap the child");
    }
    test_kill_process_group(group).is_ok()
}

/// Sends `signal` to the process group `group`; a group that is already
/// gone is fine.
fn signal(group: Pid, signal: Signal) {
    if let Err(error) = kill_process_group(group, signal) {
        tracing::debug!(%error, ?signal, "the child's group is gone");
    }
}
