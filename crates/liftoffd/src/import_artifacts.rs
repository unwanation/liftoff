//! `liftoffd import-artifacts <dir>`: the import of the standalone artifacts server.
//!
//! `<dir>` is the predecessor's `DATA_DIR`: its SQLite database
//! `<dir>/artifacts.db` (read-only; the schema of its
//! `packages/core/src/migrations/0001_init.sql`, times in epoch
//! milliseconds) and its files `<dir>/artifacts/<id>/index.html` with the
//! relative files beside it. Every artifact becomes an artifact of
//! liftoff with the same id (both are 12 base62 characters; another id is
//! skipped with a logged reason) and one version: the owner is the one
//! user whose e-mail is the predecessor owner's `users.email` (none, or
//! several: the admin), a `shared` one is [`Visibility::Shared`](liftoff_core::domain::Visibility)
//! for good, a private one keeps its `expires_at` (an expired one is left
//! to the sweeper), the title is kept as is, version 1 is created at the
//! predecessor's `created_at`, and every file becomes a file of the
//! version through [`Blobs`] by its relative path. An id the store
//! already has is skipped and counted, so a second run changes nothing;
//! one an interrupted run left at version 1 without its `index.html` is
//! written again.
//! A row without its directory or its `index.html` is skipped and
//! counted too; a file whose path liftoff refuses (a hidden segment, a
//! name that is not UTF-8) or that is no regular file is left out of its
//! version. Every reason is logged on stderr.
//!
//! The rules are the core's [`ArtifactImport`]; this module reads the
//! predecessor and prints. The command runs while the daemon is
//! stopped, like `setup --import`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use liftoff_core::domain::{ArtifactId, ArtifactPath, Timestamp};
use liftoff_core::ports::{Blobs, Store};
use liftoff_core::service::{ArtifactImport, ArtifactUpload, ImportOutcome, ImportedArtifact};
use liftoff_store::{FileBlobs, LinkMode, SqliteStore};
use rusqlite::{Connection, OpenFlags};

use crate::config::AppConfig;
use crate::error::AppError;

/// The predecessor's database, under `<dir>`.
const DATABASE: &str = "artifacts.db";
/// The predecessor's files, under `<dir>`: one directory per artifact.
const FILES: &str = "artifacts";

/// `liftoffd import-artifacts <dir>`.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct ImportArtifactsArgs {
    /// The data directory of the standalone artifacts server (its `DATA_DIR`): the
    /// database `artifacts.db` and the files under `artifacts/`. Run it
    /// while the daemon is stopped.
    #[arg(value_name = "DIR")]
    pub dir: PathBuf,
}

/// What a run of the import did (printed on stderr).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactsSummary {
    /// Artifacts written.
    pub imported: usize,
    /// Artifacts left out: their id is already in the store, or is no
    /// artifact id, or their files are missing.
    pub skipped: usize,
    /// Imported artifacts whose owner's e-mail named no user, or more
    /// than one, now the admin's.
    pub owners_defaulted: usize,
}

impl core::fmt::Display for ArtifactsSummary {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "imported {} artifacts, skipped {}, owners defaulted to the admin {}",
            self.imported, self.skipped, self.owners_defaulted
        )
    }
}

/// Why an import stopped.
#[derive(Debug, thiserror::Error)]
pub enum ImportArtifactsError {
    /// `<dir>` is not a directory.
    #[error("{} is not a directory", path.display())]
    MissingDirectory {
        /// The directory given.
        path: PathBuf,
    },
    /// `<dir>/artifacts.db` does not exist.
    #[error("no the standalone artifacts server database at {}", path.display())]
    MissingDatabase {
        /// The database path looked at.
        path: PathBuf,
    },
    /// The predecessor's database could not be read.
    #[error("cannot read the standalone artifacts server database at {}", path.display())]
    Database {
        /// The database.
        path: PathBuf,
        /// Why not.
        #[source]
        source: rusqlite::Error,
    },
    /// The predecessor's files could not be read.
    #[error("cannot read {}", path.display())]
    Read {
        /// What was being read.
        path: PathBuf,
        /// Why not.
        #[source]
        source: std::io::Error,
    },
    /// The store or the blobs failed.
    #[error("failed to write the imported artifacts")]
    Store(#[source] liftoff_core::Error),
}

impl ImportArtifactsError {
    /// The exit code: `1` for a missing input, `70` (`EX_SOFTWARE`) when
    /// reading or writing failed.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::MissingDirectory { .. } | Self::MissingDatabase { .. } => 1,
            Self::Database { .. } | Self::Read { .. } | Self::Store(_) => 70,
        }
    }
}

