//! [`FileBlobs`]: the [`Blobs`] port on disk.
//!
//! Under `root` (`paths.data/artifacts`): `blobs/<sha256 hex>` as
//! content-addressed storage, and `<artifact>/<n>/<name>` as the files
//! of version `n`, hard links to the blobs (a copy where the file system
//! has none: [`LinkMode`], detected once when opened). The number of
//! links naming each blob is kept in the table `artifact_blobs`
//! (`hash`, `refs`) of the store's database, next to the artifact rows,
//! and the blob of each file of a version in `artifact_blob_links`
//! (migration `0012`), so the blobs share the [`SqliteStore`]'s
//! connection; a blob goes with its last link (BLOB-03), once no `put`
//! of it waits for its links ([`Blobs::release`]).
//!
//! The calls that change links (`link`, `release`, `remove_*`) take a
//! lock of their own, so they run one at a time, and do their file work
//! under it; the connection is held only for their short row
//! transactions, so a large copy never stalls the rest of the store.
//! Rows and files go in this order: a link commits its rows once the
//! file is in place; a removal drops the rows, then (best-effort) the
//! version's directory, then the files of the blobs no link names. A
//! blob is written to a temporary file in `blobs/` and renamed into
//! place, so an interrupted write leaves no partial blob (BLOB-01).
//! Names are the domain's normalised [`ArtifactPath`]s, and a path that
//! would leave the version's directory (through a symbolic link on disk)
//! is refused (BLOB-05); temporary names never derive from them.
//!
//! When opened, the blobs collect what an earlier run left: blobs no
//! link names and temporary files, both older than an hour, and version
//! directories whose version has no row in `artifact_versions` (a
//! failed best-effort removal after the rows went). What cannot be
//! removed is logged and left for the next start.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use liftoff_core::Error;
use liftoff_core::domain::{ArtifactId, ArtifactPath, BlobHash, VersionNumber};
use liftoff_core::ports::Blobs;
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use crate::SqliteStore;
use crate::sql::internal;

/// The directory of the blobs under `root`.
const BLOBS: &str = "blobs";

/// The prefix of temporary files: no blob's name (64 hex characters) and
/// no file of a version (a segment of an [`ArtifactPath`] never starts
/// with a dot) has it.
const TEMP_PREFIX: &str = ".tmp-";

/// How old a leftover (an unlinked blob, a temporary file) must be before
/// opening collects it: a younger one may belong to a write in progress.
const LEFTOVER_AGE: Duration = Duration::from_hours(1);

/// How the files of a version name their blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkMode {
    /// Hard links when the file system under `root` makes them (probed
    /// once when opened), copies otherwise.
    #[default]
    Detect,
    /// Copies of the blobs, whatever the file system can do (tests of the
    /// copy path, BLOB-02).
    Copy,
}

/// The files of artifact versions under a directory: content-addressed
/// blobs and the versions that link them.
#[derive(Debug, Clone)]
pub struct FileBlobs {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    root: PathBuf,
    hard_links: bool,
    store: SqliteStore,
    /// Held by every call that changes links, for its file work and its
    /// rows: they run one at a time.
    links: Mutex<()>,
    /// The blobs `put` and not released since, with the number of such
    /// puts: a blob whose last link goes keeps its file while it is here,
    /// since a version is about to link it (put, link, release).
    pending: Mutex<HashMap<String, usize>>,
}

impl Inner {
    /// Runs `work` with the store's connection, held only for it.
    fn rows<T>(&self, work: impl FnOnce(&mut Connection) -> Result<T, Error>) -> Result<T, Error> {
        work(&mut lock(&self.store.conn))
    }

    fn blob(&self, hash: &str) -> PathBuf {
        self.root.join(BLOBS).join(hash)
    }
}

