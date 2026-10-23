#![cfg(test)]
//! BLOB-01…05: the blobs of artifact versions on disk (`FileBlobs`), over a temporary directory and a SQLite file,
//! driven through the `Blobs` port. The port's contract
//! (`liftoff_core::fakes::blobs_contract`) runs in both link modes; the
//! rest checks what only the disk has: the layout under
//! `paths.data/artifacts`, hard links or copies, the refcount in
//! `artifact_blobs`, atomic writes and paths that stay in their version.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use liftoff_core::Error;
use liftoff_core::domain::{
    Artifact, ArtifactFile, ArtifactId, ArtifactPath, ArtifactVersion, BlobHash, UserRole,
    VersionNumber, Visibility,
};
use liftoff_core::fakes::{FakePorts, Records};
use liftoff_core::ports::{Blobs as _, Store as _};
use liftoff_store::{FileBlobs, LinkMode, SqliteStore};
use tempfile::TempDir;

/// A temporary data directory with the database and the blobs under it.
struct Disk {
    dir: TempDir,
    store: SqliteStore,
}

impl Disk {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("liftoff.db")).unwrap();
        Self { dir, store }
    }

    /// `paths.data/artifacts`.
    fn root(&self) -> PathBuf {
        self.dir.path().join("artifacts")
    }

    /// The blob file of `hash`.
    fn blob(&self, hash: &BlobHash) -> PathBuf {
        self.root().join("blobs").join(hash.as_str())
    }

    /// The directory of version `number` of `id`.
    fn version(&self, id: ArtifactId, number: u32) -> PathBuf {
        self.root().join(id.as_str()).join(number.to_string())
    }

    /// The blobs opened (again) over this directory in `mode`.
    async fn open(&self, mode: LinkMode) -> FileBlobs {
        FileBlobs::open(&self.store, self.root(), mode)
            .await
            .unwrap()
    }

    /// `artifact_blobs.refs` of `hash`, `None` without a row, read over a
    /// second connection to the database.
    fn refs(&self, hash: &BlobHash) -> Option<i64> {
        let conn = rusqlite::Connection::open(self.dir.path().join("liftoff.db")).unwrap();
        let mut query = conn
            .prepare("SELECT refs FROM artifact_blobs WHERE hash = ?1")
            .unwrap();
        let mut rows = query.query([hash.as_str()]).unwrap();
        rows.next().unwrap().map(|row| row.get(0).unwrap())
    }

    /// Stores the artifact `raw` with the row of its version 1.
    async fn artifact_row(&self, raw: &str) -> ArtifactId {
        let fakes = FakePorts::new();
        let records = Records::new(fakes.rng.clone(), fakes.clock.clone());
        let owner = records.user("owner", UserRole::Admin).unwrap();
        self.store.insert_user(&owner).await.unwrap();
        let at = owner.created_at;
        let artifact = id(raw);
        let first = ArtifactVersion {
            artifact_id: artifact,
            number: VersionNumber::FIRST,
            title: "Report".to_owned(),
            size_bytes: 8,
            created_by: owner.id,
            created_at: at,
            files: vec![ArtifactFile {
                name: ArtifactPath::index(),
                hash: BlobHash::of(b"<p>1</p>"),
                size: 8,
            }],
        };
        let row = Artifact {
            id: artifact,
            owner_user_id: owner.id,
            title: "Report".to_owned(),
            visibility: Visibility::Shared,
            current_version: VersionNumber::FIRST,
            expires_at: None,
            created_at: at,
            updated_at: at,
        };
        self.store.insert_artifact(&row, &first).await.unwrap();
        artifact
    }
}

fn id(raw: &str) -> ArtifactId {
    ArtifactId::parse(raw).unwrap()
}

fn v(number: u32) -> VersionNumber {
    VersionNumber::new(number).unwrap()
}

fn name(raw: &str) -> ArtifactPath {
    ArtifactPath::new(raw).unwrap()
}

/// The names in `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Sets the modification time of `path` to `ago` before now.
fn age(path: &Path, ago: Duration) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now().checked_sub(ago).unwrap())
        .unwrap();
}

/// A directory made read-only (nothing in it can be removed) while the
/// guard lives; writable again when dropped, so the tempdir goes.
struct ReadOnly(PathBuf);

