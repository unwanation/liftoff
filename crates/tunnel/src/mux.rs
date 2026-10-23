//! The multiplexer: `yamux` over the bytes of the WebSocket, the server
//! the only side that opens streams.
//!
//! [`Mux::server`] is the daemon's end; it opens one stream per request
//! and refuses one past `max_streams` (TUN-03). [`Inbound::client`] is
//! the CLI's end; it takes the streams the server opens. Backpressure is
//! `yamux`'s window: a writer whose reader does not read waits.
//!
//! Each end runs a driver task that owns the `yamux` connection and polls
//! it all the time (the connection makes progress only while it is
//! polled); the handles talk to it over channels.

use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt as _, TokioAsyncReadCompatExt as _};

/// The receive window `yamux` grants each stream at first (256 KiB);
/// `yamux` wants a connection window of at least this much per stream.
const STREAM_WINDOW: usize = 256 * 1024;

/// Streams the client's `yamux` accepts on one connection: more than any
/// server opens. The client is the developer's own machine; the daemon's
/// memory is bounded by [`server_streams`].
const CLIENT_STREAMS: usize = 512;

/// Streams the server's `yamux` keeps for `max_streams` slots: a quarter
/// more (at least four), room for streams whose slot is already free
/// while `yamux` still closes them. With the window of [`config`] this
/// bounds what one connection makes the daemon buffer: 20 MiB for the
/// default 64 streams.
fn server_streams(max_streams: usize) -> usize {
    max_streams.saturating_add(max_streams.div_ceil(4).max(4))
}

/// Why [`Mux::open`] opened no stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MuxError {
    /// `max_streams` streams are open on this connection.
    #[error("the tunnel has as many streams open as it may")]
    TooManyStreams,
    /// The connection is gone.
    #[error("the tunnel connection is closed")]
    Closed,
}

/// A reply to one request to open a stream.
type Opened = oneshot::Sender<Result<yamux::Stream, MuxError>>;

/// The server's end of the multiplexer. Dropping it closes the
/// connection.
#[derive(Debug)]
pub(crate) struct Mux {
    opens: mpsc::UnboundedSender<Opened>,
    slots: Arc<Semaphore>,
}

impl Mux {
    /// Runs the multiplexer's server end over `io` (its driver on the
    /// current runtime) with at most `max_streams` streams at once.
    pub(crate) fn server<S>(io: S, max_streams: u32) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (opens, requests) = mpsc::unbounded_channel();
        let streams = usize::try_from(max_streams).unwrap_or(usize::MAX);
        let yamux_streams = server_streams(streams);
        let connection =
            yamux::Connection::new(io.compat(), config(yamux_streams), yamux::Mode::Server);
        tokio::spawn(drive(connection, Role::Server { requests }));
        Self {
            opens,
            slots: Arc::new(Semaphore::new(streams.min(Semaphore::MAX_PERMITS))),
        }
    }

    /// Opens a stream to the client. The slot it takes is free again when
    /// the returned stream is dropped (or its [`Slot`] taken and dropped).
    pub(crate) async fn open(&self) -> Result<MuxStream, MuxError> {
        let slot = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_full| MuxError::TooManyStreams)?;
        let (reply, opened) = oneshot::channel();
        self.opens.send(reply).map_err(|_gone| MuxError::Closed)?;
        let stream = opened.await.map_err(|_gone| MuxError::Closed)??;
        Ok(MuxStream {
            io: stream.compat(),
            slot: Some(Slot(slot)),
        })
    }
}

/// The client's end of the multiplexer: the streams the server opens.
/// Dropping it closes the connection.
#[derive(Debug)]
pub(crate) struct Inbound {
    streams: mpsc::UnboundedReceiver<yamux::Stream>,
    /// Dropped with the handle, which tells the driver to close.
    _alive: oneshot::Sender<()>,
}