impl FileBlobs {
    /// The blobs under `root` (`paths.data/artifacts`, created if
    /// needed), counting their links in `store`'s database.
    ///
    /// Probes once whether `root` takes hard links (unless `mode` is
    /// [`LinkMode::Copy`]), then collects what an earlier run left (see
    /// the module documentation).
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when `root` cannot be created or read, or the
    /// database fails.
    pub async fn open(
        store: &SqliteStore,
        root: impl Into<PathBuf>,
        mode: LinkMode,
    ) -> Result<Self, Error> {
        let root = root.into();
        let store = store.clone();
        let inner = blocking(move || {
            let blobs = root.join(BLOBS);
            fs::create_dir_all(&blobs).map_err(|error| io_failure("create", &blobs, error))?;
            let hard_links = match mode {
                LinkMode::Detect => probe_hard_links(&blobs)?,
                LinkMode::Copy => false,
            };
            let inner = Inner {
                root,
                hard_links,
                store,
                links: Mutex::default(),
                pending: Mutex::default(),
            };
            {
                let _links = lock(&inner.links);
                collect_versions(&inner)?;
                collect_blobs(&inner)?;
            }
            Ok(inner)
        })
        .await?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Whether the files of versions are hard links to the blobs (`false`:
    /// copies), as decided when opened.
    #[must_use]
    pub fn hard_links(&self) -> bool {
        self.inner.hard_links
    }
}

impl Blobs for FileBlobs {
    async fn put(&self, hash: &BlobHash, bytes: &[u8]) -> Result<(), Error> {
        let inner = Arc::clone(&self.inner);
        let key = hash.as_str().to_owned();
        let bytes = bytes.to_vec();
        blocking(move || {
            let target = inner.blob(&key);
            let written = claim(&inner.pending, &key, &target).and_then(|present| {
                if present {
                    return Ok(());
                }
                write_atomically(&inner.root.join(BLOBS), &target, |file| {
                    file.write_all(&bytes)
                })
            });
            if written.is_err() {
                unclaim(&inner.pending, &key);
            }
            written
        })
        .await
    }

    async fn release(&self, hash: &BlobHash) -> Result<(), Error> {
        let inner = Arc::clone(&self.inner);
        let key = hash.as_str().to_owned();
        blocking(move || {
            let _links = lock(&inner.links);
            unclaim(&inner.pending, &key);
            let named: bool = inner.rows(|conn| {
                conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM artifact_blobs WHERE hash = ?1)",
                    [&key],
                    |row| row.get(0),
                )
                .map_err(internal)
            })?;
            if !named {
                forget(&inner, vec![key]);
            }
            Ok(())
        })
        .await
    }

    async fn link(
        &self,
        artifact: ArtifactId,
        version: VersionNumber,
        name: &ArtifactPath,
        hash: &BlobHash,
    ) -> Result<(), Error> {
        let inner = Arc::clone(&self.inner);
        let name = name.clone();
        let hash = hash.as_str().to_owned();
        blocking(move || {
            let _links = lock(&inner.links);
            let file = (artifact, version, &name);
            link_file(&inner, file, &hash)?;
            let gone = inner.rows(|conn| link_rows(conn, file, &hash).map_err(internal))?;
            forget(&inner, gone);
            Ok(())
        })
        .await
    }