impl ReadOnly {
    fn set(dir: &Path) -> Self {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
        Self(dir.to_path_buf())
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _restored = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

/// Older than the hour after which leftovers are collected.
const TWO_HOURS: Duration = Duration::from_hours(2);

/// Puts `bytes`, returning their hash.
async fn put(blobs: &FileBlobs, bytes: &[u8]) -> BlobHash {
    let hash = BlobHash::of(bytes);
    blobs.put(&hash, bytes).await.unwrap();
    hash
}

// TC: BLOB-02 (the hard-link path passes the port's contract)
mod contract_hard_links {
    use super::*;

    liftoff_core::blobs_contract_tests!(async || {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        (blobs, disk)
    });
}

// TC: BLOB-02 (the copy path passes the same contract)
mod contract_copies {
    use super::*;

    liftoff_core::blobs_contract_tests!(async || {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Copy).await;
        (blobs, disk)
    });
}

mod put {
    use super::*;

    // TC: BLOB-01
    #[tokio::test]
    async fn leaves_only_the_whole_blob_when_put() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let hash = put(&blobs, b"body { color: red }").await;
        assert_eq!(
            (
                entries(&disk.root().join("blobs")),
                fs::read(disk.blob(&hash)).unwrap(),
            ),
            (
                vec![hash.as_str().to_owned()],
                b"body { color: red }".to_vec()
            ),
            "a put leaves the blob under its hash and no temporary file"
        );
    }

    // TC: BLOB-01
    #[tokio::test]
    async fn stores_whole_blob_when_put_after_an_interrupted_write() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let bytes = b"the whole content of the file";
        fs::write(
            disk.root().join("blobs").join(".tmp-interrupted"),
            &bytes[..8],
        )
        .unwrap();
        let hash = put(&blobs, bytes).await;
        blobs
            .link(id("Abc123def456"), v(1), &name("a.txt"), &hash)
            .await
            .unwrap();
        assert_eq!(
            blobs
                .read(id("Abc123def456"), v(1), &name("a.txt"))
                .await
                .unwrap(),
            bytes.to_vec()
        );
    }

    // TC: BLOB-01
    #[tokio::test]
    async fn removes_stale_temporary_file_when_opened() {
        let disk = Disk::new();
        drop(disk.open(LinkMode::Detect).await);
        let stale = disk.root().join("blobs").join(".tmp-interrupted");
        fs::write(&stale, b"half").unwrap();
        age(&stale, TWO_HOURS);
        let _blobs = disk.open(LinkMode::Detect).await;
        assert!(
            !stale.exists(),
            "a temporary file older than an hour is collected when opened"
        );
    }
}

mod link {
    use super::*;

    // TC: BLOB-02
    #[tokio::test]
    async fn detects_hard_links_when_file_system_has_them() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        assert!(blobs.hard_links());
    }

    // TC: BLOB-02
    #[tokio::test]
    async fn hard_links_version_file_to_blob_when_file_system_has_them() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let hash = put(&blobs, b"body{}").await;
        blobs
            .link(artifact, v(1), &name("css/site.css"), &hash)
            .await
            .unwrap();
        let file = fs::metadata(disk.version(artifact, 1).join("css/site.css")).unwrap();
        let blob = fs::metadata(disk.blob(&hash)).unwrap();
        assert_eq!(
            (file.ino() == blob.ino(), blob.nlink()),
            (true, 2),
            "the version's file is a hard link to the blob"
        );
    }

    // TC: BLOB-02
    #[tokio::test]
    async fn reports_copies_when_copy_forced() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Copy).await;
        assert!(!blobs.hard_links());
    }

    // TC: BLOB-02
    #[tokio::test]
    async fn copies_blob_into_version_when_copy_forced() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Copy).await;
        let artifact = id("Abc123def456");
        let hash = put(&blobs, b"body{}").await;
        blobs
            .link(artifact, v(1), &name("css/site.css"), &hash)
            .await
            .unwrap();
        let path = disk.version(artifact, 1).join("css/site.css");
        let file = fs::metadata(&path).unwrap();
        let blob = fs::metadata(disk.blob(&hash)).unwrap();
        assert_eq!(
            (
                fs::read(&path).unwrap(),
                file.ino() == blob.ino(),
                blob.nlink()
            ),
            (b"body{}".to_vec(), false, 1),
            "the version's file is a copy of the blob"
        );
    }
}

