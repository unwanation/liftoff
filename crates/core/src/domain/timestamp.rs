//! Points in time.

use time::OffsetDateTime;

/// A point in time (UTC in practice; the clock decides).
pub type Timestamp = OffsetDateTime;
