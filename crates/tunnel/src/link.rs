//! The WebSocket of one connection as the two things the tunnel needs:
//! a byte stream for `yamux` (binary messages) and control
//! [`Frame`]s (JSON text messages), either side.
//!
//! A writer task owns the sending half: it sends what `yamux` writes,
//! the frames it is handed and, on the client, the keepalive's pings.
//! The receiving half stays with the caller, which reads it in its own
//! loop and writes the binary messages into [`Link::to_mux`].

use core::time::Duration;
use std::sync::Arc;

use bytes::BytesMut;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::bandwidth::Bucket;
use crate::{Frame, TunnelError};

/// The bytes between the WebSocket and `yamux` buffered in memory, each
/// way; the most one binary message carries.
const BUFFER: usize = 64 * 1024;

/// The largest message (and frame) either side takes: four times what
/// one binary message of this crate carries; the control frames are a
/// few bytes of JSON. A peer that sends more loses the connection.
const MAX_MESSAGE: usize = 4 * BUFFER;

/// The WebSocket limits of both sides: [`MAX_MESSAGE`] in place of
/// tungstenite's 64 MiB a message and 16 MiB a frame.
pub(crate) fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
}

/// How long the writer may take to send the last frame and the close.
const FINISH: Duration = Duration::from_secs(1);

/// The sending half of a connection's WebSocket.
pub(crate) type Sink<S> = SplitSink<WebSocketStream<S>, Message>;

/// The receiving half of a connection's WebSocket.
pub(crate) type Source<S> = SplitStream<WebSocketStream<S>>;

/// What the writer task sends besides the `yamux` bytes.
#[derive(Debug)]
enum Out {
    Frame(Frame),
    Close,
}

/// One connection's WebSocket, split for `yamux` and the frames.
#[derive(Debug)]
pub(crate) struct Link {
    /// Where the caller writes the payload of every binary message.
    pub(crate) to_mux: WriteHalf<DuplexStream>,
    control: mpsc::UnboundedSender<Out>,
    writer: AbortOnDrop,
}

impl Link {
    /// Starts the writer over `sink`, pinging every `keepalive` when set
    /// and drawing every binary message from `bucket` when set. Returns
    /// the link and the IO `yamux` runs over.
    pub(crate) fn start<S>(
        sink: Sink<S>,
        keepalive: Option<Duration>,
        bucket: Option<Arc<Bucket>>,
    ) -> (Self, DuplexStream)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mux_io, ours) = tokio::io::duplex(BUFFER);
        let (from_mux, to_mux) = tokio::io::split(ours);
        let (control, frames) = mpsc::unbounded_channel();
        let writer = AbortOnDrop(tokio::spawn(write(
            sink, from_mux, frames, keepalive, bucket,
        )));
        (
            Self {
                to_mux,
                control,
                writer,
            },
            mux_io,
        )
    }

    /// Sends `last` (when set), closes the WebSocket and waits briefly
    /// for the writer to have done so.
    pub(crate) async fn finish(self, last: Option<Frame>) {
        if let Some(frame) = last {
            drop(self.control.send(Out::Frame(frame)));
        }
        drop(self.control.send(Out::Close));
        let mut writer = self.writer;
        if tokio::time::timeout(FINISH, &mut writer.0).await.is_err() {
            tracing::debug!("the tunnel's last frames did not go out in time");
        }
    }
}

/// How long the side that closes a connection (the client after
/// `Shutdown`, the server after `Evicted`) waits for the other to close
/// too.
const DRAIN: Duration = Duration::from_secs(2);

/// Reads what the other side still sends until it closes too (or
/// [`DRAIN`] passed): a socket closed with unread bytes resets the
/// connection, and the reset can take the last frame (`Shutdown`,
/// `Evicted`), still unread at the other side, with it.
pub(crate) async fn drain<S>(source: &mut Source<S>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let closed = async { while let Some(Ok(_message)) = source.next().await {} };
    if tokio::time::timeout(DRAIN, closed).await.is_err() {
        tracing::debug!("the other side did not close the tunnel in time");
    }
}

/// A task aborted when its handle is dropped.
#[derive(Debug)]
pub(crate) struct AbortOnDrop(pub(crate) JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The writer task: `yamux`'s bytes as binary messages, the frames as
/// text, a ping every `keepalive`.
async fn write<S>(
    mut sink: Sink<S>,
    mut from_mux: ReadHalf<DuplexStream>,
    mut frames: mpsc::UnboundedReceiver<Out>,
    keepalive: Option<Duration>,
    bucket: Option<Arc<Bucket>>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut pings = keepalive.map(|every| {
        let mut pings = tokio::time::interval(every);
        pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The first ping one interval from now, not at once.
        pings.reset();
        pings
    });
    let mut buffer = BytesMut::with_capacity(BUFFER);
    loop {
        let message = tokio::select! {
            biased;
            out = frames.recv() => match out {
                Some(Out::Frame(frame)) => text(&frame),
                Some(Out::Close) | None => break,
            },
            read = from_mux.read_buf(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(_) => Message::Binary(buffer.split().freeze()),
            },
            _ = tick(pings.as_mut()) => Message::Ping(bytes::Bytes::new()),
        };
        if buffer.capacity() < BUFFER {
            buffer.reserve(BUFFER);
        }
        if let (Some(bucket), Message::Binary(bytes)) = (&bucket, &message) {
            bucket.take(bytes.len()).await;
        }
        if let Err(error) = sink.send(message).await {
            tracing::debug!(%error, "the tunnel's WebSocket stopped taking messages");
            return;
        }
    }
    if let Err(error) = sink.close().await {
        tracing::debug!(%error, "the tunnel's WebSocket did not close cleanly");
    }
}

/// The next tick of the keepalive; never without one.
async fn tick(pings: Option<&mut tokio::time::Interval>) -> Instant {
    match pings {
        Some(pings) => pings.tick().await,
        None => core::future::pending().await,
    }
}

/// `frame` as a text message.
pub(crate) fn text(frame: &Frame) -> Message {
    // A `Frame` is plain data: serialising it cannot fail.
    Message::text(serde_json::to_string(frame).unwrap_or_default())
}

/// The frame a text message carries.
pub(crate) fn parse(text: &str) -> Result<Frame, TunnelError> {
    serde_json::from_str(text).map_err(|error| TunnelError::Protocol(format!("bad frame: {error}")))
}

/// The next control frame of `source`, skipping pings and pongs; the
/// handshake reads the `Hello`s with it.
pub(crate) async fn next_frame<S>(source: &mut Source<S>) -> Result<Frame, TunnelError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    loop {
        match source.next().await {
            Some(Ok(Message::Text(text))) => return parse(text.as_str()),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Binary(_) | Message::Frame(_))) => {
                return Err(TunnelError::Protocol(
                    "bytes before the handshake".to_owned(),
                ));
            }
            Some(Ok(Message::Close(_))) | None => {
                return Err(TunnelError::Connection(
                    "closed during the handshake".to_owned(),
                ));
            }
            Some(Err(error)) => return Err(TunnelError::Connection(error.to_string())),
        }
    }
}
