//! `deploy` in a function project:
//! `[function] build` of `liftoff.toml` runs at the top of the working
//! tree, `[function] component` is checked to be a `wasi:http` component
//! (`liftoff_wasm::functions::check`, the daemon's own first check) and
//! goes up as the multipart field `component` of `POST /deploys`, with
//! the upload's progress on stderr; then the CLI waits for the deploy
//! (`--wait` is the default for functions).
//!
//! The build is the person's own manifest string, run like the
//! `[dev] command` of `lof dev`: `sh -c`, never from a server. Its stdout
//! and stderr both go to the CLI's stderr (the CLI owns stdout), and a
//! build that fails ends the run with 1 before anything is uploaded
//! (FDEP-07); so does a component that is not a function (FDEP-08).

use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_core::Stream;
use liftoff_core::domain::ComponentHash;
use liftoff_core::manifest::{Deployable, ProjectManifest, RepoPath};
use liftoff_wasm::functions::{FunctionError, check};
use reqwest::Body;
use rustix::process::Pid;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::{Child, Command};
use tokio::signal::unix::SignalKind;

use super::deploys::queued;
use super::dev::{group_of, stop_group, watching};
use super::{Ctx, ProjectRef, resolve};
use crate::Sleeper;
use crate::error::CliError;

/// The manifest of a project.
const MANIFEST: &str = "liftoff.toml";

/// The size of one piece of the upload.
const CHUNK: usize = 256 * 1024;

/// How often the upload's progress is shown.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

/// What `[function]` of a manifest says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FunctionBuild {
    /// `build`, run with `sh -c`.
    pub(super) build: String,
    /// `component`, relative to the top of the working tree.
    pub(super) component: RepoPath,
}

/// `[function]` of the manifest at `root`; `None` without a manifest or
/// for a project of another kind.
///
/// # Errors
///
/// [`CliError::Usage`] for a manifest with a mistake,
/// [`CliError::Local`] when it cannot be read.
pub(super) fn function_of(root: &Path) -> Result<Option<FunctionBuild>, CliError> {
    let text = match std::fs::read_to_string(root.join(MANIFEST)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CliError::Local(format!("cannot read {MANIFEST}: {error}"))),
    };
    let manifest = ProjectManifest::parse(&text)
        .map_err(|error| CliError::Usage(format!("{MANIFEST}: {error}")))?;
    Ok(match manifest.deploy {
        Deployable::Function { build, component } => Some(FunctionBuild { build, component }),
        Deployable::Service { .. } => None,
    })
}

/// Builds, checks and uploads the component of `function` from the
/// working tree at `root` to the environment of `ctx`, then waits for
/// the deploy.
pub(super) async fn deploy<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    root: &Path,
    function: FunctionBuild,
    project: ProjectRef,
) -> Result<(), CliError> {
    let client = ctx.client()?;
    build(ctx, root, &function.build).await?;
    let shown = function.component.as_str().to_owned();
    let path = root.join(&shown);
    let component = std::fs::read(&path)
        .map_err(|error| CliError::Local(format!("cannot read {shown}: {error}")))?;
    check(&component).map_err(|error| {
        let reason = match error {
            FunctionError::Load { reason } => reason,
            other => other.to_string(),
        };
        CliError::Local(format!(
            "{shown} is not a wasi:http component: {reason}; nothing was uploaded"
        ))
    })?;
    let project = resolve(&client, project).await?;
    let environment = ctx.env_name();
    let form = Form::new(
        &[("project", &project), ("environment", &environment)],
        &component,
    );
    let total = form.total();
    ctx.tell(&format!("uploading {shown} ({})\n", size(total)))?;
    let content_type = form.content_type();
    let sent = Arc::clone(&form.sent);
    let upload = client.send_body(
        reqwest::Method::POST,
        "/deploys",
        &content_type,
        Body::wrap_stream(form),
    );
    tokio::pin!(upload);
    let mut ticks = tokio::time::interval(PROGRESS_EVERY);
    ticks.reset();
    let mut shown_sent = 0;
    let reply = loop {
        tokio::select! {
            reply = &mut upload => break reply?,
            _ = ticks.tick() => {
                let now = sent.load(Ordering::Relaxed);
                if now != shown_sent && now < total {
                    shown_sent = now;
                    ctx.tell(&format!("uploaded {} of {}\n", size(now), size(total)))?;
                }
            }
        }
    };
    ctx.tell(&format!("uploaded {shown}\n"))?;
    queued(ctx, &client, &reply, true).await
}

/// `bytes` as a person reads them.
#[expect(
    clippy::float_arithmetic,
    reason = "a size shown with one decimal; at most 50 MiB"
)]
fn size(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    let shown = f64::from(u32::try_from(bytes).unwrap_or(u32::MAX));
    if shown >= KIB * KIB {
        format!("{:.1} MiB", shown / (KIB * KIB))
    } else if shown >= KIB {
        format!("{:.0} KiB", (shown / KIB).ceil())
    } else {
        format!("{bytes} B")
    }
}