/// A row of the predecessor's `artifacts`, with its owner's e-mail.
struct Row {
    id: String,
    owner_email: Option<String>,
    title: String,
    shared: bool,
    created_ms: i64,
    updated_ms: i64,
    expires_ms: Option<i64>,
}

/// The files of one artifact directory.
enum Files {
    /// No directory, or no `index.html` in it: why.
    Missing(&'static str),
    /// The HTML and the files beside it.
    Found {
        html: Vec<u8>,
        files: Vec<ArtifactUpload>,
    },
}

/// The database under `dir`, once `dir` and it are there.
///
/// # Errors
///
/// [`ImportArtifactsError::MissingDirectory`],
/// [`ImportArtifactsError::MissingDatabase`].
fn database_of(dir: &Path) -> Result<PathBuf, ImportArtifactsError> {
    if !dir.is_dir() {
        return Err(ImportArtifactsError::MissingDirectory {
            path: dir.to_path_buf(),
        });
    }
    let database = dir.join(DATABASE);
    if !database.is_file() {
        return Err(ImportArtifactsError::MissingDatabase { path: database });
    }
    Ok(database)
}

/// Every row of `artifacts` in `database`, opened read-only, oldest
/// first.
fn read_rows(database: &Path) -> Result<Vec<Row>, ImportArtifactsError> {
    let failed = |source| ImportArtifactsError::Database {
        path: database.to_path_buf(),
        source,
    };
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(failed)?;
    let mut statement = connection
        .prepare(
            "SELECT a.id, u.email, a.title, a.shared, a.created_at, a.updated_at, a.expires_at \
             FROM artifacts a LEFT JOIN users u ON u.id = a.owner_id \
             ORDER BY a.created_at, a.id",
        )
        .map_err(failed)?;
    let rows = statement
        .query_map([], |row| {
            Ok(Row {
                id: row.get(0)?,
                owner_email: row.get(1)?,
                title: row.get(2)?,
                shared: row.get::<_, i64>(3)? != 0,
                created_ms: row.get(4)?,
                updated_ms: row.get(5)?,
                expires_ms: row.get(6)?,
            })
        })
        .map_err(failed)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(failed)?;
    Ok(rows)
}

/// The instant `ms` epoch milliseconds name (the predecessor's
/// `Date.now()`), when there is one.
fn instant(ms: i64) -> Option<Timestamp> {
    let nanos = i128::from(ms).checked_mul(1_000_000)?;
    Timestamp::from_unix_timestamp_nanos(nanos).ok()
}

/// The files under `root` (the directory of one artifact): its
/// `index.html` and every other regular file by its relative path. A
/// file liftoff cannot name, or that is no regular file, is left out
/// with a logged reason.
fn read_files(id: &str, root: &Path) -> Result<Files, ImportArtifactsError> {
    let read = |path: &Path| {
        let path = path.to_path_buf();
        move |source| ImportArtifactsError::Read { path, source }
    };
    if !root.is_dir() {
        return Ok(Files::Missing("its directory is missing"));
    }
    let mut html = None;
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(read(&dir))? {
            let entry = entry.map_err(read(&dir))?;
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                tracing::warn!(artifact = id, path = %path.display(), "left out a file whose name is not UTF-8");
                continue;
            };
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let kind = entry.file_type().map_err(read(&path))?;
            if kind.is_dir() {
                pending.push((path, relative));
            } else if kind.is_symlink() || !path.is_file() {
                tracing::warn!(artifact = id, file = %relative, "left out a file that is no regular file");
            } else if relative == ArtifactPath::INDEX {
                html = Some(std::fs::read(&path).map_err(read(&path))?);
            } else {
                match ArtifactPath::new(&relative) {
                    Ok(name) => files.push(ArtifactUpload {
                        name,
                        bytes: std::fs::read(&path).map_err(read(&path))?,
                    }),
                    Err(error) => {
                        tracing::warn!(artifact = id, file = %relative, %error, "left out a file liftoff cannot name");
                    }
                }
            }
        }
    }
    Ok(html.map_or(Files::Missing("it has no index.html"), |html| {
        Files::Found { html, files }
    }))
}

/// Runs the blocking `work` off the async runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ImportArtifactsError> + Send + 'static,
) -> Result<T, ImportArtifactsError> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        ImportArtifactsError::Store(liftoff_core::Error::internal_msg(format!(
            "the import's reading task ended: {error}"
        )))
    })?
}

