#![cfg(test)]
//! The import of the standalone artifacts server (`IMPA-01…04`): a predecessor data directory built here from its schema
//! (`packages/core/src/migrations/0001_init.sql`, times in epoch
//! milliseconds; files at `artifacts/<id>/index.html` and the relative
//! files beside it), imported into the daemon's `SQLite` store and its
//! blobs on disk, through [`liftoffd::import_artifacts::import_artifacts`]
//! and through `liftoffd import-artifacts <dir>` ([`liftoffd::run_cli`]).

use std::path::{Path, PathBuf};

use liftoff_core::domain::{
    Artifact, ArtifactId, ArtifactPath, BlobHash, Timestamp, User, UserRole, VersionNumber,
    Visibility,
};
use liftoff_core::fakes::World;
use liftoff_core::ports::{Blobs as _, Store as _};
use liftoff_store::{FileBlobs, LinkMode, SqliteStore};
use liftoffd::import_artifacts::{ArtifactsSummary, ImportArtifactsError, import_artifacts};
use rusqlite::{Connection, params};

/// The predecessor's schema, `packages/core/src/migrations/0001_init.sql`
/// of the standalone artifacts server, verbatim.
const PREDECESSOR_SCHEMA: &str = "
CREATE TABLE users (
  id TEXT PRIMARY KEY,
  oidc_sub TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  email TEXT,
  created_at INTEGER NOT NULL
);

CREATE TABLE sessions (
  token_hash TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX sessions_expires ON sessions(expires_at);

CREATE TABLE api_tokens (
  id TEXT PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER
);
CREATE INDEX api_tokens_user ON api_tokens(user_id);

CREATE TABLE artifacts (
  id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  title TEXT NOT NULL,
  shared INTEGER NOT NULL DEFAULT 0,
  size_bytes INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  expires_at INTEGER
);
CREATE INDEX artifacts_owner ON artifacts(owner_id, created_at);
CREATE INDEX artifacts_expires ON artifacts(expires_at);
";

/// The predecessor user whose e-mail is the daemon's `alice`'s.
const ALICE: &str = "0192f0a0-0000-7000-8000-00000000a11c";
/// A predecessor user whose e-mail no daemon user has.
const STRANGER: &str = "0192f0a0-0000-7000-8000-0000000057a6";
/// A predecessor user without an e-mail.
const NAMELESS: &str = "0192f0a0-0000-7000-8000-00000000000e";

/// A shared artifact of `alice` with files beside its HTML.
const SHARED_ID: &str = "aB3dE6gH9jK2";
/// A private artifact of `alice` that expires in 2100.
const PRIVATE_ID: &str = "Zx9Yw8Vu7Ts6";
/// A private artifact whose TTL ran out in 2001.
const EXPIRED_ID: &str = "q1W2e3R4t5Y6";

const SHARED_HTML: &[u8] =
    b"<!doctype html><title>Report</title><link rel=stylesheet href=style.css>";
const STYLE_CSS: &[u8] = b"body { color: red }";
const LOGO_PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff, 0x10,
];
const DATA_JSON: &[u8] = b"{\"rows\":[1,2,3]}";

/// A predecessor row of `artifacts`, times in epoch milliseconds.
struct Row<'a> {
    id: &'a str,
    owner: &'a str,
    title: &'a str,
    shared: bool,
    created_ms: i64,
    updated_ms: i64,
    expires_ms: Option<i64>,
}

/// The predecessor's `DATA_DIR`: `artifacts.db` and `artifacts/<id>/`.
struct Predecessor {
    dir: tempfile::TempDir,
}

