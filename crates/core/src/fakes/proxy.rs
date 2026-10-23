//! [`RecordingProxy`].

use core::future::ready;
use std::sync::{Arc, Mutex};

use super::log::{Call, CallLog, Faults, lock, scriptable};
use crate::error::Error;
use crate::ports::Proxy;
use crate::proxy_config::ProxyConfig;

/// A proxy that records every configuration it is given.
///
/// [`RecordingProxy::applied`] is the configuration of the last successful
/// `apply`; a scripted failure (`fail_next("apply", ...)`) is logged but
/// does not replace it, like Caddy keeping its old config.
///
/// `ready` answers `Ok` unless scripted (`fail_next("ready", ...)`); it is
/// not logged, so [`RecordingProxy::apply_count`] counts only `apply`.
#[derive(Debug, Clone, Default)]
pub struct RecordingProxy {
    applied: Arc<Mutex<Option<ProxyConfig>>>,
    log: CallLog,
    faults: Faults,
}

scriptable!(RecordingProxy);

impl RecordingProxy {
    /// A fake with its own call log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake writing to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            ..Self::default()
        }
    }

    /// The configuration of the last successful `apply`.
    #[must_use]
    pub fn applied(&self) -> Option<ProxyConfig> {
        lock(&self.applied).clone()
    }

    /// How many times `apply` was called, failed calls included.
    #[must_use]
    pub fn apply_count(&self) -> usize {
        self.log.filter(|call| matches!(call, Call::Proxy(_))).len()
    }
}

impl Proxy for RecordingProxy {
    fn apply(&self, cfg: ProxyConfig) -> impl Future<Output = Result<(), Error>> + Send {
        self.log.push(Call::Proxy(cfg.clone()));
        let result = self.faults.check("apply").map(|()| {
            *lock(&self.applied) = Some(cfg);
        });
        ready(result)
    }

    fn ready(&self) -> impl Future<Output = Result<(), Error>> + Send {
        ready(self.faults.check("ready"))
    }
}