/// Imports the artifacts of the standalone artifacts server under `dir` into `store` and
/// `blobs` (see the module documentation).
///
/// # Errors
///
/// [`ImportArtifactsError::MissingDirectory`] or
/// [`ImportArtifactsError::MissingDatabase`] before anything is written;
/// [`ImportArtifactsError::Database`], [`ImportArtifactsError::Read`]
/// and [`ImportArtifactsError::Store`] when reading or writing fails (the
/// artifacts imported until then stay; a new run skips them).
pub async fn import_artifacts<S: Store, B: Blobs>(
    store: &S,
    blobs: &B,
    dir: &Path,
) -> Result<ArtifactsSummary, ImportArtifactsError> {
    let database = database_of(dir)?;
    let rows = blocking(move || read_rows(&database)).await?;
    let import = ArtifactImport::new(store, blobs)
        .await
        .map_err(ImportArtifactsError::Store)?;
    let mut summary = ArtifactsSummary::default();
    for row in rows {
        let Ok(id) = ArtifactId::parse(&row.id) else {
            tracing::warn!(artifact = %row.id, "skipped: the id is not 12 base62 characters");
            summary.skipped = summary.skipped.saturating_add(1);
            continue;
        };
        if import
            .is_stored(id)
            .await
            .map_err(ImportArtifactsError::Store)?
        {
            tracing::info!(artifact = %id, "skipped: already imported");
            summary.skipped = summary.skipped.saturating_add(1);
            continue;
        }
        let expires_at = row.expires_ms.map(|ms| instant(ms).ok_or(()));
        let (Some(created_at), Some(updated_at), Ok(expires_at)) = (
            instant(row.created_ms),
            instant(row.updated_ms),
            expires_at.transpose(),
        ) else {
            tracing::warn!(artifact = %id, "skipped: a time is out of range");
            summary.skipped = summary.skipped.saturating_add(1);
            continue;
        };
        let root = dir.join(FILES).join(&row.id);
        let raw = row.id.clone();
        let (html, files) = match blocking(move || read_files(&raw, &root)).await? {
            Files::Found { html, files } => (html, files),
            Files::Missing(why) => {
                tracing::warn!(artifact = %id, "skipped: {why}");
                summary.skipped = summary.skipped.saturating_add(1);
                continue;
            }
        };
        let outcome = import
            .import(ImportedArtifact {
                id,
                owner_email: row.owner_email,
                title: row.title,
                shared: row.shared,
                created_at,
                updated_at,
                expires_at,
                html,
                files,
            })
            .await
            .map_err(ImportArtifactsError::Store)?;
        match outcome {
            ImportOutcome::Imported { owner_defaulted } => {
                summary.imported = summary.imported.saturating_add(1);
                if owner_defaulted {
                    tracing::info!(artifact = %id, "no user has the owner's e-mail: the admin owns it");
                    summary.owners_defaulted = summary.owners_defaulted.saturating_add(1);
                }
            }
            ImportOutcome::AlreadyStored => {
                summary.skipped = summary.skipped.saturating_add(1);
            }
        }
    }
    Ok(summary)
}

/// The subcommand `import-artifacts`.
///
/// It loads the configuration `config` (else the default file), checks
/// `args.dir`, opens the store and the blobs under
/// `paths.data/artifacts`, imports `args.dir` and prints the
/// [`ArtifactsSummary`] on stderr, with the reasons of what it skipped.
///
/// # Errors
///
/// [`AppError::ImportArtifacts`] when the import stops (exit 1 for a
/// missing directory or database), the configuration and setup errors of
/// opening the store otherwise.
pub async fn run(config: Option<&Path>, args: &ImportArtifactsArgs) -> Result<(), AppError> {
    let config = AppConfig::load(config)?;
    // Another subscriber (a test's) may be installed already; the log
    // then goes there.
    match crate::telemetry::init_stderr(&config.log.level) {
        Ok(()) | Err(crate::telemetry::InitError::AlreadySet(_)) => {}
        Err(error) => return Err(error.into()),
    }
    // The input is checked before the store is opened (and maybe
    // created).
    database_of(&args.dir)?;
    let setup = |what: String| move |source| AppError::Setup { what, source };
    let store = SqliteStore::open(&config.store.path).map_err(setup(format!(
        "the database at {}",
        config.store.path.display()
    )))?;
    let artifacts = config.paths.data.join("artifacts");
    let blobs = FileBlobs::open(&store, &artifacts, LinkMode::Detect)
        .await
        .map_err(setup(format!(
            "the artifact files under {}",
            artifacts.display()
        )))?;
    let summary = import_artifacts(&store, &blobs, &args.dir).await?;
    let _ignored: Result<(), std::io::Error> = writeln!(std::io::stderr().lock(), "{summary}");
    Ok(())
}