impl Predecessor {
    /// An empty data directory with the schema and the users `ALICE`
    /// (`alice@example.com`), `STRANGER` (`stranger@elsewhere.test`) and
    /// `NAMELESS` (no e-mail).
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("artifacts")).unwrap();
        let predecessor = Self { dir };
        let db = predecessor.db();
        db.execute_batch(PREDECESSOR_SCHEMA).unwrap();
        for (id, sub, name, email) in [
            (ALICE, "sub-alice", "Alice", Some("alice@example.com")),
            (
                STRANGER,
                "sub-stranger",
                "Stranger",
                Some("stranger@elsewhere.test"),
            ),
            (NAMELESS, "sub-nameless", "Nameless", None),
        ] {
            db.execute(
                "INSERT INTO users (id, oidc_sub, name, email, created_at) VALUES (?, ?, ?, ?, ?)",
                params![id, sub, name, email, 1_760_000_000_000_i64],
            )
            .unwrap();
        }
        predecessor
    }

    /// The fixture of most tests: `SHARED_ID` with four files,
    /// `PRIVATE_ID` and `EXPIRED_ID`, all of `ALICE`.
    fn with_three() -> Self {
        let predecessor = Self::new();
        predecessor.artifact(
            &Row {
                id: SHARED_ID,
                owner: ALICE,
                title: "Report",
                shared: true,
                created_ms: 1_790_000_000_123,
                updated_ms: 1_790_000_500_456,
                expires_ms: None,
            },
            &[
                ("index.html", SHARED_HTML),
                ("style.css", STYLE_CSS),
                ("img/logo.png", LOGO_PNG),
                ("nested/deep/data.json", DATA_JSON),
            ],
        );
        predecessor.artifact(
            &Row {
                id: PRIVATE_ID,
                owner: ALICE,
                title: "Draft",
                shared: false,
                created_ms: 1_791_000_000_000,
                updated_ms: 1_791_000_000_000,
                expires_ms: Some(4_102_444_800_000),
            },
            &[("index.html", b"<p>draft</p>")],
        );
        predecessor.artifact(
            &Row {
                id: EXPIRED_ID,
                owner: ALICE,
                title: "Old",
                shared: false,
                created_ms: 999_000_000_000,
                updated_ms: 999_000_000_000,
                expires_ms: Some(1_000_000_000_000),
            },
            &[("index.html", b"<p>old</p>")],
        );
        predecessor
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn db(&self) -> Connection {
        Connection::open(self.path().join("artifacts.db")).unwrap()
    }

    /// Writes `row` and its files under `artifacts/<id>/`; `size_bytes`
    /// is their sum, as the predecessor keeps it.
    fn artifact(&self, row: &Row<'_>, files: &[(&str, &[u8])]) {
        let root = self.path().join("artifacts").join(row.id);
        for (name, bytes) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let size: usize = files.iter().map(|(_name, bytes)| bytes.len()).sum();
        self.db()
            .execute(
                "INSERT INTO artifacts (id, owner_id, title, shared, size_bytes, created_at, \
                 updated_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    row.id,
                    row.owner,
                    row.title,
                    i64::from(row.shared),
                    i64::try_from(size).unwrap(),
                    row.created_ms,
                    row.updated_ms,
                    row.expires_ms
                ],
            )
            .unwrap();
    }
}

/// The daemon's store and blobs over a fresh data directory with the
/// users `owner` (the admin, `owner@example.com`) and `alice`
/// (`alice@example.com`).
struct Daemon {
    _data: tempfile::TempDir,
    store: SqliteStore,
    blobs: FileBlobs,
    admin: User,
    alice: User,
}

async fn daemon() -> Daemon {
    let world = World::new().await;
    let data = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(data.path().join("liftoff.db")).unwrap();
    let admin = world.owner.clone();
    let alice = world.records.user("alice", UserRole::User).unwrap();
    store.insert_user(&admin).await.unwrap();
    store.insert_user(&alice).await.unwrap();
    let blobs = FileBlobs::open(&store, data.path().join("artifacts"), LinkMode::Detect)
        .await
        .unwrap();
    Daemon {
        _data: data,
        store,
        blobs,
        admin,
        alice,
    }
}

impl Daemon {
    async fn import(&self, predecessor: &Predecessor) -> ArtifactsSummary {
        import_artifacts(&self.store, &self.blobs, predecessor.path())
            .await
            .unwrap()
    }

    async fn artifact(&self, id: &str) -> Artifact {
        self.store
            .artifact(ArtifactId::parse(id).unwrap())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("no artifact {id}"))
    }

    /// The names of the files of version 1 of `id`.
    async fn file_names(&self, id: &str) -> Vec<String> {
        self.store
            .artifact_version(ArtifactId::parse(id).unwrap(), VersionNumber::FIRST)
            .await
            .unwrap()
            .unwrap()
            .files
            .into_iter()
            .map(|file| file.name.as_str().to_owned())
            .collect()
    }

    async fn file(&self, id: &str, name: &str) -> Vec<u8> {
        self.blobs
            .read(
                ArtifactId::parse(id).unwrap(),
                VersionNumber::FIRST,
                &ArtifactPath::stored(name).unwrap(),
            )
            .await
            .unwrap()
    }
}

