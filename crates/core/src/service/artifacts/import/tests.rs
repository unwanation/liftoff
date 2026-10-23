//! IMPA-02…04: the rules of [`ArtifactImport`] over the shared [`World`]: the owner by e-mail, the admin otherwise,
//! an id already stored, and a failed import taken back. The reading of
//! the predecessor is the daemon's (`crates/liftoffd/tests/import_artifacts.rs`).

use time::Duration;

use super::*;
use crate::domain::{ArtifactPath, UserRole};
use crate::fakes::{BlobCall, Call, World};

/// The id every test imports.
const ID: &str = "aB3dE6gH9jK2";

fn id() -> ArtifactId {
    ArtifactId::parse(ID).unwrap()
}

/// A shared artifact `ID` of the owner `email` with one file beside its
/// HTML.
fn imported(email: Option<&str>) -> ImportedArtifact {
    let now = time::OffsetDateTime::UNIX_EPOCH;
    ImportedArtifact {
        id: id(),
        owner_email: email.map(str::to_owned),
        title: "Report".to_owned(),
        shared: true,
        created_at: now,
        updated_at: now,
        expires_at: None,
        html: b"<h1>report</h1>".to_vec(),
        files: vec![ArtifactUpload {
            name: ArtifactPath::new("style.css").unwrap(),
            bytes: b"body{}".to_vec(),
        }],
    }
}

/// A failure of the store or the blobs, scripted by a test.
fn disk_full() -> Error {
    Error::internal_msg("disk full")
}

/// Imports `artifact` into the world's store and blobs.
async fn import(world: &World, artifact: ImportedArtifact) -> Result<ImportOutcome, Error> {
    ArtifactImport::new(&world.ports.store, &world.ports.blobs)
        .await?
        .import(artifact)
        .await
}

/// The owner of the stored `ID`.
async fn owner_of(world: &World) -> crate::domain::UserId {
    world
        .ports
        .store
        .artifact(id())
        .await
        .unwrap()
        .unwrap()
        .owner_user_id
}

mod new {
    use super::*;

    // TC: IMPA-02 (without an admin nobody can own what matches no one)
    #[tokio::test]
    async fn fails_with_not_found_when_there_is_no_admin() {
        let world = World::new().await;
        let demoted = User {
            role: UserRole::User,
            ..world.owner.clone()
        };
        world.ports.store.update_user(&demoted).await.unwrap();
        let result = ArtifactImport::new(&world.ports.store, &world.ports.blobs).await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }
}

mod import {
    use super::*;

