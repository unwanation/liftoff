//! Sessions.

use super::ids::{SessionId, UserId};
use super::spelled::spelled_enum;
use super::timestamp::Timestamp;
use super::token::TokenHash;

/// What a session was opened by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionKind {
    /// `lof login`: 30 days, extended on use.
    Cli,
    /// A browser: 24 hours.
    Browser,
    /// MCP over HTTP: as long as the OAuth token.
    Mcp,
}

spelled_enum!(SessionKind { Cli => "cli", Browser => "browser", Mcp => "mcp" });

/// A login of a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Identifier.
    pub id: SessionId,
    /// The user.
    pub user_id: UserId,
    /// SHA-256 of the session secret (unique).
    pub token_hash: TokenHash,
    /// Machine name the CLI reported.
    pub name: String,
    /// What opened it.
    pub kind: SessionKind,
    /// Expiry; extended on use for `cli`.
    pub expires_at: Timestamp,
    /// Last authenticated request.
    pub last_used_at: Timestamp,
    /// Creation time.
    pub created_at: Timestamp,
}

/// The live single-use login token: its hash and the user whose
/// session it opens. The store keeps at most one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupToken {
    /// SHA-256 of the [`SetupSecret`](super::SetupSecret).
    pub token_hash: TokenHash,
    /// The user the exchange logs in.
    pub user_id: UserId,
    /// When it was minted.
    pub created_at: Timestamp,
}
