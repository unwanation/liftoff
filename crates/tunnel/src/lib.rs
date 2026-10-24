//! The tunnel of `lof expose`.
//!
//! The CLI opens `GET /api/v1/tunnel` as a WebSocket; inside it runs a
//! `yamux` session in which the **server** opens the streams: one per HTTP
//! request that reaches the environment's dev address, an application's
//! WebSocket upgrade included. The server writes the request into its
//! stream as HTTP/1.1, the client connects the stream to
//! `127.0.0.1:<port>` (else `[::1]:<port>`) and relays the bytes both ways.
//!
//! - [`server::TunnelServer`] is the daemon's side: [`serve`] runs one
//!   upgraded connection of an environment (a second one of the same
//!   environment evicts the first with a notice) and [`forward`] hands it
//!   an HTTP request and answers its response, `503` past
//!   `tunnel.max_streams` or without a tunnel. [`serve_as`] keeps the
//!   tunnel under a key of its own (the daemon's environment id) and
//!   reports the end of the handshake.
//! - [`client::TunnelClient`] is the CLI's side: it connects, answers the
//!   server's streams from the local port (`502` when the port refuses),
//!   keeps the connection alive, reconnects with an exponential backoff
//!   capped at 60 seconds and sends [`Frame::Shutdown`] on Ctrl-C; it
//!   stops for good when evicted or when the daemon refuses its session.
//! - [`Bandwidth`] is the token bucket of an environment both directions
//!   of its connection draw from.
//!
//! Besides the `yamux` bytes (binary messages) the WebSocket carries the
//! control [`Frame`]s as JSON text messages.
//!
//! The crate depends on no other crate of the workspace.
//!
//! [`serve`]: server::TunnelServer::serve
//! [`forward`]: server::TunnelServer::forward
//! [`serve_as`]: server::TunnelServer::serve_as

mod bandwidth;
pub mod client;
mod link;
mod mux;
pub mod server;

use serde::{Deserialize, Serialize};

pub use bandwidth::Bandwidth;
pub use client::{Backoff, ClientConfig, ClientState, TunnelClient};
/// A DER certificate, as [`ClientConfig::extra_roots`] takes it.
pub use rustls::pki_types::CertificateDer;
pub use server::{Disconnect, ResponseBody, ServerConfig, TunnelServer};

/// The version of the protocol both sides send in [`Frame::Hello`].
pub const PROTOCOL_VERSION: u32 = 1;

/// A control frame: a JSON text message of the WebSocket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// The handshake. The server sends it first with the environment it
    /// found or created for the port (step 2); the client
    /// answers with the same environment and its own version. Only after
    /// both is the connection the environment's tunnel.
    Hello {
        /// The environment the tunnel serves (`3000`, `alice-3f9a1c2b`).
        environment: String,
        /// [`PROTOCOL_VERSION`] of the sender.
        version: u32,
    },
    /// The client goes away on purpose (Ctrl-C): the server marks the
    /// disconnect at once instead of waiting for `offline_after`.
    Shutdown,
    /// The server's notice to a connection a newer one of the same
    /// environment replaced; the client does not reconnect.
    Evicted,
}

/// Why the tunnel failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TunnelError {
    /// The WebSocket failed or closed before the handshake ended.
    #[error("tunnel connection failed: {0}")]
    Connection(String),
    /// The peer broke the protocol (an unexpected frame, another version).
    #[error("tunnel protocol violated: {0}")]
    Protocol(String),
    /// The daemon refused the upgrade with a `4xx` other than `408` and
    /// `429`: the session is missing, expired or not allowed to open a
    /// tunnel, or the request cannot be served (a port another user
    /// exposes). Retrying does not help.
    #[error("the daemon refused the tunnel: {status}")]
    Refused {
        /// The status of the daemon's answer.
        status: http::StatusCode,
        /// The body of the answer, as text, if it had one.
        detail: Option<String>,
    },
    /// The [`ClientConfig`] cannot work (a URL that is no `ws://` or
    /// `wss://` URL with a host, a token that is no header
    /// value).
    #[error("invalid tunnel configuration: {0}")]
    Invalid(String),
}