    async fn read(
        &self,
        artifact: ArtifactId,
        version: VersionNumber,
        name: &ArtifactPath,
    ) -> Result<Vec<u8>, Error> {
        let root = self.inner.root.clone();
        let name = name.clone();
        blocking(move || {
            let missing = || Error::not_found(format!("{name} of version {version} of {artifact}"));
            let Some(segments) = segments(&name) else {
                return Err(missing());
            };
            let mut path = root.join(artifact.as_str());
            let version_dir = version.to_string();
            let (file, parents) = segments.split_last().ok_or_else(missing)?;
            for segment in core::iter::once(version_dir.as_str()).chain(parents.iter().copied()) {
                if !is_dir(&path)? {
                    return Err(missing());
                }
                path.push(segment);
            }
            if !is_dir(&path)? {
                return Err(missing());
            }
            path.push(file);
            if !is_file(&path)? {
                return Err(missing());
            }
            fs::read(&path).map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => missing(),
                _ => io_failure("read", &path, error),
            })
        })
        .await
    }

    async fn remove_version(
        &self,
        artifact: ArtifactId,
        version: VersionNumber,
    ) -> Result<(), Error> {
        let inner = Arc::clone(&self.inner);
        blocking(move || {
            let _links = lock(&inner.links);
            let gone =
                inner.rows(|conn| unlink(conn, artifact, Some(version)).map_err(internal))?;
            remove_tree(&version_dir(&inner.root, artifact, version));
            remove_if_empty(&inner.root.join(artifact.as_str()));
            forget(&inner, gone);
            Ok(())
        })
        .await
    }

    async fn remove_artifact(&self, artifact: ArtifactId) -> Result<(), Error> {
        let inner = Arc::clone(&self.inner);
        blocking(move || {
            let _links = lock(&inner.links);
            let gone = inner.rows(|conn| unlink(conn, artifact, None).map_err(internal))?;
            remove_tree(&inner.root.join(artifact.as_str()));
            forget(&inner, gone);
            Ok(())
        })
        .await
    }
}

/// Runs `work` on a blocking thread.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(Error::internal)?
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Counts a `put` of `hash` as pending and answers whether its file
/// `target` is already there. Under the lock [`forget`] holds, so either
/// the file goes before this looks, or it stays.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the look at the file must happen under the lock"
)]
fn claim(
    pending: &Mutex<HashMap<String, usize>>,
    hash: &str,
    target: &Path,
) -> Result<bool, Error> {
    let mut pending = lock(pending);
    let count = pending.entry(hash.to_owned()).or_default();
    *count = count.saturating_add(1);
    is_file(target)
}

/// One `put` of `hash` waits no more (released, or failed).
fn unclaim(pending: &Mutex<HashMap<String, usize>>, hash: &str) {
    let mut pending = lock(pending);
    if let Some(count) = pending.get_mut(hash) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            let _released = pending.remove(hash);
        }
    }
}

/// An I/O failure, logged with its path; callers see only "internal".
fn io_failure(what: &str, path: &Path, error: io::Error) -> Error {
    tracing::error!(%error, path = %path.display(), "artifact blobs: cannot {what}");
    Error::internal(error)
}

/// Whether `path` is a regular file (never through a symbolic link).
fn is_file(path: &Path) -> Result<bool, Error> {
    kind(path).map(|meta| meta.is_some_and(|meta| meta.is_file()))
}

/// Whether `path` is a directory (never through a symbolic link).
fn is_dir(path: &Path) -> Result<bool, Error> {
    kind(path).map(|meta| meta.is_some_and(|meta| meta.is_dir()))
}

/// The metadata of `path` itself (a symbolic link is not followed),
/// `None` when nothing is there.
fn kind(path: &Path) -> Result<Option<fs::Metadata>, Error> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_failure("inspect", path, error)),
    }
}

/// The segments of `name`, `None` when one would leave its directory.
/// The domain never makes such a name ([`ArtifactPath`]); the adapter
/// does not take that on trust.
fn segments(name: &ArtifactPath) -> Option<Vec<&str>> {
    let segments: Vec<&str> = name.as_str().split('/').collect();
    segments
        .iter()
        .all(|segment| !matches!(*segment, "" | "." | "..") && !segment.contains('\0'))
        .then_some(segments)
}

fn version_dir(root: &Path, artifact: ArtifactId, version: VersionNumber) -> PathBuf {
    root.join(artifact.as_str()).join(version.to_string())
}

/// Whether the directory under `root` takes hard links: a temporary file
/// linked under a second name.
fn probe_hard_links(dir: &Path) -> Result<bool, Error> {
    let probe = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(dir)
        .map_err(|error| io_failure("create a file in", dir, error))?;
    let mut twin = probe.path().as_os_str().to_owned();
    twin.push("-link");
    let twin = PathBuf::from(twin);
    match fs::hard_link(probe.path(), &twin) {
        Ok(()) => {
            if let Err(error) = fs::remove_file(&twin) {
                tracing::warn!(%error, path = %twin.display(), "artifact blobs: cannot remove the probe");
            }
            Ok(true)
        }
        Err(error) => {
            tracing::warn!(%error, dir = %dir.display(), "artifact blobs: no hard links here, versions copy their blobs");
            Ok(false)
        }
    }
}