impl Inbound {
    /// Runs the multiplexer's client end over `io`.
    pub(crate) fn client<S>(io: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (accepted, streams) = mpsc::unbounded_channel();
        let (alive, gone) = oneshot::channel();
        let connection =
            yamux::Connection::new(io.compat(), config(CLIENT_STREAMS), yamux::Mode::Client);
        tokio::spawn(drive(connection, Role::Client { accepted, gone }));
        Self {
            streams,
            _alive: alive,
        }
    }

    /// The next stream the server opened; `None` once the connection is
    /// gone.
    pub(crate) async fn next(&mut self) -> Option<MuxStream> {
        let stream = self.streams.recv().await?;
        Some(MuxStream {
            io: stream.compat(),
            slot: None,
        })
    }
}

/// The `yamux` configuration of either end with room for `streams`: the
/// connection's receive window is the smallest `yamux` allows for them,
/// [`STREAM_WINDOW`] per stream.
fn config(streams: usize) -> yamux::Config {
    let mut config = yamux::Config::default();
    // The stream count first: `yamux` checks the window against it.
    config
        .set_max_num_streams(streams)
        .set_max_connection_receive_window(Some(connection_window(streams)));
    config
}

/// The receive window of a connection with room for `streams`.
const fn connection_window(streams: usize) -> usize {
    streams.saturating_mul(STREAM_WINDOW)
}

/// What the driver does besides polling the connection.
enum Role {
    /// Opens the streams [`Mux::open`] asks for; refuses inbound ones.
    Server {
        requests: mpsc::UnboundedReceiver<Opened>,
    },
    /// Hands inbound streams to [`Inbound::next`] until it is dropped.
    Client {
        accepted: mpsc::UnboundedSender<yamux::Stream>,
        gone: oneshot::Receiver<()>,
    },
}

/// Polls `connection` until it ends or its handle is dropped (then it
/// closes the connection).
async fn drive<T>(mut connection: yamux::Connection<T>, mut role: Role)
where
    T: futures_util::AsyncRead + futures_util::AsyncWrite + Unpin,
{
    let mut pending: VecDeque<Opened> = VecDeque::new();
    let mut closing = false;
    let ended = poll_fn(|cx| {
        if !closing {
            closing = match &mut role {
                Role::Server { requests } => poll_requests(requests, &mut pending, cx),
                Role::Client { gone, .. } => Pin::new(gone).poll(cx).is_ready(),
            };
        }
        if closing {
            return connection.poll_close(cx).map(|_closed| ());
        }
        while let Some(front) = pending.front() {
            if front.is_closed() {
                drop(pending.pop_front());
                continue;
            }
            match connection.poll_new_outbound(cx) {
                Poll::Ready(Ok(stream)) => {
                    if let Some(reply) = pending.pop_front() {
                        // A caller that went away drops the stream: closed.
                        drop(reply.send(Ok(stream)));
                    }
                }
                Poll::Ready(Err(yamux::ConnectionError::TooManyStreams)) => {
                    // `yamux` still counts streams that are closing: this
                    // request waits for none of them and gets a `503`.
                    if let Some(reply) = pending.pop_front() {
                        drop(reply.send(Err(MuxError::TooManyStreams)));
                    }
                }
                Poll::Ready(Err(error)) => {
                    tracing::debug!(%error, "the tunnel refused a new stream");
                    return Poll::Ready(());
                }
                Poll::Pending => break,
            }
        }
        loop {
            match connection.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(stream))) => {
                    if let Role::Client { accepted, .. } = &role {
                        drop(accepted.send(stream));
                    }
                    // The server takes no stream from the client: dropped.
                }
                Poll::Ready(Some(Err(error))) => {
                    tracing::debug!(%error, "the tunnel connection failed");
                    return Poll::Ready(());
                }
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => return Poll::Pending,
            }
        }
    });
    ended.await;
    for reply in pending {
        drop(reply.send(Err(MuxError::Closed)));
    }
}

