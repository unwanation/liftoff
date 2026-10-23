//! The rules of a function's `wasi:keyvalue` (FN-04).
//!
//! how long a key may be and when a write would
//! pass the environment's `functions.kv_max_mb`. The runtime of functions
//! (`liftoff-wasm`) asks them before it writes; any other runtime of
//! functions over a [`Store`](crate::ports::Store) asks the same.
//!
//! A key and its value take `key.len() + value.len()` bytes, what
//! [`Store::kv_usage`](crate::ports::Store::kv_usage) adds up.

/// The longest key of a function's `wasi:keyvalue`, in bytes.
pub const MAX_KV_KEY_BYTES: usize = 256;

/// Why a function's `wasi:keyvalue` call is refused: the function's error,
/// not the daemon's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KvRefusal {
    /// The key is longer than [`MAX_KV_KEY_BYTES`].
    #[error("the key is longer than {MAX_KV_KEY_BYTES} bytes")]
    KeyTooLong,
    /// The environment's keys and values would take `after` bytes, more
    /// than `max` (`functions.kv_max_mb`).
    #[error(
        "the write is refused: the environment's keys and values would take {after} bytes, more than functions.kv_max_mb ({max} bytes)"
    )]
    PastQuota {
        /// The bytes after the write.
        after: u64,
        /// `functions.kv_max_mb`, in bytes.
        max: u64,
    },
}

/// One key of a write: the key, the length of the value it holds now
/// (`None` when it holds none) and the length of the value it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvWrite<'a> {
    /// The key.
    pub key: &'a str,
    /// The length of its value before the write.
    pub old: Option<usize>,
    /// The length of its value after the write.
    pub new: usize,
}

/// The bytes `key` and a value of `value` bytes take.
fn entry_bytes(key: &str, value: usize) -> u64 {
    u64::try_from(key.len().saturating_add(value)).unwrap_or(u64::MAX)
}

/// `key` may be read or written.
///
/// # Errors
///
/// [`KvRefusal::KeyTooLong`] past [`MAX_KV_KEY_BYTES`].
pub const fn check_kv_key(key: &str) -> Result<(), KvRefusal> {
    if key.len() > MAX_KV_KEY_BYTES {
        Err(KvRefusal::KeyTooLong)
    } else {
        Ok(())
    }
}

/// The bytes the environment's keys and values take after `writes`, when
/// they take `usage` now and may take `max`; each key of `writes` appears
/// once. A write either fits whole or is refused whole.
///
/// # Errors
///
/// [`KvRefusal::KeyTooLong`] for a key of `writes` past
/// [`MAX_KV_KEY_BYTES`]; [`KvRefusal::PastQuota`] when the keys and values
/// would take more than `max`.
pub fn check_kv_write<'a>(
    usage: u64,
    writes: impl IntoIterator<Item = KvWrite<'a>>,
    max: u64,
) -> Result<u64, KvRefusal> {
    let mut after = usage;
    for write in writes {
        check_kv_key(write.key)?;
        if let Some(old) = write.old {
            after = after.saturating_sub(entry_bytes(write.key, old));
        }
        after = after.saturating_add(entry_bytes(write.key, write.new));
    }
    if after > max {
        return Err(KvRefusal::PastQuota { after, max });
    }
    Ok(after)
}

#[cfg(test)]
mod tests;
