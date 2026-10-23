//! ART-01…ART-14: artifacts, their versions and files, over the shared [`World`] with the fake clock, the fake
//! `Rng` and the in-memory [`FakeBlobs`](crate::fakes::FakeBlobs).

use std::sync::Arc;

use time::{Duration, OffsetDateTime};

use super::*;
use crate::domain::{
    ArtifactFile, Bearer, BlobHash, GateSecret, Host, LinkSecret, LinkTarget, SessionSecret,
    Timestamp, UserRole, Visibility,
};
use crate::fakes::{BlobCall, Call, FakePorts, FixedClock, HookCall, World};
use crate::ports::{HookRequest, HookResult};
use crate::service::{Access, AccessDecision, AccessRequest, ArtifactSettings};

/// SHA-256 of `<h1>v1</h1>`.
const V1_HASH: &str = "c5484ff4f6706fb9d2418ffdc2b0ba515106dc32c68fbffb365b303bd9a6427e";
/// SHA-256 of `<h1>v2</h1>`.
const V2_HASH: &str = "3a6666dacddd1fc67d6d96e5f3315a628eaeeb9b325fd148e4510d199118ac1c";
/// SHA-256 of `body{}`.
const CSS_HASH: &str = "7c98040a541657584690ae2a1cc3b42a8b53b159cc60c5d3abbfecbaeac6c94a";
/// SHA-256 of `PNG`.
const PNG_HASH: &str = "796120837694d3f3f29259cfeb25091698c2a0aa87873658d840b4993ee889b3";
/// SHA-256 of `<p>1</p>`.
const P1_HASH: &str = "16de9dfa12664d7331c84ac261051ef5ebe566d03dae3faf5e45825fb04bcdbc";
/// SHA-256 of `same`.
const SAME_HASH: &str = "0967115f2813a3541eaef77de9d9d5773f1c0c04314b0bbfe4ff3b3b1c55b5d5";

/// The page `html` titled `Report`, without files.
fn page(html: &str) -> ArtifactContent {
    with_files(html, &[])
}

/// The page `html` titled `Report` with `files` (name, text).
fn with_files(html: &str, files: &[(&str, &str)]) -> ArtifactContent {
    ArtifactContent {
        html: html.as_bytes().to_vec(),
        files: files
            .iter()
            .map(|(name, text)| ArtifactUpload {
                name: path(name),
                bytes: text.as_bytes().to_vec(),
            })
            .collect(),
        title: Some("Report".to_owned()),
    }
}

/// `raw` as an [`ArtifactPath`].
fn path(raw: &str) -> ArtifactPath {
    ArtifactPath::new(raw).unwrap()
}

/// `raw` as a [`BlobHash`].
fn hash(raw: &str) -> BlobHash {
    BlobHash::parse(raw).unwrap()
}

/// Version `n`.
fn v(n: u32) -> VersionNumber {
    VersionNumber::new(n).unwrap()
}

/// The fake clock's start plus `hours`.
fn at_hours(hours: i64) -> Timestamp {
    OffsetDateTime::from_unix_timestamp(FixedClock::START_UNIX)
        .unwrap()
        .saturating_add(Duration::hours(hours))
}

/// The world's service with its artifact settings changed by `change`.
fn service_with(
    world: &World,
    change: impl FnOnce(&mut ArtifactSettings),
) -> ArtifactsService<FakePorts> {
    let mut settings = (*world.settings).clone();
    change(&mut settings.artifacts);
    ArtifactsService::new(Arc::clone(&world.ports), Arc::new(settings))
}

/// Publishes `html` as `owner`.
async fn published(world: &World, owner: &User, html: &str) -> PublishedArtifact {
    world.artifacts().publish(owner, page(html)).await.unwrap()
}

/// A user `login` who is not the admin.
async fn member(world: &World, login: &str) -> User {
    world.add_user(login, UserRole::User).await.unwrap()
}

/// The hook allows the next question to `authorize`.
fn allow_next(world: &World) {
    world
        .ports
        .hooks
        .answer_next("authorize", HookResult::Allow);
}

/// The numbers of the stored versions of `id`, oldest first.
async fn stored_numbers(world: &World, id: ArtifactId) -> Vec<u32> {
    world
        .ports
        .store
        .artifact_versions(id)
        .await
        .unwrap()
        .iter()
        .map(|version| version.number.get())
        .collect()
}

/// The calls that wrote a record or a blob.
fn writes(world: &World) -> Vec<Call> {
    world
        .log()
        .filter(|call| matches!(call, Call::Store(_) | Call::Blob(_)))
}

/// The artifacts a file was linked to, in the order of the calls.
fn linked(world: &World) -> Vec<ArtifactId> {
    let mut ids = Vec::new();
    for call in world.log().calls() {
        if let Call::Blob(BlobCall::Link { artifact, .. }) = call
            && !ids.contains(&artifact)
        {
            ids.push(artifact);
        }
    }
    ids
}

/// A failure of the store or the blobs, scripted by a test.
fn disk_full() -> Error {
    Error::internal_msg("disk full")
}

mod publish {
    use super::*;

    // TC: ART-01
    #[tokio::test]
    async fn version_one_private_until_ttl_when_published() {
        let world = World::new().await;
        let published = published(&world, &world.owner, "<h1>v1</h1>").await;
        assert_eq!(
            (
                published.artifact.current_version,
                published.version.number,
                published.artifact.visibility,
                published.artifact.expires_at,
                published.artifact.owner_user_id,
                published.artifact.title.as_str(),
            ),
            (
                v(1),
                v(1),
                Visibility::Private,
                Some(at_hours(5)),
                world.owner.id,
                "Report"
            )
        );
    }

    // TC: ART-01 (`https://<base>/a/<id>`)
    #[tokio::test]
    async fn url_under_api_host_when_artifacts_have_no_host() {
        let world = World::new().await;
        let published = published(&world, &world.owner, "<h1>v1</h1>").await;
        assert_eq!(
            published.url,
            format!("https://liftoff.test/a/{}", published.artifact.id)
        );
    }

    // TC: ART-01 (`https://<artifacts.host>/<id>`)
    #[tokio::test]
    async fn url_on_artifacts_host_when_set() {
        let world = World::new().await;
        let artifacts = service_with(&world, |settings| {
            settings.host = Some(Host::new("art.test").unwrap());
        });
        let published = artifacts
            .publish(&world.owner, page("<h1>v1</h1>"))
            .await
            .unwrap();
        assert_eq!(
            published.url,
            format!("https://art.test/{}", published.artifact.id)
        );
    }

    // TC: ART-01 (12 base62 characters from the `Rng`, a new one each
    // time)
    #[tokio::test]
    async fn new_twelve_character_base62_id_when_published_twice() {
        let world = World::new().await;
        let first = published(&world, &world.owner, "<h1>v1</h1>").await;
        let second = published(&world, &world.owner, "<h1>v1</h1>").await;
        let (a, b) = (first.artifact.id, second.artifact.id);
        let shaped = |id: ArtifactId| {
            id.as_str().len() == 12 && id.as_str().bytes().all(|b| b.is_ascii_alphanumeric())
        };
        assert_eq!(
            (shaped(a), shaped(b), a == b),
            (true, true, false),
            "{a} {b}"
        );
    }

