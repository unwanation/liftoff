//! The [`Blobs`] contract as one check per rule, generic over the
//! implementation.
//!
//! `FakeBlobs` and the blobs on disk of `liftoff-store` (in both of
//! their link modes, BLOB-02) are held to the same rules.
//!
//! [`blobs_contract_tests!`](crate::blobs_contract_tests) turns every
//! check into its own `#[tokio::test]`:
//!
//! ```ignore
//! // in liftoff-store's tests:
//! mod contract {
//!     liftoff_core::blobs_contract_tests!(async || {
//!         let dir = tempfile::tempdir().unwrap();
//!         let store = SqliteStore::open_in_memory().unwrap();
//!         let blobs = FileBlobs::open(&store, dir.path(), LinkMode::Detect).await.unwrap();
//!         (blobs, dir)
//!     });
//! }
//! ```
//!
//! The fixture is a closure returning a future of `(blobs, guard)`: the
//! guard (a temporary directory, or `()`) lives as long as the check.
//! Each check returns `Err` only when the blobs themselves fail where
//! the rule expects success, and panics with the rule (and its test-case
//! id) when they answer wrongly.

use crate::domain::{ArtifactId, ArtifactPath, BlobHash, VersionNumber};
use crate::error::Error;
use crate::ports::Blobs;

/// Generates one `#[tokio::test]` per [`Blobs`] contract rule.
///
/// `$fixture` is a closure returning a future of `(blobs, guard)`.
/// Invoke it inside a test module; the calling crate needs `tokio` with
/// the `macros` and `rt` features as a dev-dependency.
#[macro_export]
macro_rules! blobs_contract_tests {
    ($fixture:expr) => {
        $crate::blobs_contract_tests!(
            @rules $fixture;
            reads_bytes_when_put_and_linked,
            succeeds_when_same_blob_put_twice,
            not_found_when_name_unknown,
            not_found_when_linking_a_blob_never_put,
            reads_normalised_name_when_linked_with_dot_segments,
            keeps_blob_when_one_of_two_links_removed,
            forgets_blob_when_last_link_removed,
            forgets_blob_when_released_without_a_link,
            keeps_blob_when_released_after_its_link,
            keeps_blob_while_another_put_waits_for_its_link,
            keeps_blob_put_again_when_its_last_link_removed,
            moves_file_to_new_blob_when_name_relinked,
            keeps_blob_of_other_artifact_when_artifact_removed,
            succeeds_when_removing_a_version_without_files,
        );
    };
    (@rules $fixture:expr; $($rule:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $rule() -> ::core::result::Result<(), $crate::Error> {
                let (blobs, _guard) = ($fixture)().await;
                $crate::fakes::blobs_contract::$rule(&blobs).await
            }
        )+
    };
}

/// The artifact `raw` (12 base62 characters).
fn artifact(raw: &str) -> Result<ArtifactId, Error> {
    Ok(ArtifactId::parse(raw)?)
}

/// Version `number`.
fn version(number: u32) -> Result<VersionNumber, Error> {
    Ok(VersionNumber::new(number)?)
}

/// The uploaded file name `raw`.
fn name(raw: &str) -> Result<ArtifactPath, Error> {
    Ok(ArtifactPath::new(raw)?)
}

const fn is_not_found<T>(result: &Result<T, Error>) -> bool {
    matches!(result, Err(Error::NotFound(_)))
}

/// Puts `bytes` and returns their hash.
async fn put<B: Blobs>(blobs: &B, bytes: &[u8]) -> Result<BlobHash, Error> {
    let hash = BlobHash::of(bytes);
    blobs.put(&hash, bytes).await?;
    Ok(hash)
}

/// A put blob linked as a file of a version reads back as its bytes,
/// `index.html` and a nested name alike.
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn reads_bytes_when_put_and_linked<B: Blobs>(blobs: &B) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let one = version(1)?;
    let html = put(blobs, b"<h1>hello</h1>").await?;
    let css = put(blobs, b"body{}").await?;
    blobs.link(id, one, &ArtifactPath::index(), &html).await?;
    blobs.link(id, one, &name("css/site.css")?, &css).await?;
    assert_eq!(
        (
            blobs.read(id, one, &ArtifactPath::index()).await?,
            blobs.read(id, one, &name("css/site.css")?).await?,
        ),
        (b"<h1>hello</h1>".to_vec(), b"body{}".to_vec()),
        "BLOB-02: a linked file reads back as the bytes put"
    );
    Ok(())
}

/// A blob already stored stays as it is when put again (two versions,
/// or two files, of one content).
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn succeeds_when_same_blob_put_twice<B: Blobs>(blobs: &B) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let hash = put(blobs, b"same").await?;
    let again = blobs.put(&hash, b"same").await;
    blobs.link(id, version(1)?, &name("a.txt")?, &hash).await?;
    assert_eq!(
        (
            again.is_ok(),
            blobs.read(id, version(1)?, &name("a.txt")?).await?
        ),
        (true, b"same".to_vec()),
        "BLOB-01: putting a stored blob again succeeds and keeps it"
    );
    Ok(())
}

