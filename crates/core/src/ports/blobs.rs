use crate::domain::{ArtifactId, ArtifactPath, BlobHash, VersionNumber};
use crate::error::Error;

/// The files of artifact versions: content-addressed blobs, and the versions
/// that name them.
///
/// A blob is stored once whatever versions use it (ART-13): a version's
/// file is a link `(artifact, version, name) → hash`. A blob no link
/// names any more is removed with the last link that named it
/// ([`Blobs::remove_version`], [`Blobs::remove_artifact`]; BLOB-03).
///
/// The service writes a version's rows first, then puts its blobs, links
/// its files and releases each blob it put ([`Blobs::release`]), whether
/// the links were written or not, and takes the rows and links back when
/// a step fails. A blob a `put` still waits on is never
/// removed (a version is about to link it, while the last link of an
/// older version with the same content goes); once released, a blob no
/// link names goes at once. `liftoff-store` keeps them on disk under
/// `paths.data/artifacts/`; `fakes::FakeBlobs` in memory.
pub trait Blobs: Send + Sync {
    /// Stores `bytes` as the blob `hash` (their SHA-256); a blob already
    /// stored stays as it is. Atomic: an interrupted write leaves nothing
    /// half-written (BLOB-01). A successful `put` waits for its links
    /// until [`Blobs::release`].
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when it cannot be written.
    fn put(&self, hash: &BlobHash, bytes: &[u8]) -> impl Future<Output = Result<(), Error>> + Send;

    /// Ends the wait of one [`Blobs::put`] of `hash` for its links: from
    /// then on the blob goes with the last link that names it, and at once
    /// when none does and no other `put` of it waits (a write whose links
    /// failed leaves no blob). Releasing a hash no `put` waits on changes
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the links of `hash` cannot be read.
    fn release(&self, hash: &BlobHash) -> impl Future<Output = Result<(), Error>> + Send;

    /// Makes the blob `hash` the file `name` of version `version` of
    /// `artifact`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when no blob `hash` is stored;
    /// [`Error::Internal`] when the link cannot be written.
    fn link(
        &self,
        artifact: ArtifactId,
        version: VersionNumber,
        name: &ArtifactPath,
        hash: &BlobHash,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// The bytes of the file `name` of version `version` of `artifact`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when there is no such file (BLOB-04);
    /// [`Error::Internal`] when it cannot be read.
    fn read(
        &self,
        artifact: ArtifactId,
        version: VersionNumber,
        name: &ArtifactPath,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send;

    /// Removes the files of version `version` of `artifact`, and every
    /// blob no other version names; nothing when it has none.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when they cannot be removed.
    fn remove_version(
        &self,
        artifact: ArtifactId,
        version: VersionNumber,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Removes the files of every version of `artifact`, and every blob
    /// no other artifact names; nothing when it has none.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when they cannot be removed.
    fn remove_artifact(
        &self,
        artifact: ArtifactId,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}