/// The row of `SHARED_ID`, shared, of `ALICE`.
fn shared_row() -> Row<'static> {
    Row {
        id: SHARED_ID,
        owner: ALICE,
        title: "Report",
        shared: true,
        created_ms: 1_790_000_000_000,
        updated_ms: 1_790_000_000_000,
        expires_ms: None,
    }
}

/// The instant `ms` epoch milliseconds name.
fn at(ms: i64) -> Timestamp {
    Timestamp::from_unix_timestamp_nanos(i128::from(ms).checked_mul(1_000_000).unwrap()).unwrap()
}

mod import_artifacts {
    use super::*;

    // TC: IMPA-01
    #[tokio::test]
    async fn keeps_every_id_when_imported() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        let mut ids: Vec<String> = daemon
            .store
            .artifacts(None)
            .await
            .unwrap()
            .into_iter()
            .map(|artifact| artifact.id.to_string())
            .collect();
        ids.sort();
        assert_eq!(ids, ["Zx9Yw8Vu7Ts6", "aB3dE6gH9jK2", "q1W2e3R4t5Y6"]);
    }

    // TC: IMPA-01 (`shared` → shared for good)
    #[tokio::test]
    async fn shares_without_expiry_when_shared() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        let artifact = daemon.artifact(SHARED_ID).await;
        assert_eq!(
            (artifact.visibility, artifact.expires_at),
            (Visibility::Shared, None)
        );
    }

    // TC: IMPA-01 (`private` keeps its `expires_at`)
    #[tokio::test]
    async fn keeps_expires_at_when_private() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        let artifact = daemon.artifact(PRIVATE_ID).await;
        assert_eq!(
            (artifact.visibility, artifact.expires_at),
            (Visibility::Private, Some(at(4_102_444_800_000)))
        );
    }

    // TC: IMPA-01 (an expired one is imported and left to the sweeper)
    #[tokio::test]
    async fn imports_as_private_with_its_expiry_when_already_expired() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        let artifact = daemon.artifact(EXPIRED_ID).await;
        assert_eq!(
            (artifact.visibility, artifact.expires_at),
            (Visibility::Private, Some(at(1_000_000_000_000)))
        );
    }

    // TC: IMPA-01 (the title as is, the times of the predecessor,
    // version 1 current)
    #[tokio::test]
    async fn keeps_title_and_times_when_imported() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        let artifact = daemon.artifact(SHARED_ID).await;
        assert_eq!(
            (
                artifact.title,
                artifact.current_version,
                artifact.created_at,
                artifact.updated_at
            ),
            (
                "Report".to_owned(),
                VersionNumber::FIRST,
                at(1_790_000_000_123),
                at(1_790_000_500_456)
            )
        );
    }

    // TC: IMPA-01 (each artifact as version 1)
    #[tokio::test]
    async fn makes_one_version_created_at_the_predecessors_created_at() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        let versions: Vec<_> = daemon
            .store
            .artifact_versions(ArtifactId::parse(SHARED_ID).unwrap())
            .await
            .unwrap()
            .into_iter()
            .map(|version| {
                (
                    version.number,
                    version.title,
                    version.created_at,
                    version.created_by,
                )
            })
            .collect();
        assert_eq!(
            versions,
            [(
                VersionNumber::FIRST,
                "Report".to_owned(),
                at(1_790_000_000_123),
                daemon.alice.id
            )]
        );
    }

    // TC: IMPA-01 (an id that is not 12 base62 characters is skipped
    // and counted)
    #[tokio::test]
    async fn skips_the_row_when_its_id_is_no_artifact_id() {
        let daemon = daemon().await;
        let predecessor = Predecessor::new();
        predecessor.artifact(
            &Row {
                id: "short-id",
                owner: ALICE,
                title: "Odd",
                shared: true,
                created_ms: 1_790_000_000_000,
                updated_ms: 1_790_000_000_000,
                expires_ms: None,
            },
            &[("index.html", b"<p>odd</p>")],
        );
        predecessor.artifact(
            &Row {
                id: SHARED_ID,
                owner: ALICE,
                title: "Report",
                shared: true,
                created_ms: 1_790_000_000_000,
                updated_ms: 1_790_000_000_000,
                expires_ms: None,
            },
            &[("index.html", SHARED_HTML)],
        );
        let summary = daemon.import(&predecessor).await;
        let ids: Vec<String> = daemon
            .store
            .artifacts(None)
            .await
            .unwrap()
            .into_iter()
            .map(|artifact| artifact.id.to_string())
            .collect();
        assert_eq!(
            (summary, ids),
            (
                ArtifactsSummary {
                    imported: 1,
                    skipped: 1,
                    owners_defaulted: 0,
                },
                vec!["aB3dE6gH9jK2".to_owned()]
            )
        );
    }

    // TC: IMPA-03 (a row whose files are gone is skipped and counted)
    #[tokio::test]
    async fn skips_the_row_when_its_directory_is_missing() {
        let daemon = daemon().await;
        let predecessor = Predecessor::new();
        predecessor.artifact(
            &Row {
                id: SHARED_ID,
                owner: ALICE,
                title: "Gone",
                shared: true,
                created_ms: 1_790_000_000_000,
                updated_ms: 1_790_000_000_000,
                expires_ms: None,
            },
            &[],
        );
        let summary = daemon.import(&predecessor).await;
        let stored = daemon
            .store
            .artifact(ArtifactId::parse(SHARED_ID).unwrap())
            .await
            .unwrap();
        assert_eq!(
            (summary, stored),
            (
                ArtifactsSummary {
                    imported: 0,
                    skipped: 1,
                    owners_defaulted: 0,
                },
                None
            )
        );
    }

    // TC: IMPA-03 (a directory without `index.html` is no artifact)
    #[tokio::test]
    async fn skips_the_row_when_its_index_html_is_missing() {
        let daemon = daemon().await;
        let predecessor = Predecessor::new();
        predecessor.artifact(&shared_row(), &[("style.css", STYLE_CSS)]);
        let summary = daemon.import(&predecessor).await;
        let stored = daemon
            .store
            .artifact(ArtifactId::parse(SHARED_ID).unwrap())
            .await
            .unwrap();
        assert_eq!(
            (summary, stored),
            (
                ArtifactsSummary {
                    imported: 0,
                    skipped: 1,
                    owners_defaulted: 0,
                },
                None
            )
        );
    }

    // TC: IMPA-03 (a symlink, to a file or a directory outside the
    // artifact, is never followed: the host file it names is not
    // published)
    #[tokio::test]
    async fn leaves_out_a_symlink_when_the_directory_holds_one() {
        let daemon = daemon().await;
        let predecessor = Predecessor::new();
        predecessor.artifact(
            &shared_row(),
            &[("index.html", SHARED_HTML), ("style.css", STYLE_CSS)],
        );
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("shadow"), b"root:secret").unwrap();
        std::fs::create_dir(host.path().join("etc")).unwrap();
        std::fs::write(host.path().join("etc").join("passwd"), b"root:x:0:0").unwrap();
        let root = predecessor.path().join("artifacts").join(SHARED_ID);
        std::os::unix::fs::symlink(host.path().join("shadow"), root.join("shadow.txt")).unwrap();
        std::os::unix::fs::symlink(host.path().join("etc"), root.join("etc")).unwrap();
        let summary = daemon.import(&predecessor).await;
        assert_eq!(
            (summary.imported, daemon.file_names(SHARED_ID).await),
            (1, ["index.html", "style.css"].map(str::to_owned).to_vec())
        );
    }

    // TC: IMPA-03 (a path with a hidden segment is left out, the rest of
    // the artifact imported)
    #[tokio::test]
    async fn leaves_out_a_file_when_its_path_has_a_hidden_segment() {
        let daemon = daemon().await;
        let predecessor = Predecessor::new();
        predecessor.artifact(
            &shared_row(),
            &[
                ("index.html", SHARED_HTML),
                (".git/config", b"[remote]"),
                (".env", b"TOKEN=secret"),
                ("img/.hidden.png", LOGO_PNG),
                ("style.css", STYLE_CSS),
            ],
        );
        let summary = daemon.import(&predecessor).await;
        assert_eq!(
            (summary.imported, daemon.file_names(SHARED_ID).await),
            (1, ["index.html", "style.css"].map(str::to_owned).to_vec())
        );
    }

    // TC: IMPA-02 (the owner by `users.email`)
    #[tokio::test]
    async fn owns_by_the_user_with_the_predecessor_owners_email() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        assert_eq!(
            daemon.artifact(PRIVATE_ID).await.owner_user_id,
            daemon.alice.id
        );
    }

    // TC: IMPA-02 (no such user → the admin)
    #[tokio::test]
    async fn owns_by_the_admin_when_no_user_has_the_email() {
        let daemon = daemon().await;
        let predecessor = Predecessor::new();
        predecessor.artifact(
            &Row {
                id: SHARED_ID,
                owner: STRANGER,
                title: "Foreign",
                shared: true,
                created_ms: 1_790_000_000_000,
                updated_ms: 1_790_000_000_000,
                expires_ms: None,
            },
            &[("index.html", b"<p>foreign</p>")],
        );
        daemon.import(&predecessor).await;
        assert_eq!(
            daemon.artifact(SHARED_ID).await.owner_user_id,
            daemon.admin.id
        );
    }

    // TC: IMPA-02 (an owner without an e-mail → the admin)
    #[tokio::test]
    async fn owns_by_the_admin_when_the_predecessor_owner_has_no_email() {
        let daemon = daemon().await;
        let predecessor = Predecessor::new();
        predecessor.artifact(
            &Row {
                id: SHARED_ID,
                owner: NAMELESS,
                title: "Anonymous",
                shared: false,
                created_ms: 1_790_000_000_000,
                updated_ms: 1_790_000_000_000,
                expires_ms: Some(4_102_444_800_000),
            },
            &[("index.html", b"<p>anonymous</p>")],
        );
        daemon.import(&predecessor).await;
        assert_eq!(
            daemon.artifact(SHARED_ID).await.owner_user_id,
            daemon.admin.id
        );
    }

    // TC: IMPA-02 (the summary counts the owners defaulted to the admin)
    #[tokio::test]
    async fn counts_the_owners_defaulted_when_emails_match_nobody() {
        let daemon = daemon().await;
        let predecessor = Predecessor::with_three();
        predecessor.artifact(
            &Row {
                id: "Foreign00001",
                owner: STRANGER,
                title: "Foreign",
                shared: true,
                created_ms: 1_790_000_000_000,
                updated_ms: 1_790_000_000_000,
                expires_ms: None,
            },
            &[("index.html", b"<p>foreign</p>")],
        );
        predecessor.artifact(
            &Row {
                id: "Nameless0001",
                owner: NAMELESS,
                title: "Anonymous",
                shared: true,
                created_ms: 1_790_000_000_000,
                updated_ms: 1_790_000_000_000,
                expires_ms: None,
            },
            &[("index.html", b"<p>anonymous</p>")],
        );
        assert_eq!(
            daemon.import(&predecessor).await,
            ArtifactsSummary {
                imported: 5,
                skipped: 0,
                owners_defaulted: 2,
            }
        );
    }

    // TC: IMPA-03 (`index.html` through the blobs)
    #[tokio::test]
    async fn reads_index_html_through_blobs_when_imported() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        assert_eq!(daemon.file(SHARED_ID, "index.html").await, SHARED_HTML);
    }

    // TC: IMPA-03 (every file by the same relative path, nested and
    // binary ones included)
    #[tokio::test]
    async fn reads_every_file_by_its_relative_path_when_imported() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        assert_eq!(
            (
                daemon.file(SHARED_ID, "style.css").await,
                daemon.file(SHARED_ID, "img/logo.png").await,
                daemon.file(SHARED_ID, "nested/deep/data.json").await,
            ),
            (STYLE_CSS.to_vec(), LOGO_PNG.to_vec(), DATA_JSON.to_vec())
        );
    }

    // TC: IMPA-03 (the version's files: `index.html` first, then by
    // path, with the hash and size of their bytes)
    #[tokio::test]
    async fn lists_the_files_of_the_version_when_imported() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        let version = daemon
            .store
            .artifact_version(ArtifactId::parse(SHARED_ID).unwrap(), VersionNumber::FIRST)
            .await
            .unwrap()
            .unwrap();
        let files: Vec<(String, BlobHash, u64)> = version
            .files
            .into_iter()
            .map(|file| (file.name.as_str().to_owned(), file.hash, file.size))
            .collect();
        assert_eq!(
            files,
            [
                ("index.html".to_owned(), BlobHash::of(SHARED_HTML), 72),
                ("img/logo.png".to_owned(), BlobHash::of(LOGO_PNG), 11),
                (
                    "nested/deep/data.json".to_owned(),
                    BlobHash::of(DATA_JSON),
                    16
                ),
                ("style.css".to_owned(), BlobHash::of(STYLE_CSS), 19),
            ]
        );
    }

    // TC: IMPA-03 (the version's size is the sum of its files)
    #[tokio::test]
    async fn sums_the_files_as_the_size_of_the_version_when_imported() {
        let daemon = daemon().await;
        daemon.import(&Predecessor::with_three()).await;
        let version = daemon
            .store
            .artifact_version(ArtifactId::parse(SHARED_ID).unwrap(), VersionNumber::FIRST)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(version.size_bytes, 118);
    }

    // TC: IMPA-04
    #[tokio::test]
    async fn changes_nothing_when_run_twice() {
        let daemon = daemon().await;
        let predecessor = Predecessor::with_three();
        daemon.import(&predecessor).await;
        let artifacts = daemon.store.artifacts(None).await.unwrap();
        let mut versions = Vec::new();
        for artifact in &artifacts {
            versions.push(daemon.store.artifact_versions(artifact.id).await.unwrap());
        }
        daemon.import(&predecessor).await;
        let again = daemon.store.artifacts(None).await.unwrap();
        let mut versions_again = Vec::new();
        for artifact in &again {
            versions_again.push(daemon.store.artifact_versions(artifact.id).await.unwrap());
        }
        assert_eq!((again, versions_again), (artifacts, versions));
    }

    // TC: IMPA-04 (an id already stored is skipped and counted)
    #[tokio::test]
    async fn counts_every_artifact_as_skipped_when_run_twice() {
        let daemon = daemon().await;
        let predecessor = Predecessor::with_three();
        daemon.import(&predecessor).await;
        assert_eq!(
            daemon.import(&predecessor).await,
            ArtifactsSummary {
                imported: 0,
                skipped: 3,
                owners_defaulted: 0,
            }
        );
    }

    // TC: IMPA-04 (idempotent by id: a changed predecessor row does not
    // overwrite the stored artifact)
    #[tokio::test]
    async fn keeps_the_stored_artifact_when_the_predecessor_row_changed() {
        let daemon = daemon().await;
        let predecessor = Predecessor::with_three();
        daemon.import(&predecessor).await;
        predecessor
            .db()
            .execute(
                "UPDATE artifacts SET title = 'Renamed', shared = 0, expires_at = ? WHERE id = ?",
                params![4_102_444_800_000_i64, SHARED_ID],
            )
            .unwrap();
        daemon.import(&predecessor).await;
        let artifact = daemon.artifact(SHARED_ID).await;
        assert_eq!(
            (artifact.title, artifact.visibility, artifact.expires_at),
            ("Report".to_owned(), Visibility::Shared, None)
        );
    }

    // TC: IMPA-04 (a first run's summary)
    #[tokio::test]
    async fn counts_every_artifact_as_imported_when_run_once() {
        let daemon = daemon().await;
        assert_eq!(
            daemon.import(&Predecessor::with_three()).await,
            ArtifactsSummary {
                imported: 3,
                skipped: 0,
                owners_defaulted: 0,
            }
        );
    }

    // TC: IMPA-01 (the input of the import; nothing written)
    #[tokio::test]
    async fn fails_with_missing_database_when_the_database_is_absent() {
        let daemon = daemon().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("artifacts")).unwrap();
        let result = import_artifacts(&daemon.store, &daemon.blobs, dir.path()).await;
        assert!(
            matches!(
                &result,
                Err(ImportArtifactsError::MissingDatabase { path })
                    if *path == dir.path().join("artifacts.db")
            ),
            "{result:?}"
        );
    }

    // TC: IMPA-01 (the input of the import)
    #[tokio::test]
    async fn fails_with_missing_directory_when_the_directory_is_absent() {
        let daemon = daemon().await;
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("absent");
        let result = import_artifacts(&daemon.store, &daemon.blobs, &dir).await;
        assert!(
            matches!(
                &result,
                Err(ImportArtifactsError::MissingDirectory { path }) if *path == dir
            ),
            "{result:?}"
        );
    }
}

