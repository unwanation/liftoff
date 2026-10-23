use crate::domain::{Limits, SliceName};
use crate::error::Error;

/// Environment slices through `sudo -n systemctl`.
///
/// # Errors
///
/// [`Error::Upstream`] (system `systemd`) when `systemctl` fails.
pub trait Systemd: Send + Sync {
    /// `systemctl set-property --runtime <slice> CPUQuota=<n>% MemoryMax=<m>M
    /// MemorySwapMax=0`; also creates the slice.
    fn set_slice_limits(
        &self,
        slice: &SliceName,
        limits: Limits,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// `systemctl stop <slice>`.
    fn stop_slice(&self, slice: &SliceName) -> impl Future<Output = Result<(), Error>> + Send;
}