/// Reading a name the version does not have, a version without files or
/// an artifact without files is [`Error::NotFound`].
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn not_found_when_name_unknown<B: Blobs>(blobs: &B) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let other = artifact("Zyx987wvu654")?;
    let hash = put(blobs, b"body{}").await?;
    blobs.link(id, version(1)?, &name("a.css")?, &hash).await?;
    assert_eq!(
        (
            is_not_found(&blobs.read(id, version(1)?, &name("b.css")?).await),
            is_not_found(&blobs.read(id, version(2)?, &name("a.css")?).await),
            is_not_found(&blobs.read(other, version(1)?, &name("a.css")?).await),
        ),
        (true, true, true),
        "BLOB-04: reading a missing file is NotFound"
    );
    Ok(())
}

/// Linking a hash no `put` stored is [`Error::NotFound`], and leaves no
/// file behind.
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn not_found_when_linking_a_blob_never_put<B: Blobs>(blobs: &B) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let hash = BlobHash::of(b"never put");
    let linked = blobs.link(id, version(1)?, &name("a.txt")?, &hash).await;
    assert_eq!(
        (
            is_not_found(&linked),
            is_not_found(&blobs.read(id, version(1)?, &name("a.txt")?).await),
        ),
        (true, true),
        "BLOB-04: linking a blob never put is NotFound and links nothing"
    );
    Ok(())
}

/// A name sent as `a/./b` (or `a//b`) is the file `a/b` of the version.
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn reads_normalised_name_when_linked_with_dot_segments<B: Blobs>(
    blobs: &B,
) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let one = version(1)?;
    let hash = put(blobs, b"nested").await?;
    blobs.link(id, one, &name("a/./b")?, &hash).await?;
    assert_eq!(
        (
            name("a/./b")?.as_str().to_owned(),
            blobs.read(id, one, &name("a/b")?).await?,
            blobs.read(id, one, &name("a//b")?).await?,
        ),
        ("a/b".to_owned(), b"nested".to_vec(), b"nested".to_vec()),
        "BLOB-05: version paths are normalised (a/./b -> a/b)"
    );
    Ok(())
}

/// Removing one of two versions that link a blob keeps the blob for the
/// other one.
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn keeps_blob_when_one_of_two_links_removed<B: Blobs>(blobs: &B) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let hash = put(blobs, b"shared bytes").await?;
    blobs
        .link(id, version(1)?, &name("logo.png")?, &hash)
        .await?;
    blobs
        .link(id, version(2)?, &name("logo.png")?, &hash)
        .await?;
    blobs.remove_version(id, version(1)?).await?;
    assert_eq!(
        (
            is_not_found(&blobs.read(id, version(1)?, &name("logo.png")?).await),
            blobs.read(id, version(2)?, &name("logo.png")?).await?,
        ),
        (true, b"shared bytes".to_vec()),
        "BLOB-03: removing one link keeps the blob"
    );
    Ok(())
}

/// Removing the last version that links a blob removes the blob: linking
/// its hash again is [`Error::NotFound`].
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn forgets_blob_when_last_link_removed<B: Blobs>(blobs: &B) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let hash = put(blobs, b"shared bytes").await?;
    blobs
        .link(id, version(1)?, &name("logo.png")?, &hash)
        .await?;
    blobs
        .link(id, version(2)?, &name("logo.png")?, &hash)
        .await?;
    blobs.release(&hash).await?;
    blobs.remove_version(id, version(1)?).await?;
    blobs.remove_version(id, version(2)?).await?;
    let relinked = blobs.link(id, version(3)?, &name("logo.png")?, &hash).await;
    assert!(
        is_not_found(&relinked),
        "BLOB-03: the blob goes with the last link that named it, got {relinked:?}"
    );
    Ok(())
}

/// A blob put and released without a link goes: linking it is
/// [`Error::NotFound`] (a write whose links failed leaves no blob).
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn forgets_blob_when_released_without_a_link<B: Blobs>(blobs: &B) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let hash = put(blobs, b"its link failed").await?;
    blobs.release(&hash).await?;
    let linked = blobs.link(id, version(1)?, &name("a.txt")?, &hash).await;
    assert!(
        is_not_found(&linked),
        "BLOB-03: a released blob no link names goes, got {linked:?}"
    );
    Ok(())
}

/// A blob released after its link stays while the link names it.
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn keeps_blob_when_released_after_its_link<B: Blobs>(blobs: &B) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let hash = put(blobs, b"linked").await?;
    blobs.link(id, version(1)?, &name("a.txt")?, &hash).await?;
    blobs.release(&hash).await?;
    blobs.release(&hash).await?;
    assert_eq!(
        blobs.read(id, version(1)?, &name("a.txt")?).await?,
        b"linked".to_vec(),
        "BLOB-03: releasing a linked blob keeps it"
    );
    Ok(())
}