    // TC: ART-01 (the login link of a private artifact: a one-time link
    // of its owner to `artifact:<id>`, ten minutes)
    #[tokio::test]
    async fn login_link_of_owner_to_artifact_when_private() {
        let world = World::new().await;
        let published = published(&world, &world.owner, "<h1>v1</h1>").await;
        let link = published
            .login_url
            .expect("a private artifact has a login link");
        let token = link
            .url
            .strip_prefix("https://liftoff.test/api/v1/auth/links/")
            .expect("a link on the API host");
        let stored = world
            .ports
            .store
            .one_time_link(&LinkSecret::parse(token).unwrap().hash())
            .await
            .unwrap()
            .map(|link| (link.user_id, link.target, link.expires_at, link.used_at));
        assert_eq!(
            stored,
            Some((
                world.owner.id,
                LinkTarget::Artifact(published.artifact.id),
                at_hours(0).saturating_add(Duration::minutes(10)),
                None
            ))
        );
    }

    // TC: ART-01 (the artifact and its version 1 are stored)
    #[tokio::test]
    async fn stores_artifact_and_version_when_published() {
        let world = World::new().await;
        let published = published(&world, &world.owner, "<h1>v1</h1>").await;
        let store = &world.ports.store;
        assert_eq!(
            (
                store.artifact(published.artifact.id).await.unwrap(),
                store
                    .artifact_versions(published.artifact.id)
                    .await
                    .unwrap()
            ),
            (Some(published.artifact.clone()), vec![published.version])
        );
    }

    // TC: ART-01, ART-13 (the version names `index.html` and its files by
    // hash and size)
    #[tokio::test]
    async fn lists_index_and_files_with_hashes_when_published() {
        let world = World::new().await;
        let published = world
            .artifacts()
            .publish(
                &world.owner,
                with_files("<h1>v1</h1>", &[("css/site.css", "body{}")]),
            )
            .await
            .unwrap();
        assert_eq!(
            (published.version.files, published.version.size_bytes),
            (
                vec![
                    ArtifactFile {
                        name: ArtifactPath::index(),
                        hash: hash(V1_HASH),
                        size: 11,
                    },
                    ArtifactFile {
                        name: path("css/site.css"),
                        hash: hash(CSS_HASH),
                        size: 6,
                    },
                ],
                17
            )
        );
    }

    // TC: ART-01 (the HTML and the files are the version's blobs)
    #[tokio::test]
    async fn serves_html_and_files_of_version_when_published() {
        let world = World::new().await;
        let id = world
            .artifacts()
            .publish(
                &world.owner,
                with_files("<h1>v1</h1>", &[("css/site.css", "body{}")]),
            )
            .await
            .unwrap()
            .artifact
            .id;
        let artifacts = world.artifacts();
        assert_eq!(
            (
                artifacts
                    .content(id, v(1), &ArtifactPath::index())
                    .await
                    .ok(),
                artifacts
                    .content(id, v(1), &path("css/site.css"))
                    .await
                    .ok(),
            ),
            (Some(b"<h1>v1</h1>".to_vec()), Some(b"body{}".to_vec()))
        );
    }

    // TC: ART-01 (one question to `authorize`, about the installation)
    #[tokio::test]
    async fn asks_authorize_once_about_installation_when_published() {
        let world = World::new().await;
        published(&world, &world.owner, "<h1>v1</h1>").await;
        assert_eq!(
            world.log().authorized(),
            vec![(world.owner.id, Action::ArtifactPublish, Resource::Admin)]
        );
    }

    // TC: ART-02 (two files of one name: the error names it, nothing is
    // written)
    #[tokio::test]
    async fn validation_naming_file_when_two_files_share_a_name() {
        let world = World::new().await;
        let result = world
            .artifacts()
            .publish(
                &world.owner,
                with_files("<h1>v1</h1>", &[("a.css", "x"), ("a.css", "y")]),
            )
            .await;
        assert_eq!(
            (
                match &result {
                    Err(Error::Validation(error)) => Some((error.field, error.path.clone())),
                    _ => None,
                },
                writes(&world)
            ),
            (Some(("files", Some("a.css".to_owned()))), Vec::new()),
            "{result:?}"
        );
    }
    // TC: ART-01 (a version whose files cannot be linked is taken back:
    // no artifact, no links)
    #[tokio::test]
    async fn nothing_left_when_link_fails() {
        let world = World::new().await;
        world.ports.blobs.fail_next("link", disk_full());
        let result = world
            .artifacts()
            .publish(&world.owner, page("<h1>v1</h1>"))
            .await;
        let ids = linked(&world);
        assert_eq!(
            (
                matches!(result, Err(Error::Internal(_))),
                world.ports.store.artifacts(None).await.unwrap(),
                ids.len(),
                ids.iter()
                    .flat_map(|id| world.ports.blobs.versions(*id))
                    .count(),
            ),
            (true, Vec::new(), 1, 0),
            "{result:?}"
        );
    }

    // TC: ART-01, BLOB-03 (the blobs of a write whose links failed are
    // released and go: none is left until a restart)
    #[tokio::test]
    async fn no_blob_left_when_link_fails() {
        let world = World::new().await;
        world.ports.blobs.fail_next("link", disk_full());
        let result = world
            .artifacts()
            .publish(&world.owner, page("<h1>v1</h1>"))
            .await;
        assert_eq!(
            (
                matches!(result, Err(Error::Internal(_))),
                world.ports.blobs.blobs()
            ),
            (true, Vec::new()),
            "{result:?}"
        );
    }

    // TC: ART-01 (the login link is part of the publish: without it the
    // artifact is taken back)
    #[tokio::test]
    async fn nothing_left_when_login_link_cannot_be_issued() {
        let world = World::new().await;
        world
            .ports
            .store
            .fail_next("insert_one_time_link", disk_full());
        let result = world
            .artifacts()
            .publish(&world.owner, page("<h1>v1</h1>"))
            .await;
        let ids = linked(&world);
        assert_eq!(
            (
                matches!(result, Err(Error::Internal(_))),
                world.ports.store.artifacts(None).await.unwrap(),
                ids.len(),
                ids.iter()
                    .flat_map(|id| world.ports.blobs.versions(*id))
                    .count(),
            ),
            (true, Vec::new(), 1, 0),
            "{result:?}"
        );
    }

    // TC: ART-01 (the rows come first: a publish the store refuses
    // writes no blob)
    #[tokio::test]
    async fn puts_no_blob_when_store_refuses_artifact() {
        let world = World::new().await;
        world.ports.store.fail_next("insert_artifact", disk_full());
        let result = world
            .artifacts()
            .publish(&world.owner, page("<h1>v1</h1>"))
            .await;
        assert_eq!(
            (
                matches!(result, Err(Error::Internal(_))),
                world.ports.blobs.puts(),
                world.ports.blobs.blobs(),
            ),
            (true, Vec::new(), Vec::new()),
            "{result:?}"
        );
    }

    // TC: ART-01 (an id somebody holds is drawn again from the `Rng`)
    #[tokio::test]
    async fn published_under_new_id_when_first_id_clashes() {
        let clean = World::new().await;
        let first_drawn = published(&clean, &clean.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let world = World::new().await;
        world
            .ports
            .store
            .fail_next("insert_artifact", Error::conflict("artifact exists"));
        let published = published(&world, &world.owner, "<h1>v1</h1>").await;
        let stored = world.ports.store.artifact(published.artifact.id).await;
        assert_eq!(
            (
                published.artifact.id == first_drawn,
                stored.unwrap().map(|artifact| artifact.id)
            ),
            (false, Some(published.artifact.id))
        );
    }

    // TC: ART-01 (three clashes in a row: the conflict is reported)
    #[tokio::test]
    async fn conflict_when_every_id_clashes() {
        let world = World::new().await;
        for _ in 0..3 {
            world
                .ports
                .store
                .fail_next("insert_artifact", Error::conflict("artifact exists"));
        }
        let result = world
            .artifacts()
            .publish(&world.owner, page("<h1>v1</h1>"))
            .await;
        assert!(matches!(result, Err(Error::Conflict(_))), "{result:?}");
    }

    // TC: ART-01, ART-07 (`[artifacts] ttl` sets the TTL)
    #[tokio::test]
    async fn ttl_from_settings_when_ttl_changed() {
        let world = World::new().await;
        let artifacts = service_with(&world, |settings| {
            settings.ttl = core::time::Duration::from_hours(1);
        });
        let published = artifacts
            .publish(&world.owner, page("<h1>v1</h1>"))
            .await
            .unwrap();
        assert_eq!(published.artifact.expires_at, Some(at_hours(1)));
    }
}

mod size {
    use super::*;

