//! [`RecordingSystemd`].

use core::future::ready;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::log::{Call, CallLog, Faults, SystemdCall, lock, scriptable};
use crate::domain::{Limits, SliceName};
use crate::error::Error;
use crate::ports::Systemd;

/// Records [`Systemd`] calls and remembers the limits of each slice.
#[derive(Debug, Clone, Default)]
pub struct RecordingSystemd {
    slices: Arc<Mutex<BTreeMap<SliceName, Limits>>>,
    log: CallLog,
    faults: Faults,
}

scriptable!(RecordingSystemd);

impl RecordingSystemd {
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

    /// The limits last set on `slice`, while it is not stopped.
    #[must_use]
    pub fn limits(&self, slice: &SliceName) -> Option<Limits> {
        lock(&self.slices).get(slice).copied()
    }

    /// The systemd calls received so far, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<SystemdCall> {
        self.log
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Systemd(call) => Some(call),
                _ => None,
            })
            .collect()
    }
}

impl Systemd for RecordingSystemd {
    fn set_slice_limits(
        &self,
        slice: &SliceName,
        limits: Limits,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.log.push(Call::Systemd(SystemdCall::SetSliceLimits {
            slice: slice.clone(),
            limits,
        }));
        ready(self.faults.check("set_slice_limits").map(|()| {
            let _previous: Option<Limits> = lock(&self.slices).insert(slice.clone(), limits);
        }))
    }

    fn stop_slice(&self, slice: &SliceName) -> impl Future<Output = Result<(), Error>> + Send {
        self.log
            .push(Call::Systemd(SystemdCall::StopSlice(slice.clone())));
        ready(self.faults.check("stop_slice").map(|()| {
            let _removed: Option<Limits> = lock(&self.slices).remove(slice);
        }))
    }
}