mod artifacts_summary {
    use super::*;

    // TC: IMPA-04 (the summary printed on stderr)
    #[test]
    fn names_every_count_when_displayed() {
        let summary = ArtifactsSummary {
            imported: 2,
            skipped: 1,
            owners_defaulted: 1,
        };
        assert_eq!(
            summary.to_string(),
            "imported 2 artifacts, skipped 1, owners defaulted to the admin 1"
        );
    }
}

/// `liftoffd import-artifacts <dir>` through the binary's entry point.
mod run_cli {
    use super::*;

    /// A configuration whose store and data directory are under `data`,
    /// with the users of [`daemon`] already in its database.
    struct Installed {
        data: tempfile::TempDir,
        config: PathBuf,
    }

    async fn installed() -> Installed {
        let data = tempfile::tempdir().unwrap();
        let key = data.path().join("master.key");
        std::fs::write(&key, "00".repeat(32)).unwrap();
        let config = data.path().join("liftoffd.toml");
        std::fs::write(
            &config,
            format!(
                "[api]\npublic_url = \"https://liftoff.test\"\n\
                 [domains]\nbase = \"apps.test\"\npublic_ip = \"203.0.113.10\"\n\
                 [proxy]\ntls = \"edge\"\nedge_port = 7002\nadmin = \"127.0.0.1:9\"\n\
                 [store]\npath = \"{db}\"\n\
                 [paths]\ndata = \"{data}\"\nmaster_key = \"{key}\"\n",
                db = data.path().join("liftoff.db").display(),
                data = data.path().display(),
                key = key.display(),
            ),
        )
        .unwrap();
        let world = World::new().await;
        let store = SqliteStore::open(data.path().join("liftoff.db")).unwrap();
        store.insert_user(&world.owner).await.unwrap();
        store
            .insert_user(&world.records.user("alice", UserRole::User).unwrap())
            .await
            .unwrap();
        Installed { data, config }
    }