    /// The service with a limit of 1024 bytes per version.
    fn limited(world: &World) -> ArtifactsService<FakePorts> {
        service_with(world, |settings| settings.max_size_bytes = 1024)
    }

    /// 600 bytes of HTML and a file of `file` bytes.
    fn sized(file: usize) -> ArtifactContent {
        ArtifactContent {
            html: vec![b'h'; 600],
            files: vec![ArtifactUpload {
                name: path("data.bin"),
                bytes: vec![b'f'; file],
            }],
            title: None,
        }
    }

    // TC: ART-03
    #[tokio::test]
    async fn validation_and_nothing_written_when_version_above_limit() {
        let world = World::new().await;
        let result = limited(&world).publish(&world.owner, sized(425)).await;
        assert_eq!(
            (
                matches!(result, Err(Error::Validation(_))),
                writes(&world),
                world.ports.blobs.blobs()
            ),
            (true, Vec::new(), Vec::new()),
            "{result:?}"
        );
    }

    // TC: ART-03 (the limit itself passes: HTML and files together)
    #[tokio::test]
    async fn published_when_version_exactly_at_limit() {
        let world = World::new().await;
        let result = limited(&world).publish(&world.owner, sized(424)).await;
        assert_eq!(
            result.map(|published| published.version.size_bytes).ok(),
            Some(1024)
        );
    }

    // TC: ART-03 (an update above the limit leaves the artifact as it
    // was)
    #[tokio::test]
    async fn validation_and_version_kept_when_update_above_limit() {
        let world = World::new().await;
        let artifacts = limited(&world);
        let id = artifacts
            .publish(&world.owner, sized(10))
            .await
            .unwrap()
            .artifact
            .id;
        world.log().clear();
        let result = artifacts.update(&world.owner, id, sized(425)).await;
        assert_eq!(
            (
                matches!(result, Err(Error::Validation(_))),
                writes(&world),
                stored_numbers(&world, id).await
            ),
            (true, Vec::new(), vec![1]),
            "{result:?}"
        );
    }
}

mod update {
    use super::*;

    // TC: ART-04
    #[tokio::test]
    async fn version_two_current_when_updated() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let updated = world
            .artifacts()
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await
            .unwrap();
        let stored = world.ports.store.artifact(id).await.unwrap();
        assert_eq!(
            (
                updated.version.number,
                updated.artifact.current_version,
                stored.map(|artifact| artifact.current_version),
                updated.url,
            ),
            (
                v(2),
                v(2),
                Some(v(2)),
                format!("https://liftoff.test/a/{id}")
            )
        );
    }

    // TC: ART-04 (`/a/{id}/v/1` still serves version 1)
    #[tokio::test]
    async fn version_one_readable_when_updated() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let artifacts = world.artifacts();
        artifacts
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await
            .unwrap();
        let index = ArtifactPath::index();
        assert_eq!(
            (
                artifacts.content(id, v(1), &index).await.ok(),
                artifacts.content(id, v(2), &index).await.ok(),
            ),
            (Some(b"<h1>v1</h1>".to_vec()), Some(b"<h1>v2</h1>".to_vec()))
        );
    }

    // TC: ART-04 (`versions` lists both, oldest first, with their titles)
    #[tokio::test]
    async fn lists_both_versions_oldest_first_when_updated() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let artifacts = world.artifacts();
        let mut second = page("<h1>v2</h1>");
        second.title = Some("Report, revised".to_owned());
        artifacts.update(&world.owner, id, second).await.unwrap();
        let listed: Vec<(u32, String)> = artifacts
            .versions(&world.owner, id)
            .await
            .unwrap()
            .into_iter()
            .map(|version| (version.number.get(), version.title))
            .collect();
        assert_eq!(
            listed,
            vec![(1, "Report".to_owned()), (2, "Report, revised".to_owned())]
        );
    }

    // TC: ART-07 (the TTL runs from the latest version)
    #[tokio::test]
    async fn ttl_runs_from_latest_version_when_updated() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world.ports.clock.advance(Duration::hours(3));
        let updated = world
            .artifacts()
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await
            .unwrap();
        assert_eq!(updated.artifact.expires_at, Some(at_hours(8)));
    }

    // TC: ART-07, ART-06 (a shared artifact stays shared, without a TTL)
    #[tokio::test]
    async fn stays_shared_without_ttl_when_shared_artifact_updated() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let artifacts = world.artifacts();
        artifacts.share(&world.owner, id).await.unwrap();
        let updated = artifacts
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await
            .unwrap();
        assert_eq!(
            (updated.artifact.visibility, updated.artifact.expires_at),
            (Visibility::Shared, None)
        );
    }

    // TC: ART-08 (an expired artifact is gone even before the sweeper
    // ran, as in the standalone artifacts server)
    #[tokio::test]
    async fn not_found_when_artifact_expired() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world.ports.clock.advance(Duration::hours(5));
        let result = world
            .artifacts()
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }
    // TC: ART-04 (a version whose files cannot be linked is taken back:
    // the previous one stays current, readable, with its files)
    #[tokio::test]
    async fn previous_version_current_when_link_fails() {
        let world = World::new().await;
        let published = published(&world, &world.owner, "<h1>v1</h1>").await;
        let id = published.artifact.id;
        world.ports.blobs.fail_next("link", disk_full());
        let artifacts = world.artifacts();
        let result = artifacts
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await;
        let read = artifacts.content(id, v(1), &ArtifactPath::index()).await;
        assert_eq!(
            (
                matches!(result, Err(Error::Internal(_))),
                world.ports.store.artifact(id).await.unwrap(),
                stored_numbers(&world, id).await,
                world.ports.blobs.versions(id),
                read.ok(),
            ),
            (
                true,
                Some(published.artifact),
                vec![1],
                vec![v(1)],
                Some(b"<h1>v1</h1>".to_vec())
            ),
            "{result:?}"
        );
    }

    // TC: ART-09 (a rollback whose files cannot be linked is taken back)
    #[tokio::test]
    async fn previous_version_current_when_rollback_link_fails() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let artifacts = world.artifacts();
        let second = artifacts
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await
            .unwrap();
        world.ports.blobs.fail_next("link", disk_full());
        let result = artifacts.rollback(&world.owner, id, v(1)).await;
        assert_eq!(
            (
                matches!(result, Err(Error::Internal(_))),
                world.ports.store.artifact(id).await.unwrap(),
                stored_numbers(&world, id).await,
                world.ports.blobs.versions(id),
            ),
            (true, Some(second.artifact), vec![1, 2], vec![v(1), v(2)]),
            "{result:?}"
        );
    }

    // TC: ART-04 (the rows come first: a version the store refuses, as
    // the loser of two concurrent updates, writes no blob)
    #[tokio::test]
    async fn puts_no_blob_when_store_refuses_version() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world
            .ports
            .store
            .fail_next("add_artifact_version", Error::conflict("version 2 exists"));
        let result = world
            .artifacts()
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await;
        assert_eq!(
            (
                matches!(result, Err(Error::Conflict(_))),
                world.ports.blobs.puts(),
                world.ports.blobs.blobs(),
            ),
            (true, vec![hash(V1_HASH)], vec![hash(V1_HASH)]),
            "{result:?}"
        );
    }
}