/// Runs `line` with `sh -c` at `root` and passes what it prints on
/// either stream to the CLI's stderr; a failure (or Ctrl-C) ends the run.
///
/// The build leads its own process group, as the `[dev] command` does:
/// Ctrl-C, and `SIGTERM` or `SIGHUP` of the CLI, end the whole group
/// (`SIGTERM`, then `SIGKILL` after the grace period of `lof dev`), and
/// so does the shell's exit, so what the build left running in the
/// background cannot keep its pipes, and the run, open.
async fn build<S: Sleeper>(ctx: &mut Ctx<'_, S>, root: &Path, line: &str) -> Result<(), CliError> {
    ctx.tell(&format!("building: {line}\n"))?;
    // Watched before there is a child: either would end the CLI and leave
    // the group running.
    let mut term = watching(SignalKind::terminate())?;
    let mut hangup = watching(SignalKind::hangup())?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(line)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| CliError::Local(format!("cannot start `{line}`: {error}")))?;
    // Taken now: once the shell has been waited for, it has no id.
    let group = group_of(&child);
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let cancel = ctx.env.cancel.clone();
    let mut out_buf = vec![0_u8; 8192];
    let mut err_buf = vec![0_u8; 8192];
    let mut status = None;
    while status.is_none() || out.is_some() || err.is_some() {
        tokio::select! {
            read = read_some(out.as_mut(), &mut out_buf), if out.is_some() => {
                match read {
                    Some(text) => ctx.tell(&String::from_utf8_lossy(text))?,
                    None => out = None,
                }
            }
            read = read_some(err.as_mut(), &mut err_buf), if err.is_some() => {
                match read {
                    Some(text) => ctx.tell(&String::from_utf8_lossy(text))?,
                    None => err = None,
                }
            }
            exited = child.wait(), if status.is_none() => {
                status = Some(exited);
                stop_group(group, &mut child).await;
            }
            () = cancel.cancelled() => return interrupted(group, &mut child).await,
            _ = term.recv() => return interrupted(group, &mut child).await,
            _ = hangup.recv() => return interrupted(group, &mut child).await,
        }
    }
    let status = status
        .unwrap_or_else(|| Err(std::io::Error::other("the build was not waited for")))
        .map_err(|error| CliError::Local(format!("cannot wait for `{line}`: {error}")))?;
    if status.success() {
        return Ok(());
    }
    let ended = status.code().map_or_else(
        || format!("ended with {status}"),
        |code| format!("ended with exit code {code}"),
    );
    Err(CliError::Local(format!(
        "the build `{line}` {ended}; nothing was uploaded"
    )))
}

/// Ends the build's group and the run.
async fn interrupted(group: Option<Pid>, child: &mut Child) -> Result<(), CliError> {
    stop_group(group, child).await;
    Err(CliError::Interrupted)
}

/// The next bytes of `pipe` into `buf`; `None` at its end or on an error
/// (the build's exit status tells what happened).
async fn read_some<'b, R: AsyncRead + Unpin>(
    pipe: Option<&mut R>,
    buf: &'b mut [u8],
) -> Option<&'b [u8]> {
    let pipe = pipe?;
    match pipe.read(buf).await {
        Ok(0) | Err(_) => None,
        Ok(read) => buf.get(..read),
    }
}

/// The `multipart/form-data` body of a function deploy: the text fields,
/// then the file field `component`, as a stream of pieces that counts
/// what has been sent.
struct Form {
    boundary: String,
    pieces: VecDeque<Vec<u8>>,
    total: usize,
    sent: Arc<AtomicUsize>,
}

impl Form {
    /// The form of `fields` and `component`. The boundary is made of the
    /// component's hash, which the component cannot contain, and the
    /// fields are names, which hold no line break.
    fn new(fields: &[(&str, &str)], component: &[u8]) -> Self {
        let boundary = format!("liftoff-{}", ComponentHash::of(component));
        let head: String = fields
            .iter()
            .map(|(name, value)| {
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
            })
            .chain(core::iter::once(format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"component\"; filename=\"component.wasm\"\r\nContent-Type: application/wasm\r\n\r\n"
            )))
            .collect();
        let mut pieces = VecDeque::from([head.into_bytes()]);
        pieces.extend(component.chunks(CHUNK).map(<[u8]>::to_vec));
        pieces.push_back(format!("\r\n--{boundary}--\r\n").into_bytes());
        let total = pieces.iter().map(Vec::len).sum();
        Self {
            boundary,
            pieces,
            total,
            sent: Arc::default(),
        }
    }

    fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    const fn total(&self) -> usize {
        self.total
    }
}

impl Stream for Form {
    type Item = Result<Vec<u8>, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let piece = this.pieces.pop_front();
        if let Some(piece) = &piece {
            let _before = this.sent.fetch_add(piece.len(), Ordering::Relaxed);
        }
        Poll::Ready(piece.map(Ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod size {
        use super::*;

        #[test]
        fn mebibytes_with_one_decimal_when_large() {
            assert_eq!(size(12 * 1024 * 1024 + 512 * 1024), "12.5 MiB");
        }

        #[test]
        fn kibibytes_rounded_up_when_small() {
            assert_eq!(size(1025), "2 KiB");
        }
    }

    mod form {
        use super::*;

        #[test]
        fn total_counts_every_byte_when_built() {
            let form = Form::new(&[("project", "shop")], &vec![1; CHUNK + 1]);
            let pieces: usize = form.pieces.iter().map(Vec::len).sum();
            assert_eq!(form.total(), pieces);
        }

        #[test]
        fn splits_component_into_chunks_when_large() {
            let form = Form::new(&[], &vec![1; CHUNK + 1]);
            // The head, two pieces of the component, the end.
            assert_eq!(form.pieces.len(), 4);
        }
    }
}