/// Two writes put one content; the first releases it without a link, the
/// second still links it.
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn keeps_blob_while_another_put_waits_for_its_link<B: Blobs>(
    blobs: &B,
) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let hash = put(blobs, b"two writes").await?;
    blobs.put(&hash, b"two writes").await?;
    blobs.release(&hash).await?;
    blobs.link(id, version(2)?, &name("a.txt")?, &hash).await?;
    blobs.release(&hash).await?;
    assert_eq!(
        blobs.read(id, version(2)?, &name("a.txt")?).await?,
        b"two writes".to_vec(),
        "BLOB-03: a blob another put waits on stays"
    );
    Ok(())
}

/// A version puts the content an older version links; the older one goes
/// (its link was the last) before the new one links: the blob stays for
/// the new link, and goes with it.
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn keeps_blob_put_again_when_its_last_link_removed<B: Blobs>(
    blobs: &B,
) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let hash = put(blobs, b"same content").await?;
    blobs.link(id, version(1)?, &name("a.txt")?, &hash).await?;
    blobs.release(&hash).await?;
    blobs.put(&hash, b"same content").await?;
    blobs.remove_version(id, version(1)?).await?;
    blobs.link(id, version(2)?, &name("a.txt")?, &hash).await?;
    blobs.release(&hash).await?;
    let read = blobs.read(id, version(2)?, &name("a.txt")?).await?;
    blobs.remove_version(id, version(2)?).await?;
    assert_eq!(
        (
            read,
            is_not_found(&blobs.link(id, version(3)?, &name("a.txt")?, &hash).await),
        ),
        (b"same content".to_vec(), true),
        "BLOB-03: a blob a put waits on outlives its last link, then goes with the next"
    );
    Ok(())
}

/// Linking a name of a version again to another blob makes it that blob,
/// and the first one goes when that was its only link.
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn moves_file_to_new_blob_when_name_relinked<B: Blobs>(blobs: &B) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let old = put(blobs, b"old").await?;
    let new = put(blobs, b"new").await?;
    blobs.link(id, version(1)?, &name("a.txt")?, &old).await?;
    blobs.link(id, version(1)?, &name("a.txt")?, &new).await?;
    blobs.release(&old).await?;
    blobs.release(&new).await?;
    assert_eq!(
        (
            blobs.read(id, version(1)?, &name("a.txt")?).await?,
            is_not_found(&blobs.link(id, version(2)?, &name("b.txt")?, &old).await),
        ),
        (b"new".to_vec(), true),
        "BLOB-03: a relinked name names the new blob, and the old one goes"
    );
    Ok(())
}

/// Removing an artifact removes its files and the blobs only it named,
/// and keeps the blob another artifact links.
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn keeps_blob_of_other_artifact_when_artifact_removed<B: Blobs>(
    blobs: &B,
) -> Result<(), Error> {
    let gone = artifact("Abc123def456")?;
    let kept = artifact("Zyx987wvu654")?;
    let shared = put(blobs, b"shared").await?;
    let own = put(blobs, b"own").await?;
    blobs
        .link(gone, version(1)?, &name("s.txt")?, &shared)
        .await?;
    blobs.link(gone, version(2)?, &name("o.txt")?, &own).await?;
    blobs
        .link(kept, version(1)?, &name("s.txt")?, &shared)
        .await?;
    blobs.release(&shared).await?;
    blobs.release(&own).await?;
    blobs.remove_artifact(gone).await?;
    assert_eq!(
        (
            is_not_found(&blobs.read(gone, version(1)?, &name("s.txt")?).await),
            is_not_found(&blobs.read(gone, version(2)?, &name("o.txt")?).await),
            blobs.read(kept, version(1)?, &name("s.txt")?).await?,
            is_not_found(&blobs.link(kept, version(2)?, &name("o.txt")?, &own).await),
        ),
        (true, true, b"shared".to_vec(), true),
        "BLOB-03: removing an artifact keeps the blobs other artifacts link"
    );
    Ok(())
}

/// Removing a version (or an artifact) that has no files succeeds and
/// changes nothing.
///
/// # Errors
///
/// Whatever the blobs report.
///
/// # Panics
///
/// When the blobs break the rule.
pub async fn succeeds_when_removing_a_version_without_files<B: Blobs>(
    blobs: &B,
) -> Result<(), Error> {
    let id = artifact("Abc123def456")?;
    let hash = put(blobs, b"kept").await?;
    blobs.link(id, version(1)?, &name("k.txt")?, &hash).await?;
    let version_gone = blobs.remove_version(id, version(7)?).await;
    let artifact_gone = blobs.remove_artifact(artifact("Zyx987wvu654")?).await;
    assert_eq!(
        (
            version_gone.is_ok(),
            artifact_gone.is_ok(),
            blobs.read(id, version(1)?, &name("k.txt")?).await?,
        ),
        (true, true, b"kept".to_vec()),
        "BLOB-03: removing what has no files is a no-op"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    mod fake_blobs {
        use crate::fakes::{CallLog, FakeBlobs};

        // TC: BLOB-03, BLOB-04, BLOB-05
        crate::blobs_contract_tests!(async || { (FakeBlobs::with_log(CallLog::new()), ()) });
    }
}