mod trimming {
    use super::*;

    /// Publishes `<p>1</p>` and updates it to `<p>n</p>` until it has
    /// `count` versions.
    async fn with_versions(world: &World, count: u32) -> ArtifactId {
        let artifacts = world.artifacts();
        let id = published(world, &world.owner, "<p>1</p>").await.artifact.id;
        for n in 2..=count {
            artifacts
                .update(&world.owner, id, page(&format!("<p>{n}</p>")))
                .await
                .unwrap();
        }
        id
    }

    // TC: ART-05 (50 versions are kept as they are)
    #[tokio::test]
    async fn keeps_every_version_when_fifty() {
        let world = World::new().await;
        let id = with_versions(&world, 50).await;
        assert_eq!(
            stored_numbers(&world, id).await,
            (1..=50).collect::<Vec<_>>()
        );
    }

    // TC: ART-05
    #[tokio::test]
    async fn drops_first_version_when_fifty_first_added() {
        let world = World::new().await;
        let id = with_versions(&world, 51).await;
        assert_eq!(
            stored_numbers(&world, id).await,
            (2..=51).collect::<Vec<_>>()
        );
    }

    // TC: ART-05 (the first version's files go with it)
    #[tokio::test]
    async fn removes_files_of_first_version_when_fifty_first_added() {
        let world = World::new().await;
        let id = with_versions(&world, 51).await;
        let read = world
            .artifacts()
            .content(id, v(1), &ArtifactPath::index())
            .await;
        assert_eq!(
            (
                matches!(read, Err(Error::NotFound(_))),
                world.ports.blobs.versions(id).first().map(|n| n.get()),
                world.ports.blobs.blob(&hash(P1_HASH)),
            ),
            (true, Some(2), None),
            "{read:?}"
        );
    }

    // TC: ART-05 (`[artifacts] versions` sets how many are kept)
    #[tokio::test]
    async fn keeps_configured_versions_when_versions_changed() {
        let world = World::new().await;
        let artifacts = service_with(&world, |settings| settings.versions = 3);
        let id = artifacts
            .publish(&world.owner, page("<p>1</p>"))
            .await
            .unwrap()
            .artifact
            .id;
        for n in 2..=4 {
            artifacts
                .update(&world.owner, id, page(&format!("<p>{n}</p>")))
                .await
                .unwrap();
        }
        assert_eq!(
            (
                stored_numbers(&world, id).await,
                world.ports.blobs.versions(id)
            ),
            (vec![2, 3, 4], vec![v(2), v(3), v(4)])
        );
    }

    // TC: ART-05 (dropping the old files fails after the new version is
    // written: the update stands, and the dropped version is not served
    // although its files stayed)
    #[tokio::test]
    async fn new_version_current_and_dropped_one_unserved_when_removing_files_fails() {
        let world = World::new().await;
        let id = with_versions(&world, 50).await;
        world.ports.blobs.fail_next("remove_version", disk_full());
        let artifacts = world.artifacts();
        let result = artifacts.update(&world.owner, id, page("<p>51</p>")).await;
        let read = artifacts.content(id, v(1), &ArtifactPath::index()).await;
        assert_eq!(
            (
                result.map(|updated| updated.artifact.current_version).ok(),
                stored_numbers(&world, id).await,
                world.ports.blobs.versions(id).first().map(|n| n.get()),
                matches!(read, Err(Error::NotFound(_))),
            ),
            (Some(v(51)), (2..=51).collect::<Vec<_>>(), Some(1), true),
            "{read:?}"
        );
    }
}

mod share {
    use super::*;

    // TC: ART-06
    #[tokio::test]
    async fn shared_without_ttl_when_shared() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let shared = world.artifacts().share(&world.owner, id).await.unwrap();
        let stored = world.ports.store.artifact(id).await.unwrap();
        assert_eq!(
            (
                (shared.visibility, shared.expires_at),
                stored.map(|artifact| (artifact.visibility, artifact.expires_at))
            ),
            ((Visibility::Shared, None), Some((Visibility::Shared, None)))
        );
    }

    // TC: ART-06 (unshare starts a new TTL from now)
    #[tokio::test]
    async fn private_with_ttl_from_now_when_unshared() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let artifacts = world.artifacts();
        world.ports.clock.advance(Duration::hours(1));
        artifacts.share(&world.owner, id).await.unwrap();
        world.ports.clock.advance(Duration::hours(2));
        let unshared = artifacts.unshare(&world.owner, id).await.unwrap();
        assert_eq!(
            (unshared.visibility, unshared.expires_at),
            (Visibility::Private, Some(at_hours(8)))
        );
    }

    // TC: ART-06, ART-08 (a shared artifact outlives its TTL)
    #[tokio::test]
    async fn still_there_after_ttl_when_shared() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let artifacts = world.artifacts();
        artifacts.share(&world.owner, id).await.unwrap();
        world.ports.clock.advance(Duration::hours(6));
        let versions = artifacts.versions(&world.owner, id).await;
        assert_eq!(versions.map(|versions| versions.len()).ok(), Some(1));
    }
}

mod rollback {
    use super::*;

    /// An artifact of `<h1>v1</h1>` with `css/site.css`, updated to
    /// `<h1>v2</h1>` without files.
    async fn two_versions(world: &World) -> ArtifactId {
        let artifacts = world.artifacts();
        let id = artifacts
            .publish(
                &world.owner,
                with_files("<h1>v1</h1>", &[("css/site.css", "body{}")]),
            )
            .await
            .unwrap()
            .artifact
            .id;
        let mut second = page("<h1>v2</h1>");
        second.title = Some("Second".to_owned());
        artifacts.update(&world.owner, id, second).await.unwrap();
        id
    }

    // TC: ART-09
    #[tokio::test]
    async fn version_three_equals_version_one_when_rolled_back_to_one() {
        let world = World::new().await;
        let id = two_versions(&world).await;
        let rolled = world
            .artifacts()
            .rollback(&world.owner, id, v(1))
            .await
            .unwrap();
        assert_eq!(
            (
                rolled.version.number,
                rolled.artifact.current_version,
                rolled.version.title.as_str(),
                rolled.version.files,
            ),
            (
                v(3),
                v(3),
                "Report",
                vec![
                    ArtifactFile {
                        name: ArtifactPath::index(),
                        hash: hash(V1_HASH),
                        size: 11,
                    },
                    ArtifactFile {
                        name: path("css/site.css"),
                        hash: hash(CSS_HASH),
                        size: 6,
                    },
                ]
            )
        );
    }

    // TC: ART-09 (version 3 serves version 1's content)
    #[tokio::test]
    async fn serves_content_of_version_one_when_rolled_back_to_one() {
        let world = World::new().await;
        let id = two_versions(&world).await;
        let artifacts = world.artifacts();
        artifacts.rollback(&world.owner, id, v(1)).await.unwrap();
        assert_eq!(
            (
                artifacts
                    .content(id, v(3), &ArtifactPath::index())
                    .await
                    .ok(),
                artifacts
                    .content(id, v(3), &path("css/site.css"))
                    .await
                    .ok(),
            ),
            (Some(b"<h1>v1</h1>".to_vec()), Some(b"body{}".to_vec()))
        );
    }

    // TC: ART-09, ART-13 (the copy links the same blobs: nothing is put)
    #[tokio::test]
    async fn puts_no_blob_when_rolled_back() {
        let world = World::new().await;
        let id = two_versions(&world).await;
        let before = world.ports.blobs.blobs();
        world.log().clear();
        world
            .artifacts()
            .rollback(&world.owner, id, v(1))
            .await
            .unwrap();
        assert_eq!(
            (world.ports.blobs.puts(), world.ports.blobs.blobs()),
            (Vec::new(), before)
        );
    }

