use crate::error::Error;

/// The module registry (`[modules] registry`): its
/// `index.json` and the files of its releases.
///
/// Nothing it answers is trusted: [`ModuleService`](crate::service::ModuleService)
/// checks every release against the pinned
/// [`RegistryKey`](crate::module::RegistryKey) before anything is kept.
/// The daemon's source fetches over HTTPS; tests use `FakeModuleSource`.
pub trait ModuleSource: Send + Sync {
    /// The text of `index.json`.
    ///
    /// # Errors
    ///
    /// [`Error::Upstream`] when the registry cannot be reached or answers
    /// an error.
    fn index(&self) -> impl Future<Output = Result<String, Error>> + Send;

    /// The bytes at `url` (a release's `.wasm`).
    ///
    /// # Errors
    ///
    /// [`Error::Upstream`] when the registry cannot be reached or answers
    /// an error.
    fn download(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, Error>> + Send;
}
