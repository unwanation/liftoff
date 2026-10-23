use crate::domain::ComposeProject;
use crate::error::Error;

/// A file liftoff keeps in an environment's compose project directory
/// (`<data>/<compose project>/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProjectFile {
    /// `compose.yaml`, as sent.
    Compose,
    /// `.env`: the sent env plus the `LIFTOFF_*` variables.
    Env,
    /// `liftoff.override.yaml`, generated.
    Override,
}

impl ProjectFile {
    /// The file name inside the project directory.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Compose => "compose.yaml",
            Self::Env => ".env",
            Self::Override => "liftoff.override.yaml",
        }
    }
}

/// The compose project directories of environments. The adapter creates a directory
/// (`0750`) on first write.
///
/// Not in plan 1.3: the deploy writes three files and removing an environment deletes
/// the directory, and the core does no I/O, so both go through this port.
///
/// # Errors
///
/// `Err` only on I/O failure of the adapter.
pub trait ProjectFiles: Send + Sync {
    /// Writes (replaces) one file.
    fn write(
        &self,
        project: &ComposeProject,
        file: ProjectFile,
        content: &str,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// Reads one file; `None` when absent.
    fn read(
        &self,
        project: &ComposeProject,
        file: ProjectFile,
    ) -> impl Future<Output = Result<Option<String>, Error>> + Send;
    /// Deletes the whole project directory; succeeds when it is absent.
    fn remove_all(
        &self,
        project: &ComposeProject,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}