    // TC: ART-09
    #[tokio::test]
    async fn not_found_when_version_unknown() {
        let world = World::new().await;
        let id = two_versions(&world).await;
        let result = world.artifacts().rollback(&world.owner, id, v(7)).await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }
}

mod delete_version {
    use super::*;

    // TC: ART-10
    #[tokio::test]
    async fn conflict_and_kept_when_current_version_deleted() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let result = world
            .artifacts()
            .delete_version(&world.owner, id, v(1))
            .await;
        assert_eq!(
            (
                matches!(result, Err(Error::Conflict(_))),
                stored_numbers(&world, id).await
            ),
            (true, vec![1]),
            "{result:?}"
        );
    }

    // TC: ART-10
    #[tokio::test]
    async fn removes_version_with_its_files_when_not_current() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let artifacts = world.artifacts();
        artifacts
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await
            .unwrap();
        artifacts
            .delete_version(&world.owner, id, v(1))
            .await
            .unwrap();
        let read = artifacts.content(id, v(1), &ArtifactPath::index()).await;
        assert_eq!(
            (
                stored_numbers(&world, id).await,
                matches!(read, Err(Error::NotFound(_))),
                world.ports.blobs.versions(id),
                world.ports.blobs.blobs(),
            ),
            (vec![2], true, vec![v(2)], vec![hash(V2_HASH)]),
            "{read:?}"
        );
    }

    // TC: ART-10
    #[tokio::test]
    async fn not_found_when_version_unknown() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let result = world
            .artifacts()
            .delete_version(&world.owner, id, v(4))
            .await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }

    // TC: ART-10 (files go first: a version whose files could not be
    // removed is still there, whole, and the deletion can be retried)
    #[tokio::test]
    async fn version_kept_whole_when_removing_its_files_fails() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let artifacts = world.artifacts();
        artifacts
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await
            .unwrap();
        world.ports.blobs.fail_next("remove_version", disk_full());
        let result = artifacts.delete_version(&world.owner, id, v(1)).await;
        let read = artifacts.content(id, v(1), &ArtifactPath::index()).await;
        assert_eq!(
            (
                matches!(result, Err(Error::Internal(_))),
                stored_numbers(&world, id).await,
                read.ok(),
            ),
            (true, vec![1, 2], Some(b"<h1>v1</h1>".to_vec())),
            "{result:?}"
        );
    }
}

mod authorization {
    use super::*;

    /// The admin's artifact with two versions.
    async fn admins_artifact(world: &World) -> ArtifactId {
        let id = published(world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world
            .artifacts()
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await
            .unwrap();
        id
    }

    // TC: ART-11 (one question to `authorize`, about the artifact)
    #[tokio::test]
    async fn asks_authorize_once_about_artifact_when_shared() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world.log().clear();
        world.artifacts().share(&world.owner, id).await.unwrap();
        assert_eq!(
            world.log().authorized(),
            vec![(
                world.owner.id,
                Action::ArtifactShare,
                Resource::Artifact(id)
            )]
        );
    }

    // TC: ART-17 (reading an artifact is its own question, apart from
    // reading its versions: modules and the audit tell them apart)
    #[tokio::test]
    async fn asks_authorize_once_about_artifact_when_read() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world.log().clear();
        let _artifact = world.artifacts().get(&world.owner, id).await.unwrap();
        assert_eq!(
            world.log().authorized(),
            vec![(world.owner.id, Action::ArtifactGet, Resource::Artifact(id))]
        );
    }

    // TC: ART-11 (without a module nobody but the admin passes)
    #[tokio::test]
    async fn forbidden_when_non_admin_updates_and_no_module_answers() {
        let world = World::new().await;
        let carol = member(&world, "carol").await;
        let id = admins_artifact(&world).await;
        let result = world
            .artifacts()
            .update(&carol, id, page("<h1>v3</h1>"))
            .await;
        assert!(
            matches!(&result, Err(Error::Forbidden(reason)) if reason == "carol may not artifact.update"),
            "{result:?}"
        );
    }

    // TC: ART-11 (a user the hook lets in still may not change another
    // user's artifact: every mutation is refused and nothing changes)
    #[tokio::test]
    async fn forbidden_and_unchanged_when_other_user_mutates_with_hook_allowing() {
        let world = World::new().await;
        let carol = member(&world, "carol").await;
        let id = admins_artifact(&world).await;
        let artifacts = world.artifacts();
        let mut refused = Vec::new();
        allow_next(&world);
        refused.push(artifacts.update(&carol, id, page("<h1>x</h1>")).await.err());
        allow_next(&world);
        refused.push(artifacts.share(&carol, id).await.err());
        allow_next(&world);
        refused.push(artifacts.unshare(&carol, id).await.err());
        allow_next(&world);
        refused.push(artifacts.rollback(&carol, id, v(1)).await.err());
        allow_next(&world);
        refused.push(artifacts.delete_version(&carol, id, v(1)).await.err());
        allow_next(&world);
        refused.push(artifacts.delete(&carol, id).await.err());
        let forbidden: Vec<bool> = refused
            .iter()
            .map(|error| matches!(error, Some(Error::Forbidden(_))))
            .collect();
        let stored = world.ports.store.artifact(id).await.unwrap();
        assert_eq!(
            (
                forbidden,
                stored.map(|artifact| (artifact.current_version, artifact.visibility)),
                stored_numbers(&world, id).await
            ),
            (vec![true; 6], Some((v(2), Visibility::Private)), vec![1, 2]),
            "{refused:?}"
        );
    }

    // TC: ART-11 (the versions of another user's artifact are theirs)
    #[tokio::test]
    async fn forbidden_when_other_user_lists_versions_with_hook_allowing() {
        let world = World::new().await;
        let carol = member(&world, "carol").await;
        let id = admins_artifact(&world).await;
        allow_next(&world);
        let result = world.artifacts().versions(&carol, id).await;
        assert!(matches!(result, Err(Error::Forbidden(_))), "{result:?}");
    }

    // TC: ART-11 (the owner who is not the admin changes their own
    // artifact when the hook lets them in)
    #[tokio::test]
    async fn updated_when_non_admin_owner_updates_own_with_hook_allowing() {
        let world = World::new().await;
        let alice = member(&world, "alice").await;
        allow_next(&world);
        let id = published(&world, &alice, "<h1>v1</h1>").await.artifact.id;
        allow_next(&world);
        let updated = world
            .artifacts()
            .update(&alice, id, page("<h1>v2</h1>"))
            .await;
        assert_eq!(
            updated.map(|updated| updated.artifact.current_version).ok(),
            Some(v(2))
        );
    }

    // TC: ART-11 (the admin may)
    #[tokio::test]
    async fn deleted_when_admin_deletes_other_users_artifact() {
        let world = World::new().await;
        let alice = member(&world, "alice").await;
        allow_next(&world);
        let id = published(&world, &alice, "<h1>v1</h1>").await.artifact.id;
        let result = world.artifacts().delete(&world.owner, id).await;
        assert_eq!(
            (
                result.is_ok(),
                world.ports.store.artifact(id).await.unwrap()
            ),
            (true, None),
            "{result:?}"
        );
    }
}

mod list {
    use super::*;