/// Writes what `fill` writes to a temporary file in `dir` and renames it
/// to `target`: when `fill` fails half-way, `target` is untouched.
fn write_atomically(
    dir: &Path,
    target: &Path,
    fill: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> Result<(), Error> {
    let mut temp = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(dir)
        .map_err(|error| io_failure("create a file in", dir, error))?;
    fill(temp.as_file_mut())
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|error| io_failure("write", temp.path(), error))?;
    temp.persist(target)
        .map(drop)
        .map_err(|error| io_failure("rename into", target, error.error))
}

/// The file `name` of a version: its directories created (never through a
/// symbolic link), and the blob linked or copied to it, replacing what
/// was there.
fn link_file(
    inner: &Inner,
    (artifact, version, name): (ArtifactId, VersionNumber, &ArtifactPath),
    hash: &str,
) -> Result<(), Error> {
    let blob = inner.blob(hash);
    if !is_file(&blob)? {
        return Err(Error::not_found(format!("blob {hash}")));
    }
    let escapes = || {
        tracing::error!(%artifact, %version, %name, "artifact blobs: the file would leave its version's directory");
        Error::internal_msg(format!("{name} leaves version {version} of {artifact}"))
    };
    let segments = segments(name).ok_or_else(escapes)?;
    let (file, parents) = segments.split_last().ok_or_else(escapes)?;
    let version_number = version.to_string();
    let mut dir = inner.root.clone();
    for segment in [artifact.as_str(), version_number.as_str()]
        .into_iter()
        .chain(parents.iter().copied())
    {
        dir.push(segment);
        match kind(&dir)? {
            Some(meta) if meta.is_dir() => {}
            Some(_) => return Err(escapes()),
            None => fs::create_dir(&dir).map_err(|error| io_failure("create", &dir, error))?,
        }
    }
    let path = dir.join(file);
    if is_dir(&path)? {
        return Err(Error::conflict(format!(
            "{name} is a directory of version {version} of {artifact}"
        )));
    }
    place(&blob, &dir, &path, inner.hard_links)
}