    // TC: IMPA-02
    #[tokio::test]
    async fn owns_by_the_user_with_the_email_when_one_has_it() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let outcome = import(&world, imported(Some("alice@example.com"))).await;
        assert_eq!(
            (outcome.unwrap(), owner_of(&world).await),
            (
                ImportOutcome::Imported {
                    owner_defaulted: false
                },
                alice.id
            )
        );
    }

    // TC: IMPA-02 (no user has the e-mail → the admin)
    #[tokio::test]
    async fn owns_by_the_admin_when_no_user_has_the_email() {
        let world = World::new().await;
        let outcome = import(&world, imported(Some("nobody@elsewhere.test"))).await;
        assert_eq!(
            (outcome.unwrap(), owner_of(&world).await),
            (
                ImportOutcome::Imported {
                    owner_defaulted: true
                },
                world.owner.id
            )
        );
    }

    // TC: IMPA-02 (an empty e-mail is none)
    #[tokio::test]
    async fn owns_by_the_admin_when_the_email_is_empty() {
        let world = World::new().await;
        let mut nameless = world.records.user("nameless", UserRole::User).unwrap();
        nameless.email = String::new();
        world.ports.store.insert_user(&nameless).await.unwrap();
        let outcome = import(&world, imported(Some(""))).await;
        assert_eq!(
            (outcome.unwrap(), owner_of(&world).await),
            (
                ImportOutcome::Imported {
                    owner_defaulted: true
                },
                world.owner.id
            )
        );
    }

    // TC: IMPA-02 (an e-mail two users share names neither: the admin
    // owns, not whoever sorts first)
    #[tokio::test]
    async fn owns_by_the_admin_when_two_users_have_the_email() {
        let world = World::new().await;
        world.add_user("alice", UserRole::User).await.unwrap();
        let mut twin = world.records.user("aaron", UserRole::User).unwrap();
        twin.email = "alice@example.com".to_owned();
        world.ports.store.insert_user(&twin).await.unwrap();
        let outcome = import(&world, imported(Some("alice@example.com"))).await;
        assert_eq!(
            (outcome.unwrap(), owner_of(&world).await),
            (
                ImportOutcome::Imported {
                    owner_defaulted: true
                },
                world.owner.id
            )
        );
    }

    // TC: IMPA-02 (the admin is the first created, whatever the logins)
    #[tokio::test]
    async fn owns_by_the_earliest_created_admin_when_there_are_two() {
        let world = World::new().await;
        world.ports.clock.advance(Duration::hours(1));
        world.add_user("aaron", UserRole::Admin).await.unwrap();
        import(&world, imported(None)).await.unwrap();
        assert_eq!(owner_of(&world).await, world.owner.id);
    }

    // TC: IMPA-02 (two admins created at once: the first by login)
    #[tokio::test]
    async fn owns_by_the_first_admin_by_login_when_created_at_once() {
        let world = World::new().await;
        let aaron = world.add_user("aaron", UserRole::Admin).await.unwrap();
        import(&world, imported(None)).await.unwrap();
        assert_eq!(owner_of(&world).await, aaron.id);
    }

    // TC: IMPA-04
    #[tokio::test]
    async fn already_stored_when_the_id_is_stored() {
        let world = World::new().await;
        import(&world, imported(None)).await.unwrap();
        world.log().clear();
        let outcome = import(&world, imported(Some("other@example.com"))).await;
        assert_eq!(
            (
                outcome.unwrap(),
                world
                    .log()
                    .filter(|call| matches!(call, Call::Store(_) | Call::Blob(_)))
            ),
            (ImportOutcome::AlreadyStored, Vec::new())
        );
    }

    // TC: IMPA-04 (an id stored between the check and the insert)
    #[tokio::test]
    async fn already_stored_when_the_insert_conflicts() {
        let world = World::new().await;
        world
            .ports
            .store
            .fail_next("insert_artifact", Error::conflict("artifact aB3dE6gH9jK2"));
        let outcome = import(&world, imported(None)).await;
        assert_eq!(
            (
                outcome.unwrap(),
                world.log().filter(|call| matches!(call, Call::Blob(_)))
            ),
            (ImportOutcome::AlreadyStored, Vec::new())
        );
    }

    // TC: IMPA-03 (a version whose files cannot be linked is taken back)
    #[tokio::test]
    async fn nothing_left_when_link_fails() {
        let world = World::new().await;
        world.ports.blobs.fail_next("link", disk_full());
        let result = import(&world, imported(None)).await;
        let calls = world.log().calls();
        assert_eq!(
            (
                matches!(result, Err(Error::Internal(_))),
                world.ports.store.artifact(id()).await.unwrap(),
                world.ports.blobs.versions(id()),
                calls.contains(&Call::Blob(BlobCall::RemoveArtifact(id()))),
                calls.contains(&Call::Store("delete_artifact")),
            ),
            (true, None, Vec::new(), true, true),
            "{result:?}"
        );
    }

    // TC: IMPA-03 (a blob that cannot be written: the rows are taken back)
    #[tokio::test]
    async fn nothing_left_when_put_fails() {
        let world = World::new().await;
        world.ports.blobs.fail_next("put", disk_full());
        let result = import(&world, imported(None)).await;
        assert_eq!(
            (
                matches!(result, Err(Error::Internal(_))),
                world.ports.store.artifact(id()).await.unwrap(),
                world.ports.blobs.blobs(),
            ),
            (true, None, Vec::new()),
            "{result:?}"
        );
    }

    // TC: IMPA-04 (an import interrupted before its files, and not taken
    // back, is not "already stored": a new run writes it again)
    #[tokio::test]
    async fn imports_again_when_an_earlier_run_left_the_artifact_without_files() {
        let world = World::new().await;
        world.ports.blobs.fail_next("link", disk_full());
        world.ports.blobs.fail_next("remove_artifact", disk_full());
        import(&world, imported(None)).await.unwrap_err();
        let outcome = import(&world, imported(None)).await;
        let html = world
            .ports
            .blobs
            .read(id(), VersionNumber::FIRST, &ArtifactPath::index())
            .await;
        assert_eq!(
            (outcome.unwrap(), html.unwrap()),
            (
                ImportOutcome::Imported {
                    owner_defaulted: true
                },
                b"<h1>report</h1>".to_vec()
            )
        );
    }

    // TC: IMPA-04 (such an artifact is not counted as stored)
    #[tokio::test]
    async fn not_stored_when_an_earlier_run_left_the_artifact_without_files() {
        let world = World::new().await;
        world.ports.blobs.fail_next("link", disk_full());
        world.ports.blobs.fail_next("remove_artifact", disk_full());
        import(&world, imported(None)).await.unwrap_err();
        let import = ArtifactImport::new(&world.ports.store, &world.ports.blobs)
            .await
            .unwrap();
        assert!(!import.is_stored(id()).await.unwrap());
    }
}
