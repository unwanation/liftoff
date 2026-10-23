//! The kept `.wasm` files of function deploys,
//! `paths.data/functions/<hash>.wasm`.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use liftoff_core::Error;
use liftoff_core::domain::ComponentHash;
use liftoff_core::ports::{ComponentFiles, ComponentsLock};
use tokio::io::AsyncWriteExt;

use crate::files::io_error;

/// Mode of the directory: the daemon's own.
const DIR_MODE: u32 = 0o700;
/// Mode of a kept component.
const FILE_MODE: u32 = 0o600;
/// The extension of a kept component.
const EXTENSION: &str = "wasm";

/// Tells apart the temporary files of writes in flight.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// One file per [`ComponentHash`] under one directory.
///
/// A file is written to a temporary name next to it and renamed into
/// place, so a reader never sees half a component; a file already kept
/// is not written again. Every write has a temporary name of its own, so
/// two saves of one component at once both succeed. Files whose name is
/// not `<hash>.wasm` (the temporary ones of a write a crash cut short)
/// are not listed. Clones share one [`ComponentsLock`].
#[derive(Debug, Clone)]
pub struct FileComponentFiles {
    dir: PathBuf,
    guard: Arc<ComponentsLock>,
}

impl FileComponentFiles {
    /// Components under `dir` (`paths.data/functions`), made on the first
    /// write.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            guard: Arc::default(),
        }
    }

    fn path(&self, hash: &ComponentHash) -> PathBuf {
        self.dir.join(format!("{hash}.{EXTENSION}"))
    }

    async fn prepare(&self) -> io::Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        tokio::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(DIR_MODE)).await
    }
}

/// Writes `content` to `temporary` with [`FILE_MODE`], synced, and renames
/// it to `path`.
async fn write_atomically(temporary: &Path, path: &Path, content: &[u8]) -> io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(temporary)
        .await?;
    file.write_all(content).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(temporary, path).await
}

impl ComponentFiles for FileComponentFiles {
    fn lock(&self) -> &ComponentsLock {
        &self.guard
    }