/// The rows of the file `name` of a version naming the blob `hash`, in
/// one transaction. Answers the blobs no link names any more (the one an
/// earlier link of the name had); linking a name to the blob it already
/// names counts nothing.
fn link_rows(
    conn: &mut Connection,
    (artifact, version, name): (ArtifactId, VersionNumber, &ArtifactPath),
    hash: &str,
) -> rusqlite::Result<Vec<String>> {
    let tx = conn.transaction()?;
    let previous: Option<String> = tx
        .query_row(
            "SELECT hash FROM artifact_blob_links \
             WHERE artifact_id = ?1 AND number = ?2 AND name = ?3",
            params![artifact.as_str(), version.get(), name.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let mut gone = Vec::new();
    if previous.as_deref() != Some(hash) {
        let _counted = tx.execute(
            "INSERT INTO artifact_blobs (hash, refs) VALUES (?1, 1) \
             ON CONFLICT (hash) DO UPDATE SET refs = refs + 1",
            [hash],
        )?;
        let _linked = tx.execute(
            "INSERT INTO artifact_blob_links (artifact_id, number, name, hash) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (artifact_id, number, name) DO UPDATE SET hash = excluded.hash",
            params![artifact.as_str(), version.get(), name.as_str(), hash],
        )?;
        if let Some(previous) = previous
            && decrement(&tx, &previous)?
        {
            gone.push(previous);
        }
    }
    tx.commit()?;
    Ok(gone)
}

/// Makes `path` (a file in `dir`) a hard link to, or a copy of, `blob`,
/// replacing what was there in one rename. The temporary name is random,
/// never derived from the file's own (which may be as long as a name on
/// disk can be).
fn place(blob: &Path, dir: &Path, path: &Path, hard_links: bool) -> Result<(), Error> {
    if hard_links {
        let link = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .make_in(dir, |temp| fs::hard_link(blob, temp))
            .map_err(|error| io_failure("link into", dir, error))?;
        link.persist(path)
            .map_err(|error| io_failure("rename into", path, error.error))
    } else {
        let mut copy = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempfile_in(dir)
            .map_err(|error| io_failure("create a file in", dir, error))?;
        fs::File::open(blob)
            .and_then(|mut source| io::copy(&mut source, &mut copy))
            .and_then(|_copied| copy.as_file().sync_all())
            .map_err(|error| io_failure("copy", blob, error))?;
        copy.persist(path)
            .map(drop)
            .map_err(|error| io_failure("rename into", path, error.error))
    }
}

/// Takes one link off `hash`'s count; `true` when that was the last one
/// (the row is gone).
fn decrement(tx: &Transaction<'_>, hash: &str) -> rusqlite::Result<bool> {
    let kept = tx.execute(
        "UPDATE artifact_blobs SET refs = refs - 1 WHERE hash = ?1 AND refs > 1",
        [hash],
    )?;
    if kept > 0 {
        return Ok(false);
    }
    Ok(tx.execute("DELETE FROM artifact_blobs WHERE hash = ?1", [hash])? > 0)
}

/// Removes the link rows of version `version` of `artifact` (of every
/// version for `None`), answering the blobs no link names any more.
fn unlink(
    conn: &mut Connection,
    artifact: ArtifactId,
    version: Option<VersionNumber>,
) -> rusqlite::Result<Vec<String>> {
    let number = version.map(VersionNumber::get);
    let tx = conn.transaction()?;
    let hashes: Vec<String> = {
        let mut query = tx.prepare(
            "SELECT hash FROM artifact_blob_links \
             WHERE artifact_id = ?1 AND (?2 IS NULL OR number = ?2)",
        )?;
        query
            .query_map(params![artifact.as_str(), number], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let _unlinked = tx.execute(
        "DELETE FROM artifact_blob_links WHERE artifact_id = ?1 AND (?2 IS NULL OR number = ?2)",
        params![artifact.as_str(), number],
    )?;
    let mut gone = Vec::new();
    for hash in hashes {
        if decrement(&tx, &hash)? {
            gone.push(hash);
        }
    }
    tx.commit()?;
    Ok(gone)
}

/// Removes the files of blobs `gone` (their rows went), but those a
/// `put` waits on. A failure is logged: the blob is then an unlinked
/// one, which opening collects.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the files go under the lock `claim` looks at them with"
)]
fn forget(inner: &Inner, gone: Vec<String>) {
    let pending = lock(&inner.pending);
    for hash in gone {
        if pending.contains_key(&hash) {
            continue;
        }
        let path = inner.blob(&hash);
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, path = %path.display(), "artifact blobs: cannot remove a blob no version links");
        }
    }
}

/// Removes the directory `path` and what is in it (a symbolic link is
/// removed, never followed); nothing when it is not there. Best-effort,
/// after the rows went: a failure is logged, and opening collects the
/// directory (a version without rows).
fn remove_tree(path: &Path) {
    if let Err(error) = fs::remove_dir_all(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %path.display(), "artifact blobs: cannot remove a directory without rows");
    }
}

/// Removes the directory `path` if it is empty.
fn remove_if_empty(path: &Path) {
    if let Err(error) = fs::remove_dir(path) {
        tracing::trace!(%error, path = %path.display(), "artifact blobs: directory kept");
    }
}

/// The names in `dir` with their metadata (symbolic links not followed);
/// none when `dir` is not there.
fn entries(dir: &Path) -> Result<Vec<(String, fs::Metadata)>, Error> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_failure("read", dir, error)),
    };
    let mut entries = Vec::new();
    for entry in read {
        let entry = entry.map_err(|error| io_failure("read", dir, error))?;
        let meta = entry
            .metadata()
            .map_err(|error| io_failure("inspect", &entry.path(), error))?;
        if let Ok(name) = entry.file_name().into_string() {
            entries.push((name, meta));
        }
    }
    Ok(entries)
}