mod refcount {
    use super::*;

    // TC: BLOB-03
    #[tokio::test]
    async fn counts_two_refs_when_two_versions_link_one_blob() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let hash = put(&blobs, b"logo").await;
        blobs
            .link(artifact, v(1), &name("logo.png"), &hash)
            .await
            .unwrap();
        blobs
            .link(artifact, v(2), &name("logo.png"), &hash)
            .await
            .unwrap();
        assert_eq!(disk.refs(&hash), Some(2));
    }

    // TC: BLOB-03
    #[tokio::test]
    async fn keeps_blob_file_when_one_of_two_links_removed() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let hash = put(&blobs, b"logo").await;
        blobs
            .link(artifact, v(1), &name("logo.png"), &hash)
            .await
            .unwrap();
        blobs
            .link(artifact, v(2), &name("logo.png"), &hash)
            .await
            .unwrap();
        blobs.remove_version(artifact, v(1)).await.unwrap();
        assert_eq!(
            (
                disk.refs(&hash),
                disk.blob(&hash).exists(),
                disk.version(artifact, 1).exists(),
            ),
            (Some(1), true, false)
        );
    }

    // TC: BLOB-03
    #[tokio::test]
    async fn deletes_blob_file_and_row_when_last_link_removed() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let hash = put(&blobs, b"logo").await;
        blobs
            .link(artifact, v(1), &name("logo.png"), &hash)
            .await
            .unwrap();
        blobs
            .link(artifact, v(2), &name("logo.png"), &hash)
            .await
            .unwrap();
        blobs.release(&hash).await.unwrap();
        blobs.remove_version(artifact, v(1)).await.unwrap();
        blobs.remove_version(artifact, v(2)).await.unwrap();
        assert_eq!((disk.refs(&hash), disk.blob(&hash).exists()), (None, false));
    }

    // TC: BLOB-03 (a write whose links failed releases its blobs: none
    // waits for the next start)
    #[tokio::test]
    async fn deletes_blob_file_when_put_released_without_a_link() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let hash = put(&blobs, b"its link failed").await;
        blobs.release(&hash).await.unwrap();
        assert_eq!((disk.blob(&hash).exists(), disk.refs(&hash)), (false, None));
    }

    // TC: BLOB-03 (the content an older version links is put again; the
    // older version goes before the new one links it)
    #[tokio::test]
    async fn keeps_blob_file_when_its_last_link_removed_while_put_again() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let hash = put(&blobs, b"logo").await;
        blobs
            .link(artifact, v(1), &name("logo.png"), &hash)
            .await
            .unwrap();
        blobs.release(&hash).await.unwrap();
        blobs.put(&hash, b"logo").await.unwrap();
        blobs.remove_version(artifact, v(1)).await.unwrap();
        let kept = disk.blob(&hash).exists();
        blobs
            .link(artifact, v(2), &name("logo.png"), &hash)
            .await
            .unwrap();
        blobs.release(&hash).await.unwrap();
        assert_eq!(
            (
                kept,
                disk.refs(&hash),
                blobs.read(artifact, v(2), &name("logo.png")).await.unwrap()
            ),
            (true, Some(1), b"logo".to_vec())
        );
    }

    // TC: BLOB-03 (a name linked again to another blob takes its link off
    // the old one)
    #[tokio::test]
    async fn moves_ref_to_new_hash_when_name_relinked() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let old = put(&blobs, b"old").await;
        let new = put(&blobs, b"new").await;
        for hash in [&old, &new] {
            blobs
                .link(artifact, v(1), &name("a.txt"), hash)
                .await
                .unwrap();
        }
        blobs.release(&old).await.unwrap();
        blobs.release(&new).await.unwrap();
        assert_eq!(
            (
                disk.refs(&old),
                disk.blob(&old).exists(),
                disk.refs(&new),
                fs::read(disk.version(artifact, 1).join("a.txt")).unwrap(),
            ),
            (None, false, Some(1), b"new".to_vec())
        );
    }

    // TC: BLOB-03 (the same link written twice counts once)
    #[tokio::test]
    async fn counts_one_ref_when_same_name_linked_twice() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let hash = put(&blobs, b"once").await;
        for _ in 0..2 {
            blobs
                .link(artifact, v(1), &name("a.txt"), &hash)
                .await
                .unwrap();
        }
        assert_eq!(disk.refs(&hash), Some(1));
    }

    // TC: BLOB-03 (the directory goes best-effort once the rows are gone;
    // opening collects what is left)
    #[tokio::test]
    async fn forgets_links_when_version_directory_cannot_be_removed() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let hash = put(&blobs, b"stuck").await;
        blobs
            .link(artifact, v(1), &name("a/b.txt"), &hash)
            .await
            .unwrap();
        blobs.release(&hash).await.unwrap();
        let stuck = disk.version(artifact, 1).join("a");
        let _guard = ReadOnly::set(&stuck);
        let removed = blobs.remove_version(artifact, v(1)).await;
        assert_eq!(
            (removed.is_ok(), disk.refs(&hash), disk.blob(&hash).exists()),
            (true, None, false),
            "{removed:?}"
        );
    }

    // TC: BLOB-03 (a leftover version directory that cannot be removed
    // does not stop the blobs from opening)
    #[tokio::test]
    async fn opens_when_a_leftover_version_directory_cannot_be_removed() {
        let disk = Disk::new();
        let artifact = disk.artifact_row("Abc123def456").await;
        let blobs = disk.open(LinkMode::Detect).await;
        let hash = put(&blobs, b"stuck").await;
        blobs
            .link(artifact, v(2), &name("a/b.txt"), &hash)
            .await
            .unwrap();
        drop(blobs);
        let _guard = ReadOnly::set(&disk.version(artifact, 2).join("a"));
        let opened = FileBlobs::open(&disk.store, disk.root(), LinkMode::Detect).await;
        assert_eq!(
            (opened.is_ok(), disk.refs(&hash)),
            (true, None),
            "{:?}",
            opened.err()
        );
    }

    // TC: BLOB-03
    #[tokio::test]
    async fn deletes_artifact_directory_and_its_only_blobs_when_artifact_removed() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Copy).await;
        let gone = id("Abc123def456");
        let kept = id("Zyx987wvu654");
        let own = put(&blobs, b"own").await;
        let shared = put(&blobs, b"shared").await;
        blobs.link(gone, v(1), &name("o.txt"), &own).await.unwrap();
        blobs
            .link(gone, v(2), &name("s.txt"), &shared)
            .await
            .unwrap();
        blobs
            .link(kept, v(1), &name("s.txt"), &shared)
            .await
            .unwrap();
        blobs.release(&own).await.unwrap();
        blobs.release(&shared).await.unwrap();
        blobs.remove_artifact(gone).await.unwrap();
        assert_eq!(
            (
                disk.root().join(gone.as_str()).exists(),
                disk.blob(&own).exists(),
                disk.refs(&own),
                disk.blob(&shared).exists(),
                disk.refs(&shared),
            ),
            (false, false, None, true, Some(1))
        );
    }

    // TC: BLOB-03
    #[tokio::test]
    async fn collects_unlinked_blob_older_than_an_hour_when_opened() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let hash = put(&blobs, b"put, then the link failed").await;
        drop(blobs);
        age(&disk.blob(&hash), TWO_HOURS);
        let _blobs = disk.open(LinkMode::Detect).await;
        assert_eq!((disk.blob(&hash).exists(), disk.refs(&hash)), (false, None));
    }

    // TC: BLOB-03
    #[tokio::test]
    async fn keeps_fresh_unlinked_blob_when_opened() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let hash = put(&blobs, b"a concurrent version may link it").await;
        drop(blobs);
        let blobs = disk.open(LinkMode::Detect).await;
        blobs
            .link(id("Abc123def456"), v(1), &name("a.txt"), &hash)
            .await
            .unwrap();
        assert_eq!(disk.refs(&hash), Some(1));
    }

    // TC: BLOB-03
    #[tokio::test]
    async fn removes_version_directory_without_rows_when_opened() {
        let disk = Disk::new();
        let artifact = disk.artifact_row("Abc123def456").await;
        let blobs = disk.open(LinkMode::Detect).await;
        let hash = put(&blobs, b"<p>1</p>").await;
        blobs
            .link(artifact, v(1), &ArtifactPath::index(), &hash)
            .await
            .unwrap();
        // Version 2 has no rows (pruned, or its write was taken back) and
        // the best-effort removal of its files failed.
        blobs
            .link(artifact, v(2), &ArtifactPath::index(), &hash)
            .await
            .unwrap();
        drop(blobs);
        let blobs = disk.open(LinkMode::Detect).await;
        assert_eq!(
            (
                disk.version(artifact, 2).exists(),
                disk.refs(&hash),
                blobs
                    .read(artifact, v(1), &ArtifactPath::index())
                    .await
                    .unwrap(),
            ),
            (false, Some(1), b"<p>1</p>".to_vec())
        );
    }
}