/// Takes the open requests that arrived; `true` once [`Mux`] is dropped.
fn poll_requests(
    requests: &mut mpsc::UnboundedReceiver<Opened>,
    pending: &mut VecDeque<Opened>,
    cx: &mut Context<'_>,
) -> bool {
    loop {
        match requests.poll_recv(cx) {
            Poll::Ready(Some(reply)) => pending.push_back(reply),
            Poll::Ready(None) => return true,
            Poll::Pending => return false,
        }
    }
}

/// One of the `max_streams` slots of a connection, free again when
/// dropped.
#[derive(Debug)]
pub(crate) struct Slot(#[expect(dead_code, reason = "held for its drop")] OwnedSemaphorePermit);

/// One stream of the multiplexer, as tokio IO.
#[derive(Debug)]
pub(crate) struct MuxStream {
    io: Compat<yamux::Stream>,
    slot: Option<Slot>,
}

impl MuxStream {
    /// Takes the stream's slot so that it ends with something else than
    /// the stream (the body of the answer, a spliced upgrade).
    pub(crate) const fn take_slot(&mut self) -> Option<Slot> {
        self.slot.take()
    }
}

impl AsyncRead for MuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for MuxStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream, duplex};

    use super::*;

    /// How long one exchange may take before the test fails instead of
    /// hanging.
    const EXCHANGE: Duration = Duration::from_secs(10);

    /// Both ends of the multiplexer over an in-memory pipe.
    fn pair(max_streams: u32) -> (Mux, Inbound) {
        let (server, client): (DuplexStream, DuplexStream) = duplex(64 * 1024);
        (Mux::server(server, max_streams), Inbound::client(client))
    }

    async fn accepted(inbound: &mut Inbound) -> MuxStream {
        tokio::time::timeout(EXCHANGE, inbound.next())
            .await
            .expect("the client takes the stream in time")
            .expect("the connection is open")
    }

    mod streams {
        use super::*;

        // TC: TUN-01
        #[tokio::test]
        async fn carries_bytes_unchanged_both_ways_when_the_server_opens_a_stream() {
            let (mux, mut inbound) = pair(64);
            let request = b"GET /path?q=1 HTTP/1.1\r\nhost: web-dev.apps.test\r\n\r\n";
            let answer: Vec<u8> = (0..=255_u8).cycle().take(70_000).collect();

            let mut opened = mux.open().await.expect("a stream opens");
            opened.write_all(request).await.expect("the server writes");
            let mut taken = accepted(&mut inbound).await;
            let mut received = vec![0_u8; request.len()];
            taken
                .read_exact(&mut received)
                .await
                .expect("the client reads");
            taken.write_all(&answer).await.expect("the client writes");
            taken.shutdown().await.expect("the client closes its half");
            let mut back = Vec::new();
            tokio::time::timeout(EXCHANGE, opened.read_to_end(&mut back))
                .await
                .expect("the answer arrives in time")
                .expect("the server reads");

            assert_eq!(received, request.to_vec(), "the request crosses unchanged");
            assert_eq!(back, answer, "the answer crosses byte for byte");
        }

        // TC: TUN-02
        #[tokio::test]
        #[expect(
            clippy::significant_drop_tightening,
            reason = "the test holds its streams, and their slots, to its end"
        )]
        async fn keeps_streams_apart_when_many_are_open_at_once() {
            let (mux, mut inbound) = pair(64);
            let echo = tokio::spawn(async move {
                while let Some(mut stream) = inbound.next().await {
                    tokio::spawn(async move {
                        let (mut read, mut write) = tokio::io::split(&mut stream);
                        let _copied = tokio::io::copy(&mut read, &mut write).await;
                        let _closed = write.shutdown().await;
                    });
                }
            });
            let mux = Arc::new(mux);
            let mut exchanges = Vec::new();
            for tag in 0..8_u8 {
                let mux = Arc::clone(&mux);
                exchanges.push(tokio::spawn(async move {
                    let payload = vec![tag; 100_000];
                    let mut stream = mux.open().await.expect("a stream opens");
                    let (mut read, mut write) = tokio::io::split(&mut stream);
                    let writing = async {
                        write.write_all(&payload).await.expect("the server writes");
                        write.shutdown().await.expect("the server closes its half");
                    };
                    let mut back = Vec::new();
                    let reading = read.read_to_end(&mut back);
                    let ((), read_result) = tokio::join!(writing, reading);
                    read_result.expect("the server reads");
                    (tag, back)
                }));
            }

            for exchange in exchanges {
                let (tag, back) = tokio::time::timeout(EXCHANGE, exchange)
                    .await
                    .expect("every exchange ends in time")
                    .expect("no exchange panics");
                assert_eq!(
                    back,
                    vec![tag; 100_000],
                    "stream {tag} gets its own bytes only"
                );
            }
            echo.abort();
        }
    }

    mod limits {
        use super::*;

        // TC: TUN-03
        #[tokio::test]
        async fn refuses_a_stream_when_max_streams_are_open() {
            let (mux, _inbound) = pair(2);
            let _first = mux.open().await.expect("the first stream opens");
            let _second = mux.open().await.expect("the second stream opens");

            let third = mux.open().await;

            assert_eq!(third.err(), Some(MuxError::TooManyStreams));
        }

        // TC: TUN-03
        #[tokio::test]
        #[expect(
            clippy::significant_drop_tightening,
            reason = "the test holds its streams, and their slots, to its end"
        )]
        async fn opens_a_stream_when_an_open_one_was_dropped() {
            let (mux, _inbound) = pair(1);
            {
                let _first = mux.open().await.expect("the first stream opens");
            }

            let again = mux.open().await;

            assert!(again.is_ok(), "the slot is free again: {again:?}");
        }
    }

    mod memory {
        use super::*;

        // TC: TUN-03 (the daemon's memory per tunnel is bounded)
        #[test]
        fn bounds_the_receive_window_by_max_streams_when_the_server_runs() {
            assert_eq!(connection_window(server_streams(64)), 20 * 1024 * 1024);
            assert_eq!(connection_window(server_streams(1)), 5 * STREAM_WINDOW);
        }
    }

    mod backpressure {
        use super::*;

        // backpressure through the yamux window (no catalogue ID;
        // the window keeps TUN-01's large answers within bounds).
        #[tokio::test]
        async fn holds_the_writer_when_the_reader_does_not_read() {
            let (mux, mut inbound) = pair(64);
            let mut opened = mux.open().await.expect("a stream opens");
            opened.write_all(b"x").await.expect("the server writes");
            let mut taken = accepted(&mut inbound).await;
            let big = vec![7_u8; 4 * 1024 * 1024];

            let held =
                tokio::time::timeout(Duration::from_millis(300), opened.write_all(&big)).await;
            assert!(
                held.is_err(),
                "4 MiB do not fit into the window of a reader that does not read"
            );

            let mut sink = Vec::new();
            let (written, read) = tokio::join!(
                async {
                    opened.write_all(&big).await?;
                    opened.shutdown().await
                },
                taken.read_to_end(&mut sink),
            );
            written.expect("the writer goes on once the reader reads");
            read.expect("the reader reads");
            assert!(
                sink.len() > big.len(),
                "the bytes arrive once the reader reads"
            );
        }
    }

    mod closing {
        use super::*;

        // TC: TUN-04 (the client notices a broken connection and reconnects)
        #[tokio::test]
        #[expect(
            clippy::significant_drop_tightening,
            reason = "the test holds its streams, and their slots, to its end"
        )]
        async fn ends_the_inbound_streams_when_the_server_end_is_gone() {
            let mut inbound = {
                let (_mux, inbound) = pair(64);
                inbound
            };

            let next = tokio::time::timeout(EXCHANGE, inbound.next())
                .await
                .expect("the client notices in time");

            assert!(next.is_none(), "no stream comes from a closed connection");
        }
    }
}