    async fn run(installed: &Installed, dir: &Path) -> u8 {
        liftoffd::run_cli([
            "liftoffd",
            "--config",
            installed.config.to_str().unwrap(),
            "import-artifacts",
            dir.to_str().unwrap(),
        ])
        .await
    }

    // TC: IMPA-01, IMPA-03 (the subcommand imports into the configured
    // store and `paths.data/artifacts`, exit 0)
    #[tokio::test]
    async fn exits_0_with_the_artifacts_imported_when_given_the_directory() {
        let installed = installed().await;
        let predecessor = Predecessor::with_three();
        let code = run(&installed, predecessor.path()).await;
        let store = SqliteStore::open(installed.data.path().join("liftoff.db")).unwrap();
        let blobs = FileBlobs::open(
            &store,
            installed.data.path().join("artifacts"),
            LinkMode::Detect,
        )
        .await
        .unwrap();
        let shared = store
            .artifact(ArtifactId::parse(SHARED_ID).unwrap())
            .await
            .unwrap()
            .map(|artifact| artifact.visibility);
        let css = blobs
            .read(
                ArtifactId::parse(SHARED_ID).unwrap(),
                VersionNumber::FIRST,
                &ArtifactPath::new("style.css").unwrap(),
            )
            .await
            .ok();
        assert_eq!(
            (code, shared, css),
            (0, Some(Visibility::Shared), Some(STYLE_CSS.to_vec()))
        );
    }

    // TC: IMPA-04 (a second run of the subcommand exits 0 too)
    #[tokio::test]
    async fn exits_0_when_run_twice() {
        let installed = installed().await;
        let predecessor = Predecessor::with_three();
        let first = run(&installed, predecessor.path()).await;
        let second = run(&installed, predecessor.path()).await;
        let store = SqliteStore::open(installed.data.path().join("liftoff.db")).unwrap();
        let count = store.artifacts(None).await.unwrap().len();
        assert_eq!((first, second, count), (0, 0, 3));
    }

    // TC: IMPA-01 (a missing database is a clear error, exit 1)
    #[tokio::test]
    async fn exits_1_when_the_database_is_missing() {
        let installed = installed().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("artifacts")).unwrap();
        assert_eq!(run(&installed, dir.path()).await, 1);
    }

    // TC: IMPA-01 (a missing directory is a clear error, exit 1)
    #[tokio::test]
    async fn exits_1_when_the_directory_is_missing() {
        let installed = installed().await;
        let parent = tempfile::tempdir().unwrap();
        assert_eq!(run(&installed, &parent.path().join("absent")).await, 1);
    }
}