/// Removes the links and the directories of versions that have no row in
/// `artifact_versions`: what pruning's best-effort removal of files, or a
/// version whose write was taken back, left. A directory that cannot be
/// read or removed is logged and kept for the next start.
fn collect_versions(inner: &Inner) -> Result<(), Error> {
    let root = &inner.root;
    let mut found: BTreeSet<(ArtifactId, VersionNumber)> = BTreeSet::new();
    for (name, meta) in entries(root)? {
        let Ok(artifact) = ArtifactId::parse(&name) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Ok(numbers) = entries(&root.join(&name)) else {
            continue;
        };
        for (number, meta) in numbers {
            if let Some(version) = number
                .parse()
                .ok()
                .and_then(|number| VersionNumber::new(number).ok())
                && meta.is_dir()
            {
                let _new = found.insert((artifact, version));
            }
        }
    }
    let linked = inner.rows(|conn| {
        let mut query = conn
            .prepare("SELECT DISTINCT artifact_id, number FROM artifact_blob_links")
            .map_err(internal)?;
        query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
            })
            .map_err(internal)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)
    })?;
    for (artifact, number) in linked {
        if let (Ok(artifact), Ok(version)) =
            (ArtifactId::parse(&artifact), VersionNumber::new(number))
        {
            let _new = found.insert((artifact, version));
        }
    }
    for (artifact, version) in found {
        let gone = inner.rows(|conn| {
            let kept: bool = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM artifact_versions \
                     WHERE artifact_id = ?1 AND number = ?2)",
                    params![artifact.as_str(), version.get()],
                    |row| row.get(0),
                )
                .map_err(internal)?;
            if kept {
                return Ok(None);
            }
            unlink(conn, artifact, Some(version))
                .map(Some)
                .map_err(internal)
        })?;
        let Some(gone) = gone else {
            continue;
        };
        tracing::info!(%artifact, %version, "artifact blobs: removing the files of a version without rows");
        remove_tree(&version_dir(root, artifact, version));
        remove_if_empty(&root.join(artifact.as_str()));
        forget(inner, gone);
    }
    Ok(())
}

/// Removes, from `blobs/`, the blobs no link names and the temporary
/// files, both older than [`LEFTOVER_AGE`]; one that cannot be removed is
/// logged and kept for the next start.
fn collect_blobs(inner: &Inner) -> Result<(), Error> {
    let dir = inner.root.join(BLOBS);
    let now = SystemTime::now();
    for (name, meta) in entries(&dir)? {
        let old = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > LEFTOVER_AGE);
        if !old || !meta.is_file() {
            continue;
        }
        if BlobHash::parse(&name).is_ok() {
            let linked: bool = inner.rows(|conn| {
                conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM artifact_blobs WHERE hash = ?1)",
                    [&name],
                    |row| row.get(0),
                )
                .map_err(internal)
            })?;
            if linked {
                continue;
            }
        }
        let path = dir.join(&name);
        tracing::info!(path = %path.display(), "artifact blobs: removing a leftover");
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, path = %path.display(), "artifact blobs: cannot remove a leftover");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    mod write_atomically {
        use super::super::{entries, io, write_atomically};
        use std::io::Write as _;

        // TC: BLOB-01 (a write that fails half-way leaves no blob)
        #[test]
        fn leaves_no_blob_when_write_fails_half_way() {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("blob");
            let written = write_atomically(dir.path(), &target, |file| {
                file.write_all(b"half of it")?;
                Err(io::Error::other("interrupted"))
            });
            assert_eq!(
                (written.is_err(), entries(dir.path()).unwrap().len()),
                (true, 0),
                "nothing is left in the directory, not even the temporary file"
            );
        }
    }
}