    /// The admin's artifacts `first` and then `second` (an hour later),
    /// and alice's `third` an hour after that.
    async fn three(world: &World) -> (User, [ArtifactId; 3]) {
        let alice = member(world, "alice").await;
        let first = published(world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world.ports.clock.advance(Duration::hours(1));
        let second = published(world, &world.owner, "<h1>v2</h1>")
            .await
            .artifact
            .id;
        world.ports.clock.advance(Duration::hours(1));
        allow_next(world);
        let third = published(world, &alice, "<h1>v1</h1>").await.artifact.id;
        (alice, [first, second, third])
    }

    /// The ids of `listed`.
    fn ids(listed: Result<Vec<Artifact>, Error>) -> Option<Vec<ArtifactId>> {
        listed
            .ok()
            .map(|listed| listed.into_iter().map(|artifact| artifact.id).collect())
    }

    // TC: ART-12 (a user sees only their own)
    #[tokio::test]
    async fn lists_only_own_when_user_lists() {
        let world = World::new().await;
        let (alice, [_first, _second, third]) = three(&world).await;
        allow_next(&world);
        let listed = world.artifacts().list(&alice, false).await;
        assert_eq!(ids(listed), Some(vec![third]));
    }

    // TC: ART-12 (the admin without `--all` sees their own, newest first)
    #[tokio::test]
    async fn lists_own_newest_first_when_admin_lists_without_all() {
        let world = World::new().await;
        let (_alice, [first, second, _third]) = three(&world).await;
        let listed = world.artifacts().list(&world.owner, false).await;
        assert_eq!(ids(listed), Some(vec![second, first]));
    }

    // TC: ART-12 (`--all`: every user's, newest first)
    #[tokio::test]
    async fn lists_everyones_newest_first_when_admin_asks_for_all() {
        let world = World::new().await;
        let (_alice, [first, second, third]) = three(&world).await;
        let listed = world.artifacts().list(&world.owner, true).await;
        assert_eq!(ids(listed), Some(vec![third, second, first]));
    }

    // TC: ART-12 (`--all` is the admin's, whatever the hook says)
    #[tokio::test]
    async fn forbidden_when_non_admin_asks_for_all() {
        let world = World::new().await;
        let (alice, _ids) = three(&world).await;
        allow_next(&world);
        let listed = world.artifacts().list(&alice, true).await;
        assert!(matches!(listed, Err(Error::Forbidden(_))), "{listed:?}");
    }

    // TC: ART-12 (one question to `authorize`, about the installation)
    #[tokio::test]
    async fn asks_authorize_once_about_installation_when_listed() {
        let world = World::new().await;
        three(&world).await;
        world.log().clear();
        world.artifacts().list(&world.owner, false).await.unwrap();
        assert_eq!(
            world.log().authorized(),
            vec![(world.owner.id, Action::ArtifactList, Resource::Admin)]
        );
    }
}

mod dedup {
    use super::*;

    // TC: ART-13 (one blob for a file both versions have)
    #[tokio::test]
    async fn stores_content_once_when_two_versions_share_a_file() {
        let world = World::new().await;
        let artifacts = world.artifacts();
        let id = artifacts
            .publish(
                &world.owner,
                with_files("<h1>v1</h1>", &[("logo.png", "PNG")]),
            )
            .await
            .unwrap()
            .artifact
            .id;
        artifacts
            .update(
                &world.owner,
                id,
                with_files("<h1>v2</h1>", &[("logo.png", "PNG")]),
            )
            .await
            .unwrap();
        let logo = |version| {
            world
                .ports
                .blobs
                .files(id, version)
                .into_iter()
                .find(|(name, _)| name.as_str() == "logo.png")
                .map(|(_, hash)| hash)
        };
        let mut expected = vec![hash(V2_HASH), hash(PNG_HASH), hash(V1_HASH)];
        expected.sort();
        assert_eq!(
            (world.ports.blobs.blobs(), logo(v(1)), logo(v(2))),
            (expected, Some(hash(PNG_HASH)), Some(hash(PNG_HASH)))
        );
    }

    // TC: ART-13 (one `put` for two files of one content)
    #[tokio::test]
    async fn puts_content_once_when_two_files_of_a_version_share_it() {
        let world = World::new().await;
        world
            .artifacts()
            .publish(
                &world.owner,
                with_files("<h1>v1</h1>", &[("a.txt", "same"), ("b/c.txt", "same")]),
            )
            .await
            .unwrap();
        let same = hash(SAME_HASH);
        let puts = world.ports.blobs.puts();
        assert_eq!(
            (puts.iter().filter(|put| **put == same).count(), puts.len()),
            (1, 2)
        );
    }
}

mod delete {
    use super::*;

    // TC: ART-14
    #[tokio::test]
    async fn removes_every_version_and_blob_when_deleted() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let artifacts = world.artifacts();
        artifacts
            .update(&world.owner, id, page("<h1>v2</h1>"))
            .await
            .unwrap();
        artifacts.delete(&world.owner, id).await.unwrap();
        assert_eq!(
            (
                world.ports.store.artifact(id).await.unwrap(),
                stored_numbers(&world, id).await,
                world.ports.blobs.versions(id),
                world.ports.blobs.blobs(),
            ),
            (None, Vec::new(), Vec::new(), Vec::new())
        );
    }

    // TC: ART-14 (a blob another artifact names stays)
    #[tokio::test]
    async fn keeps_blob_another_artifact_names_when_deleted() {
        let world = World::new().await;
        let artifacts = world.artifacts();
        let gone = artifacts
            .publish(
                &world.owner,
                with_files("<h1>v1</h1>", &[("logo.png", "PNG")]),
            )
            .await
            .unwrap()
            .artifact
            .id;
        artifacts
            .publish(
                &world.owner,
                with_files("<h1>v2</h1>", &[("img/logo.png", "PNG")]),
            )
            .await
            .unwrap();
        artifacts.delete(&world.owner, gone).await.unwrap();
        let mut expected = vec![hash(V2_HASH), hash(PNG_HASH)];
        expected.sort();
        assert_eq!(world.ports.blobs.blobs(), expected);
    }

    // TC: ART-14
    #[tokio::test]
    async fn not_found_when_artifact_unknown() {
        let world = World::new().await;
        let unknown = ArtifactId::parse("zzzzzzzzzzzz").unwrap();
        let result = world.artifacts().delete(&world.owner, unknown).await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }
}

mod sweep {
    use super::*;

    // TC: ART-08
    #[tokio::test]
    async fn deletes_expired_private_artifact_with_its_blobs_when_swept() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world.ports.clock.advance(Duration::hours(5));
        let swept = world.artifacts().sweep().await;
        assert_eq!(
            (
                swept.ok(),
                world.ports.store.artifact(id).await.unwrap(),
                world.ports.blobs.blobs()
            ),
            (Some(vec![id]), None, Vec::new())
        );
    }

    // TC: ART-08 (a shared artifact has no TTL)
    #[tokio::test]
    async fn keeps_shared_artifact_when_swept() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world.artifacts().share(&world.owner, id).await.unwrap();
        world.ports.clock.advance(Duration::hours(6));
        let swept = world.artifacts().sweep().await;
        assert_eq!(
            (
                swept.ok(),
                world.ports.store.artifact(id).await.unwrap().map(|a| a.id),
                world.ports.blobs.blobs()
            ),
            (Some(Vec::new()), Some(id), vec![hash(V1_HASH)])
        );
    }

    // TC: ART-08 (until its TTL ends a private artifact stays)
    #[tokio::test]
    async fn keeps_private_artifact_before_expiry_when_swept() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world
            .ports
            .clock
            .advance(Duration::hours(5) - Duration::seconds(1));
        let swept = world.artifacts().sweep().await;
        assert_eq!(
            (
                swept.ok(),
                world.ports.store.artifact(id).await.unwrap().map(|a| a.id)
            ),
            (Some(Vec::new()), Some(id))
        );
    }

    // TC: ART-08 (one that cannot be deleted is left for the next sweep;
    // the others go)
    #[tokio::test]
    async fn sweeps_the_rest_when_one_artifact_fails() {
        let world = World::new().await;
        let first = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let second = published(&world, &world.owner, "<h1>v2</h1>")
            .await
            .artifact
            .id;
        world.ports.clock.advance(Duration::hours(5));
        world.ports.store.fail_next("delete_artifact", disk_full());
        let swept = world.artifacts().sweep().await.unwrap();
        let left: Vec<ArtifactId> = world
            .ports
            .store
            .artifacts(None)
            .await
            .unwrap()
            .iter()
            .map(|artifact| artifact.id)
            .collect();
        let mut all = swept.clone();
        all.extend(&left);
        all.sort();
        let mut both = vec![first, second];
        both.sort();
        assert_eq!((swept.len(), left.len(), all), (1, 1, both));
    }

    // TC: ART-08 (and the next sweep deletes it)
    #[tokio::test]
    async fn sweeps_it_next_time_when_one_artifact_failed() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world.ports.clock.advance(Duration::hours(5));
        world.ports.store.fail_next("delete_artifact", disk_full());
        let artifacts = world.artifacts();
        let first = artifacts.sweep().await.unwrap();
        let second = artifacts.sweep().await.unwrap();
        assert_eq!(
            (
                first,
                second,
                world.ports.store.artifact(id).await.unwrap(),
                world.ports.blobs.blobs()
            ),
            (Vec::new(), vec![id], None, Vec::new())
        );
    }
}

