#![cfg(test)]
//! ART-04, ART-08 over the SQLite store: the artifacts service writes
//! its rows into SQLite and its files into the in-memory `FakeBlobs`
//! (the blobs on disk are).

use std::sync::Arc;

use liftoff_core::domain::{ArtifactId, ArtifactPath, User, UserRole, VersionNumber};
use liftoff_core::fakes::{FakePorts, Records, WithStore, World};
use liftoff_core::ports::Store as _;
use liftoff_core::service::{ArtifactContent, ArtifactsService};
use liftoff_store::SqliteStore;
use time::Duration;

/// The service over a fresh SQLite store holding the admin `owner`.
async fn service() -> (
    Arc<WithStore<SqliteStore>>,
    ArtifactsService<WithStore<SqliteStore>>,
    User,
) {
    let fakes = Arc::new(FakePorts::new());
    let records = Records::new(fakes.rng.clone(), fakes.clock.clone());
    let store = SqliteStore::open_in_memory().unwrap();
    let owner = records.user("owner", UserRole::Admin).unwrap();
    store.insert_user(&owner).await.unwrap();
    let ports = Arc::new(WithStore::new(fakes, store));
    let settings = World::new().await.settings;
    (
        Arc::clone(&ports),
        ArtifactsService::new(ports, settings),
        owner,
    )
}

/// The page `html`.
fn page(html: &str) -> ArtifactContent {
    ArtifactContent {
        html: html.as_bytes().to_vec(),
        files: Vec::new(),
        title: Some("Report".to_owned()),
    }
}

/// The numbers of the versions of `id` SQLite holds.
async fn numbers(ports: &WithStore<SqliteStore>, id: ArtifactId) -> Vec<u32> {
    ports
        .store
        .artifact_versions(id)
        .await
        .unwrap()
        .iter()
        .map(|version| version.number.get())
        .collect()
}

mod update {
    use super::*;

    // TC: ART-04
    #[tokio::test]
    async fn keeps_both_versions_in_sqlite_when_updated() {
        let (ports, artifacts, owner) = service().await;
        let id = artifacts
            .publish(&owner, page("<h1>v1</h1>"))
            .await
            .unwrap()
            .artifact
            .id;
        artifacts
            .update(&owner, id, page("<h1>v2</h1>"))
            .await
            .unwrap();
        let current = ports
            .store
            .artifact(id)
            .await
            .unwrap()
            .map(|artifact| artifact.current_version.get());
        let first = artifacts
            .content(id, VersionNumber::FIRST, &ArtifactPath::index())
            .await
            .ok();
        assert_eq!(
            (numbers(&ports, id).await, current, first),
            (vec![1, 2], Some(2), Some(b"<h1>v1</h1>".to_vec()))
        );
    }
}

mod sweep {
    use super::*;

    // TC: ART-08
    #[tokio::test]
    async fn deletes_expired_artifact_from_sqlite_when_swept() {
        let (ports, artifacts, owner) = service().await;
        let id = artifacts
            .publish(&owner, page("<h1>v1</h1>"))
            .await
            .unwrap()
            .artifact
            .id;
        ports.fakes.clock.advance(Duration::hours(5));
        let swept = artifacts.sweep().await.ok();
        assert_eq!(
            (
                swept,
                ports.store.artifact(id).await.unwrap(),
                numbers(&ports, id).await
            ),
            (Some(vec![id]), None, Vec::new())
        );
    }
}
