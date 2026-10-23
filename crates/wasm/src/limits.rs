//! [`Limits`]: what one module may use: the memory cap (a
//! `StoreLimits` on the module's store) and the call timeout (epoch
//! interruption), from `[modules]` of the daemon's configuration. The
//! fixed caps below bound what a module makes the daemon hold for it
//! outside its own memory.

use core::time::Duration;

/// What one module may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most linear memory the module may grow to, in bytes. A module
    /// has one linear memory, and a handful of tables and core instances
    /// (what a component built for `wasm32-wasip2` needs), so this is the
    /// module's memory.
    pub max_memory_bytes: usize,
    /// How long one call into the module may run (`modules.call_timeout`).
    pub call_timeout: Duration,
}

/// The longest key of `store-get`/`store-set`, in bytes; a longer one
/// traps the call.
pub const MAX_KEY_BYTES: usize = 256;

/// The largest value of `store-set`, in bytes; a larger one traps the
/// call.
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;

/// The longest `log` line, in bytes, after its control characters are
/// escaped; a longer one is cut there and ends in `…`.
pub const MAX_LOG_BYTES: usize = 4096;

/// How many `log` lines one call into a module (a tool, a tick, a
/// migration) may write; the rest of that call's lines are dropped, and
/// the drop is logged once.
pub const MAX_LOG_LINES_PER_CALL: usize = 100;

/// How many names a module may keep scheduled at once; `schedule` of one
/// more is ignored (and logged).
pub const MAX_SCHEDULES: usize = 32;

/// The longest name `schedule` accepts, in bytes.
pub const MAX_SCHEDULE_NAME_BYTES: usize = 64;

/// The shortest period `schedule` accepts, in seconds; a shorter one
/// (except `0`, which stops the name) is ignored (and logged).
pub const MIN_PERIOD_SECS: u32 = 10;

/// Core instances, tables and table elements one module may create: a
/// component of `wasm32-wasip2` uses a few of each (its own module plus
/// the adapter shims `wit-component` adds).
pub(crate) const MAX_INSTANCES: usize = 32;
/// See [`MAX_INSTANCES`].
pub(crate) const MAX_TABLES: usize = 32;
/// See [`MAX_INSTANCES`].
pub(crate) const MAX_TABLE_ELEMENTS: usize = 100_000;

/// Handles one module instance may hold in the host at once.
///
/// Streams, pollables, `wasi:http` fields, requests and responses each
/// cost daemon memory outside the module's own, so one more traps the
/// call (MOD-10). A component of `wasm32-wasip2` needs a handful.
pub const MAX_HOST_RESOURCES: usize = 1024;

/// The most bytes one set of `wasi:http` fields (headers or trailers) of a
/// module may hold; more is the module's error.
pub const MAX_HTTP_FIELDS_BYTES: usize = 64 * 1024;

/// How many `wasi:http` requests of one module instance may be open at
/// once; one more is refused (`connection-limit-reached`), so a module
/// cannot hold the daemon's sockets.
pub const MAX_HTTP_REQUESTS_IN_FLIGHT: usize = 16;
