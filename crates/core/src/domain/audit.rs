//! The audit log.

use super::ids::{AuditId, UserId};
use super::timestamp::Timestamp;

/// One recorded action. Never carries a secret value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audit {
    /// Identifier (ULID: creation order).
    pub id: AuditId,
    /// Who acted; `None` for CI and the system.
    pub actor_user_id: Option<UserId>,
    /// What was done, e.g. `secret.set`.
    pub action: String,
    /// On what, e.g. `project:shop/DATABASE_URL`.
    pub resource: String,
    /// When.
    pub at: Timestamp,
}