mod read {
    use super::*;

    // TC: BLOB-04
    #[tokio::test]
    async fn not_found_when_name_is_a_directory_of_the_version() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let hash = put(&blobs, b"nested").await;
        blobs
            .link(artifact, v(1), &name("a/b.txt"), &hash)
            .await
            .unwrap();
        let read = blobs.read(artifact, v(1), &name("a")).await;
        assert!(matches!(read, Err(Error::NotFound(_))), "got {read:?}");
    }

    // TC: BLOB-04
    #[tokio::test]
    async fn not_found_when_nothing_was_ever_stored() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let read = blobs
            .read(id("Abc123def456"), v(1), &ArtifactPath::index())
            .await;
        assert!(matches!(read, Err(Error::NotFound(_))), "got {read:?}");
    }
}

mod paths {
    use super::*;

    // TC: BLOB-05
    #[tokio::test]
    async fn stores_file_at_normalised_path_when_linked_with_dot_segments() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let hash = put(&blobs, b"nested").await;
        blobs
            .link(artifact, v(1), &name("a/./b"), &hash)
            .await
            .unwrap();
        let version = disk.version(artifact, 1);
        assert_eq!(
            (
                entries(&version),
                entries(&version.join("a")),
                fs::read(version.join("a").join("b")).unwrap(),
            ),
            (
                vec!["a".to_owned()],
                vec!["b".to_owned()],
                b"nested".to_vec()
            )
        );
    }

    // TC: BLOB-05 (the longest segment the domain takes is a file name
    // in both modes; no temporary name is built from it)
    #[tokio::test]
    async fn links_file_named_by_longest_segment_in_both_modes() {
        let longest = name(&format!(
            "a/{}",
            "b".repeat(ArtifactPath::MAX_SEGMENT_BYTES)
        ));
        let mut read = Vec::new();
        for mode in [LinkMode::Detect, LinkMode::Copy] {
            let disk = Disk::new();
            let blobs = disk.open(mode).await;
            let artifact = id("Abc123def456");
            let hash = put(&blobs, b"long").await;
            let linked = blobs.link(artifact, v(1), &longest, &hash).await;
            read.push((
                linked.map_err(|error| error.to_string()),
                blobs.read(artifact, v(1), &longest).await.ok(),
            ));
        }
        assert_eq!(read, vec![(Ok(()), Some(b"long".to_vec())); 2]);
    }

    // TC: BLOB-05
    #[tokio::test]
    async fn refuses_link_when_a_directory_of_the_version_leads_outside() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let outside = disk.dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let version = disk.version(artifact, 1);
        fs::create_dir_all(&version).unwrap();
        std::os::unix::fs::symlink(&outside, version.join("a")).unwrap();
        let hash = put(&blobs, b"escaped").await;
        let linked = blobs.link(artifact, v(1), &name("a/b"), &hash).await;
        assert_eq!(
            (linked.is_err(), entries(&outside)),
            (true, Vec::<String>::new()),
            "a link never writes outside its version's directory, got {linked:?}"
        );
    }

    // TC: BLOB-05
    #[tokio::test]
    async fn not_found_when_read_would_leave_the_version_directory() {
        let disk = Disk::new();
        let blobs = disk.open(LinkMode::Detect).await;
        let artifact = id("Abc123def456");
        let outside = disk.dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"not an artifact's").unwrap();
        let version = disk.version(artifact, 1);
        fs::create_dir_all(&version).unwrap();
        std::os::unix::fs::symlink(&outside, version.join("a")).unwrap();
        let read = blobs.read(artifact, v(1), &name("a/secret.txt")).await;
        assert!(
            matches!(read, Err(Error::NotFound(_))),
            "a read never leaves its version's directory, got {read:?}"
        );
    }
}