mod access {
    use super::*;

    /// What a browser accepts.
    const BROWSER_ACCEPT: &str = "text/html,application/xhtml+xml";

    /// A request for `/a/<id>` on the API host with the API host's
    /// session `session` and `accept`.
    fn request(id: ArtifactId, session: Option<SessionSecret>, accept: &str) -> AccessRequest {
        AccessRequest {
            host: Host::new(World::API_HOST).unwrap(),
            cookie: None,
            session,
            bearer: None,
            accept: Some(accept.to_owned()),
            request: HookRequest {
                method: "GET".to_owned(),
                path: format!("/a/{id}"),
                headers: Vec::new(),
            },
        }
    }

    /// A browser's request with `user`'s `browser` session, if any.
    async fn from_browser(world: &World, id: ArtifactId, user: Option<&User>) -> AccessRequest {
        let session = match user {
            Some(user) => Some(world.browser_login(user).await.unwrap()),
            None => None,
        };
        request(id, session, BROWSER_ACCEPT)
    }

    /// Alice's private artifact (the hook lets her publish).
    async fn alices(world: &World) -> (User, ArtifactId) {
        let alice = member(world, "alice").await;
        allow_next(world);
        let id = published(world, &alice, "<h1>v1</h1>").await.artifact.id;
        (alice, id)
    }

    /// The `gate` questions so far.
    fn gate_calls(world: &World) -> Vec<Call> {
        world
            .log()
            .filter(|call| matches!(call, Call::Hook(HookCall::Gate { .. })))
    }

    // TC: VIEW-01 (the core's decision: a shared artifact opens without a
    // session)
    #[tokio::test]
    async fn allows_without_session_when_artifact_shared() {
        let world = World::new().await;
        let (alice, id) = alices(&world).await;
        allow_next(&world);
        world.artifacts().share(&alice, id).await.unwrap();
        let decision = world
            .access()
            .check(
                &from_browser(&world, id, None).await,
                &LinkTarget::Artifact(id),
            )
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Allow { user: None }));
    }

    // TC: VIEW-01 (a shared artifact opened with a live session knows
    // its viewer: the wrapper shows the owner's buttons)
    #[tokio::test]
    async fn allows_with_viewer_when_artifact_shared_and_session_present() {
        let world = World::new().await;
        let (alice, id) = alices(&world).await;
        allow_next(&world);
        world.artifacts().share(&alice, id).await.unwrap();
        let decision = world
            .access()
            .check(
                &from_browser(&world, id, Some(&alice)).await,
                &LinkTarget::Artifact(id),
            )
            .await;
        assert_eq!(
            decision.ok(),
            Some(AccessDecision::Allow { user: Some(alice) })
        );
    }

    // TC: VIEW-02 (a browser without a session goes to the login)
    #[tokio::test]
    async fn login_when_private_and_browser_has_no_session() {
        let world = World::new().await;
        let (_alice, id) = alices(&world).await;
        let decision = world
            .access()
            .check(
                &from_browser(&world, id, None).await,
                &LinkTarget::Artifact(id),
            )
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Login));
    }

    // TC: VIEW-02 (a JSON client without a session gets 401)
    #[tokio::test]
    async fn unauthenticated_when_private_and_json_client_has_no_session() {
        let world = World::new().await;
        let (_alice, id) = alices(&world).await;
        let decision = world
            .access()
            .check(
                &request(id, None, "application/json"),
                &LinkTarget::Artifact(id),
            )
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Unauthenticated));
    }

    // TC: VIEW-02 (the owner's `browser` session opens it)
    #[tokio::test]
    async fn allows_owner_when_request_carries_owners_browser_session() {
        let world = World::new().await;
        let (alice, id) = alices(&world).await;
        let decision = world
            .access()
            .check(
                &from_browser(&world, id, Some(&alice)).await,
                &LinkTarget::Artifact(id),
            )
            .await;
        assert_eq!(
            decision.ok(),
            Some(AccessDecision::Allow { user: Some(alice) })
        );
    }

    // TC: VIEW-02 (the admin opens anybody's)
    #[tokio::test]
    async fn allows_admin_when_artifact_is_another_users() {
        let world = World::new().await;
        let (_alice, id) = alices(&world).await;
        let decision = world
            .access()
            .check(
                &from_browser(&world, id, Some(&world.owner)).await,
                &LinkTarget::Artifact(id),
            )
            .await;
        assert_eq!(
            decision.ok(),
            Some(AccessDecision::Allow {
                user: Some(world.owner.clone())
            })
        );
    }

    // TC: VIEW-02 (anybody else is refused, and the `gate` hook is not
    // asked about an artifact even when it would let them in)
    #[tokio::test]
    async fn forbidden_without_asking_gate_when_session_is_another_users() {
        let world = World::new().await;
        let (_alice, id) = alices(&world).await;
        let carol = member(&world, "carol").await;
        world.ports.hooks.answer_next("gate", HookResult::Allow);
        let request = from_browser(&world, id, Some(&carol)).await;
        world.log().clear();
        let decision = world
            .access()
            .check(&request, &LinkTarget::Artifact(id))
            .await;
        assert_eq!(
            (
                matches!(decision, Ok(AccessDecision::Forbidden(_))),
                gate_calls(&world)
            ),
            (true, Vec::new()),
            "{decision:?}"
        );
    }

    // TC: VIEW-02 (a `cli` session is no browser session of the API host)
    #[tokio::test]
    async fn login_when_api_session_cookie_is_a_cli_session() {
        let world = World::new().await;
        let (alice, id) = alices(&world).await;
        let cli = world.login(&alice).await.unwrap();
        let decision = world
            .access()
            .check(
                &request(id, Some(cli), BROWSER_ACCEPT),
                &LinkTarget::Artifact(id),
            )
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Login));
    }

    // TC: VIEW-02 (the owner's `cli` session as the bearer, as for a
    // private environment)
    #[tokio::test]
    async fn allows_owner_when_bearer_is_owners_cli_session() {
        let world = World::new().await;
        let (alice, id) = alices(&world).await;
        let cli = world.login(&alice).await.unwrap();
        let mut request = request(id, None, "application/json");
        request.bearer = Some(Bearer::Session(cli));
        let decision = world
            .access()
            .check(&request, &LinkTarget::Artifact(id))
            .await;
        assert_eq!(
            decision.ok(),
            Some(AccessDecision::Allow { user: Some(alice) })
        );
    }

    /// The artifacts' own host of an installation that sets one.
    const ARTIFACTS_HOST: &str = "art.test";

    /// The world's check with `[artifacts] host = "art.test"`.
    fn access_with_artifacts_host(world: &World) -> Access<FakePorts> {
        let mut settings = (*world.settings).clone();
        settings.artifacts.host = Some(Host::new(ARTIFACTS_HOST).unwrap());
        Access::new(Arc::clone(&world.ports), Arc::new(settings))
    }

    /// A browser's request for `/<id>` on `host` with the host session
    /// `cookie` there.
    fn on_host(host: &str, id: ArtifactId, cookie: GateSecret) -> AccessRequest {
        AccessRequest {
            host: Host::new(host).unwrap(),
            cookie: Some(cookie),
            ..request(id, None, BROWSER_ACCEPT)
        }
    }

    // TC: VIEW-02, VIEW-08 (the artifacts' host has its own session, as
    // a private address: the owner's there opens it)
    #[tokio::test]
    async fn allows_owner_when_host_session_of_artifacts_host() {
        let world = World::new().await;
        let (alice, id) = alices(&world).await;
        let cookie = world.host_login(&alice, ARTIFACTS_HOST).await.unwrap();
        let decision = access_with_artifacts_host(&world)
            .check(
                &on_host(ARTIFACTS_HOST, id, cookie),
                &LinkTarget::Artifact(id),
            )
            .await;
        assert_eq!(
            decision.ok(),
            Some(AccessDecision::Allow { user: Some(alice) })
        );
    }

    // TC: VIEW-02, VIEW-08 (a session of another host counts as none)
    #[tokio::test]
    async fn login_when_host_session_is_of_another_host() {
        let world = World::new().await;
        let (alice, id) = alices(&world).await;
        let cookie = world
            .host_login(&alice, "shop-dev.apps.test")
            .await
            .unwrap();
        let decision = access_with_artifacts_host(&world)
            .check(
                &on_host(ARTIFACTS_HOST, id, cookie),
                &LinkTarget::Artifact(id),
            )
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Login));
    }

    // TC: VIEW-02 (a host session opens an artifact on the artifacts'
    // host alone: without one set, a host session counts as none)
    #[tokio::test]
    async fn login_when_host_session_and_no_artifacts_host() {
        let world = World::new().await;
        let (alice, id) = alices(&world).await;
        let cookie = world.host_login(&alice, World::API_HOST).await.unwrap();
        let decision = world
            .access()
            .check(
                &on_host(World::API_HOST, id, cookie),
                &LinkTarget::Artifact(id),
            )
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Login));
    }

    // TC: ART-08 (an expired artifact is gone before the sweeper ran)
    #[tokio::test]
    async fn not_found_when_private_artifact_expired() {
        let world = World::new().await;
        let (alice, id) = alices(&world).await;
        let request = from_browser(&world, id, Some(&alice)).await;
        world.ports.clock.advance(Duration::hours(5));
        let decision = world
            .access()
            .check(&request, &LinkTarget::Artifact(id))
            .await;
        assert!(matches!(decision, Err(Error::NotFound(_))), "{decision:?}");
    }

    // TC: VIEW-05 (the core's decision for an unknown artifact)
    #[tokio::test]
    async fn not_found_when_artifact_unknown() {
        let world = World::new().await;
        let unknown = ArtifactId::parse("zzzzzzzzzzzz").unwrap();
        let decision = world
            .access()
            .check(
                &from_browser(&world, unknown, Some(&world.owner)).await,
                &LinkTarget::Artifact(unknown),
            )
            .await;
        assert!(matches!(decision, Err(Error::NotFound(_))), "{decision:?}");
    }
}

