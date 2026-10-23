//! What a function writes to its stdout and stderr (`lof logs` of a function).
//!
//! Each environment keeps its last [`KEPT_LINES`] lines in memory, across
//! the components it activates; a line longer than [`MAX_LINE_BYTES`] is
//! cut there. Nothing is written to disk: the daemon's restart forgets
//! them, as `docker compose logs` forgets a container that is gone.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::stream::{self, Stream, StreamExt as _};
use liftoff_core::Error;
use tokio::io::AsyncWrite;
use tokio::sync::broadcast;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

/// How many lines of one environment are kept.
pub const KEPT_LINES: usize = 1000;

/// The longest line kept; the rest of a longer one is dropped.
pub const MAX_LINE_BYTES: usize = 16 * 1024;

/// How many lines a follower may fall behind before it skips some.
const FOLLOW_BACKLOG: usize = 256;

/// The kept lines of one environment, and the followers of new ones.
#[derive(Debug)]
pub(super) struct FunctionLog {
    lines: Mutex<VecDeque<String>>,
    followers: broadcast::Sender<String>,
}

impl Default for FunctionLog {
    fn default() -> Self {
        Self {
            lines: Mutex::new(VecDeque::with_capacity(KEPT_LINES)),
            followers: broadcast::channel(FOLLOW_BACKLOG).0,
        }
    }
}

impl FunctionLog {
    /// Keeps `line` and hands it to every follower.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the send happens under the lock: a new follower sees each line once"
    )]
    fn push(&self, line: String) {
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == KEPT_LINES {
            drop(lines.pop_front());
        }
        lines.push_back(line.clone());
        // Under the lock, so a new follower sees each line once: in what
        // it is handed or in what it follows. No follower is no error.
        let _followers = self.followers.send(line);
    }

    /// The kept lines, the last `tail` of them only when given, then, with
    /// `follow`, every new line as it comes.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the follower subscribes under the lock, so no line falls between"
    )]
    pub(super) fn lines(
        &self,
        tail: Option<u32>,
        follow: bool,
    ) -> impl Stream<Item = Result<String, Error>> + Send + 'static + use<> {
        let (kept, receiver) = {
            let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
            let skip = tail.map_or(0, |tail| {
                lines
                    .len()
                    .saturating_sub(usize::try_from(tail).unwrap_or(usize::MAX))
            });
            let kept: Vec<String> = lines.iter().skip(skip).cloned().collect();
            (kept, follow.then(|| self.followers.subscribe()))
        };
        let followed = stream::unfold(receiver, async |receiver| {
            let mut receiver = receiver?;
            let line = match receiver.recv().await {
                Ok(line) => line,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    format!("[liftoff: {skipped} lines skipped]")
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            };
            Some((Ok(line), Some(receiver)))
        });
        stream::iter(kept.into_iter().map(Ok)).chain(followed)
    }
}

/// The stdout or stderr of a function's instance: its lines go to the
/// environment's [`FunctionLog`].
#[derive(Debug, Clone)]
pub(super) struct FunctionOutput(pub(super) Arc<FunctionLog>);

impl IsTerminal for FunctionOutput {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for FunctionOutput {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(LineWriter {
            log: Arc::clone(&self.0),
            partial: Vec::new(),
            cut: false,
        })
    }
}

/// Splits what a function writes into lines.
struct LineWriter {
    log: Arc<FunctionLog>,
    /// The line written so far, at most [`MAX_LINE_BYTES`].
    partial: Vec<u8>,
    /// Whether the current line went past [`MAX_LINE_BYTES`].
    cut: bool,
}

impl LineWriter {
    /// Keeps the line written so far.
    fn end_line(&mut self) {
        let mut line = String::from_utf8_lossy(&self.partial).into_owned();
        if line.ends_with('\r') {
            let _return = line.pop();
        }
        self.partial.clear();
        self.cut = false;
        self.log.push(line);
    }

    /// Takes `bytes` in.
    fn take(&mut self, bytes: &[u8]) {
        for chunk in bytes.split_inclusive(|byte| *byte == b'\n') {
            let (text, ends) = match chunk.split_last() {
                Some((&b'\n', text)) => (text, true),
                _ => (chunk, false),
            };
            if !self.cut {
                let room = MAX_LINE_BYTES.saturating_sub(self.partial.len());
                let kept = text.get(..room.min(text.len())).unwrap_or_default();
                self.partial.extend_from_slice(kept);
                self.cut = kept.len() < text.len();
            }
            if ends {
                self.end_line();
            }
        }
    }
}

impl AsyncWrite for LineWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.get_mut().take(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if !this.partial.is_empty() {
            this.end_line();
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        // The instance ended without a newline after its last words.
        if !self.partial.is_empty() {
            self.end_line();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kept lines of `log`.
    async fn kept(log: &FunctionLog, tail: Option<u32>) -> Vec<String> {
        log.lines(tail, false)
            .map(|line| line.unwrap())
            .collect()
            .await
    }

    /// `bytes` written to `log` in one write, the writer then dropped.
    fn write(log: &Arc<FunctionLog>, bytes: &[u8]) {
        let mut writer = LineWriter {
            log: Arc::clone(log),
            partial: Vec::new(),
            cut: false,
        };
        writer.take(bytes);
    }

    mod lines {
        use super::*;

        #[tokio::test]
        async fn one_item_per_line_when_written_at_once() {
            let log = Arc::new(FunctionLog::default());
            write(&log, b"one\r\ntwo\nthree");
            assert_eq!(kept(&log, None).await, vec!["one", "two", "three"]);
        }

        #[tokio::test]
        async fn last_lines_only_when_tail_is_given() {
            let log = Arc::new(FunctionLog::default());
            write(&log, b"one\ntwo\nthree\n");
            assert_eq!(kept(&log, Some(2)).await, vec!["two", "three"]);
        }

        #[tokio::test]
        async fn oldest_dropped_when_more_than_kept_lines_are_written() {
            let log = Arc::new(FunctionLog::default());
            for number in 0..=KEPT_LINES {
                write(&log, format!("{number}\n").as_bytes());
            }
            let lines = kept(&log, None).await;
            assert_eq!(
                (lines.len(), lines.first().cloned()),
                (KEPT_LINES, Some("1".to_owned()))
            );
        }

        #[tokio::test]
        async fn cut_at_max_line_bytes_when_line_is_longer() {
            let log = Arc::new(FunctionLog::default());
            let mut too_long = vec![b'x'; MAX_LINE_BYTES + 10];
            too_long.push(b'\n');
            write(&log, &too_long);
            let lengths: Vec<usize> = kept(&log, None).await.iter().map(String::len).collect();
            assert_eq!(lengths, vec![MAX_LINE_BYTES]);
        }

        #[tokio::test]
        async fn new_lines_follow_when_following() {
            let log = Arc::new(FunctionLog::default());
            write(&log, b"before\n");
            let mut followed = Box::pin(log.lines(None, true));
            write(&log, b"after\n");
            let first_two = vec![
                followed.next().await.unwrap().unwrap(),
                followed.next().await.unwrap().unwrap(),
            ];
            assert_eq!(first_two, vec!["before", "after"]);
        }
    }
}