    async fn save(&self, hash: &ComponentHash, component: &[u8]) -> Result<(), Error> {
        let path = self.path(hash);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(());
        }
        self.prepare()
            .await
            .map_err(|source| io_error("cannot create", &self.dir, &source))?;
        let write = WRITES.fetch_add(1, Ordering::Relaxed);
        let temporary = self.dir.join(format!(
            ".{hash}.{EXTENSION}.{}.{write}.part",
            std::process::id()
        ));
        let written = write_atomically(&temporary, &path, component).await;
        if let Err(source) = written {
            if let Err(error) = tokio::fs::remove_file(&temporary).await {
                tracing::debug!(%error, "no partial component to remove");
            }
            return Err(io_error("cannot write", &path, &source));
        }
        Ok(())
    }

    async fn read(&self, hash: &ComponentHash) -> Result<Option<Vec<u8>>, Error> {
        let path = self.path(hash);
        match tokio::fs::read(&path).await {
            Ok(content) => Ok(Some(content)),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(io_error("cannot read", &path, &source)),
        }
    }

    async fn list(&self) -> Result<Vec<ComponentHash>, Error> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error("cannot read", &self.dir, &source)),
        };
        let mut hashes = Vec::new();
        loop {
            let entry = entries
                .next_entry()
                .await
                .map_err(|source| io_error("cannot read", &self.dir, &source))?;
            let Some(entry) = entry else {
                return Ok(hashes);
            };
            let name = entry.file_name();
            let hash = name
                .to_str()
                .and_then(|name| name.strip_suffix(".wasm"))
                .and_then(|stem| ComponentHash::parse(stem).ok());
            if let Some(hash) = hash {
                hashes.push(hash);
            }
        }
    }

    async fn remove(&self, hash: &ComponentHash) -> Result<(), Error> {
        let path = self.path(hash);
        match tokio::fs::remove_file(&path).await {
            Err(source) if source.kind() != io::ErrorKind::NotFound => {
                Err(io_error("cannot remove", &path, &source))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("exists")
            .permissions()
            .mode()
            & 0o777
    }

    fn files(root: &Path) -> FileComponentFiles {
        FileComponentFiles::new(root.join("functions"))
    }

    mod save {
        use super::*;

        #[tokio::test]
        async fn writes_hash_named_file_0600_in_directory_0700_when_saved() {
            let root = tempfile::tempdir().expect("tempdir");
            let hash = ComponentHash::of(b"component");
            files(root.path())
                .save(&hash, b"component")
                .await
                .expect("saves");
            let dir = root.path().join("functions");
            let path = dir.join(format!("{hash}.wasm"));
            assert_eq!(
                (std::fs::read(&path).ok(), mode(&dir), mode(&path)),
                (Some(b"component".to_vec()), 0o700, 0o600)
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn every_save_succeeds_when_one_component_is_saved_at_once() {
            let root = tempfile::tempdir().expect("tempdir");
            let files = files(root.path());
            let component = vec![7_u8; 1024 * 1024];
            let hash = ComponentHash::of(&component);
            let saves: Vec<_> = (0..16)
                .map(|_| {
                    let (files, hash, component) = (files.clone(), hash.clone(), component.clone());
                    tokio::spawn(async move { files.save(&hash, &component).await })
                })
                .collect();
            let mut failed = Vec::new();
            for save in saves {
                if let Err(error) = save.await.expect("joins") {
                    failed.push(error.to_string());
                }
            }
            let kept = files.read(&hash).await.expect("reads") == Some(component);
            assert_eq!((failed, kept), (Vec::<String>::new(), true));
        }

        #[tokio::test]
        async fn leaves_no_temporary_file_when_saved() {
            let root = tempfile::tempdir().expect("tempdir");
            let hash = ComponentHash::of(b"component");
            files(root.path())
                .save(&hash, b"component")
                .await
                .expect("saves");
            let names: Vec<String> = std::fs::read_dir(root.path().join("functions"))
                .expect("lists")
                .map(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            assert_eq!(names, vec![format!("{hash}.wasm")]);
        }
    }

    mod read {
        use super::*;

        #[tokio::test]
        async fn none_when_nothing_kept() {
            let root = tempfile::tempdir().expect("tempdir");
            let read = files(root.path())
                .read(&ComponentHash::of(b"component"))
                .await;
            assert_eq!(read.expect("reads"), None);
        }

        #[tokio::test]
        async fn returns_what_was_saved_when_read_back() {
            let root = tempfile::tempdir().expect("tempdir");
            let files = files(root.path());
            let hash = ComponentHash::of(b"component");
            files.save(&hash, b"component").await.expect("saves");
            assert_eq!(
                files.read(&hash).await.expect("reads"),
                Some(b"component".to_vec())
            );
        }
    }

    mod list {
        use super::*;

        #[tokio::test]
        async fn empty_when_directory_is_missing() {
            let root = tempfile::tempdir().expect("tempdir");
            assert_eq!(
                files(root.path()).list().await.expect("lists"),
                Vec::<ComponentHash>::new()
            );
        }

        #[tokio::test]
        async fn hashes_of_kept_files_only_when_other_files_lie_around() {
            let root = tempfile::tempdir().expect("tempdir");
            let files = files(root.path());
            let hash = ComponentHash::of(b"component");
            files.save(&hash, b"component").await.expect("saves");
            let dir = root.path().join("functions");
            std::fs::write(dir.join(format!(".{hash}.wasm.part")), b"half").expect("writes");
            std::fs::write(dir.join("notes.txt"), b"x").expect("writes");
            assert_eq!(files.list().await.expect("lists"), vec![hash]);
        }
    }

    mod remove {
        use super::*;

        #[tokio::test]
        async fn file_is_gone_when_removed() {
            let root = tempfile::tempdir().expect("tempdir");
            let files = files(root.path());
            let hash = ComponentHash::of(b"component");
            files.save(&hash, b"component").await.expect("saves");
            files.remove(&hash).await.expect("removes");
            assert_eq!(files.read(&hash).await.expect("reads"), None);
        }

        #[tokio::test]
        async fn ok_when_nothing_kept() {
            let root = tempfile::tempdir().expect("tempdir");
            let removed = files(root.path())
                .remove(&ComponentHash::of(b"component"))
                .await;
            assert!(removed.is_ok(), "{removed:?}");
        }
    }
}