mod view {
    use super::*;

    /// Whether `viewer` sees the share or unshare button of `id`.
    async fn may_change(world: &World, id: ArtifactId, viewer: Option<&User>) -> Option<bool> {
        world
            .artifacts()
            .view(id, viewer)
            .await
            .ok()
            .map(|view| view.may_change_visibility)
    }

    // TC: VIEW-01, ART-06 (the owner, here the admin, gets the buttons)
    #[tokio::test]
    async fn may_change_visibility_when_owner_views() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        assert_eq!(may_change(&world, id, Some(&world.owner)).await, Some(true));
    }

    // TC: VIEW-01 (nobody without a session)
    #[tokio::test]
    async fn may_not_change_visibility_without_viewer() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        assert_eq!(may_change(&world, id, None).await, Some(false));
    }

    // TC: VIEW-01, ART-11 (another user who is no admin)
    #[tokio::test]
    async fn may_not_change_visibility_when_another_user_views() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        let carol = member(&world, "carol").await;
        allow_next(&world);
        assert_eq!(may_change(&world, id, Some(&carol)).await, Some(false));
    }

    // TC: VIEW-01, MOD-04 (the `authorize` hook decides, as for the
    // share itself: a denial hides the buttons even from the admin)
    #[tokio::test]
    async fn may_not_change_visibility_when_authorize_denies_owner() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Deny("read-only".to_owned()));
        assert_eq!(
            may_change(&world, id, Some(&world.owner)).await,
            Some(false)
        );
    }

    // TC: VIEW-01 (an owner who is no admin, once the hook allows it)
    #[tokio::test]
    async fn may_change_visibility_when_hook_allows_non_admin_owner() {
        let world = World::new().await;
        let (alice, id) = {
            let alice = member(&world, "alice").await;
            allow_next(&world);
            let id = published(&world, &alice, "<h1>v1</h1>").await.artifact.id;
            (alice, id)
        };
        allow_next(&world);
        assert_eq!(may_change(&world, id, Some(&alice)).await, Some(true));
    }

    // TC: VIEW-01, MOD-05 (without a module that answers, an owner who
    // is no admin may not share, so sees no button)
    #[tokio::test]
    async fn may_not_change_visibility_when_no_module_answers_for_non_admin_owner() {
        let world = World::new().await;
        let alice = member(&world, "alice").await;
        allow_next(&world);
        let id = published(&world, &alice, "<h1>v1</h1>").await.artifact.id;
        assert_eq!(may_change(&world, id, Some(&alice)).await, Some(false));
    }
}

mod get {
    use super::*;

    // TC: ART-17 (`GET /artifacts/{id}`: the artifact as stored now)
    #[tokio::test]
    async fn answers_stored_artifact_when_owner_asks() {
        let world = World::new().await;
        let published = published(&world, &world.owner, "<h1>v1</h1>").await;
        let got = world
            .artifacts()
            .get(&world.owner, published.artifact.id)
            .await;
        assert_eq!(got.ok(), Some(published.artifact));
    }

    // TC: ART-18 (ART-11: another user's artifact is refused)
    #[tokio::test]
    async fn forbidden_when_artifact_is_another_users() {
        let world = World::new().await;
        let alice = member(&world, "alice").await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        allow_next(&world);
        let got = world.artifacts().get(&alice, id).await;
        assert!(matches!(got, Err(Error::Forbidden(_))), "{got:?}");
    }

    // TC: ART-18 (an expired artifact no longer exists)
    #[tokio::test]
    async fn not_found_when_expired() {
        let world = World::new().await;
        let id = published(&world, &world.owner, "<h1>v1</h1>")
            .await
            .artifact
            .id;
        world.ports.clock.advance(Duration::hours(6));
        let got = world.artifacts().get(&world.owner, id).await;
        assert!(matches!(got, Err(Error::NotFound(_))), "{got:?}");
    }
}
