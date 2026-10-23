//! Tests of [`SecretSync`] over the shared [`World`] (`SYNC-01`…`SYNC-04`).
//!
//! The world's `owner` is admin of `owner/shop` on the fake forge; `shop`
//! holds `DATABASE_URL` and `LOG_LEVEL` (both `ci = false`), `qa` holds
//! `DATABASE_URL`.

use super::*;
use crate::domain::{
    RepoRef, ScopeRef, SecretScope, SecretSyncRecord, SecretValue, SyncTarget, UserRole,
    VersionNumber,
};
use crate::error::System;
use crate::fakes::{GitCall, World};
use crate::ports::{Action, HookResult, Resource};
use time::Duration;

fn shop_name() -> ProjectName {
    ProjectName::new("shop").unwrap()
}

fn key(raw: &str) -> SecretKey {
    SecretKey::new(raw).unwrap()
}

fn repo() -> RepoRef {
    RepoRef::new("owner/shop").unwrap()
}

fn shop_scope() -> ScopeRef {
    ScopeRef::Project(shop_name())
}

/// Sets the project secret `name = value` of `shop` with the `ci` flag.
async fn set(world: &World, name: &str, value: &str, ci: bool) {
    let _entry = world
        .secrets()
        .set(
            &world.owner,
            &shop_scope(),
            key(name),
            SecretValue::new(value.to_owned()),
            ci,
        )
        .await
        .unwrap();
}

/// What the fake forge holds as the CI secret `name` of `owner/shop`.
fn on_forge(world: &World, name: &str) -> Option<String> {
    world
        .ports
        .git
        .ci_secret(&repo(), &key(name))
        .map(|value| value.expose().to_owned())
}

/// The keys `put_ci_secret` was called with, oldest first.
fn sent(world: &World) -> Vec<String> {
    world
        .ports
        .git
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            GitCall::PutCiSecret { key, .. } => Some(key.to_string()),
            _ => None,
        })
        .collect()
}

/// The stored sync row of the project secret `name` of `shop`.
async fn sync_row(world: &World, name: &str) -> Option<SecretSyncRecord> {
    let secret = world
        .ports
        .store
        .secret(&SecretScope::Project(world.shop.id), &key(name))
        .await
        .unwrap()
        .unwrap();
    world
        .ports
        .store
        .secret_sync(secret.id, SyncTarget::Ci)
        .await
        .unwrap()
}

fn report(synced: &[&str], unchanged: &[&str], failed: &[(&str, &str)]) -> SyncReport {
    SyncReport {
        synced: synced.iter().map(|raw| key(raw)).collect(),
        unchanged: unchanged.iter().map(|raw| key(raw)).collect(),
        failed: failed
            .iter()
            .map(|(raw, error)| SyncFailure {
                key: key(raw),
                error: (*error).to_owned(),
            })
            .collect(),
    }
}

mod sync {
    use super::*;

    // TC: SYNC-01
    #[tokio::test]
    async fn sends_only_ci_secrets_when_synced() {
        let world = World::new().await;
        set(&world, "REGISTRY_PASSWORD", "hunter2", true).await;
        let synced = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        assert_eq!(
            (
                synced,
                on_forge(&world, "REGISTRY_PASSWORD"),
                on_forge(&world, "DATABASE_URL"),
                on_forge(&world, "LOG_LEVEL"),
            ),
            (
                report(&["REGISTRY_PASSWORD"], &[], &[]),
                Some("hunter2".to_owned()),
                None,
                None
            )
        );
    }

    // TC: SYNC-01 (a repository's CI secrets are not per environment)
    #[tokio::test]
    async fn skips_environment_secret_when_its_ci_flag_is_set() {
        let world = World::new().await;
        // `secret set` refuses `--ci` here (SYNC-04); a row may still carry
        // the flag (written before that rule, or by another store client).
        let sealed = world.ports.secrets.encrypt("qa-token").unwrap();
        let mut flagged = world
            .records
            .secret(
                SecretScope::Environment(world.qa.id),
                "QA_TOKEN",
                sealed,
                &world.owner,
            )
            .unwrap();
        flagged.ci = true;
        world.ports.store.put_secret(&flagged).await.unwrap();
        let synced = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        assert_eq!(
            (synced, on_forge(&world, "QA_TOKEN")),
            (report(&[], &[], &[]), None)
        );
    }

    // TC: SYNC-01 (what and when went to CI)
    #[tokio::test]
    async fn records_sent_version_and_time_when_synced() {
        let world = World::new().await;
        set(&world, "REGISTRY_PASSWORD", "hunter2", true).await;
        let _synced = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        let secret_id = world
            .ports
            .store
            .secret(
                &SecretScope::Project(world.shop.id),
                &key("REGISTRY_PASSWORD"),
            )
            .await
            .unwrap()
            .unwrap()
            .id;
        assert_eq!(
            sync_row(&world, "REGISTRY_PASSWORD").await,
            Some(SecretSyncRecord {
                secret_id,
                target: SyncTarget::Ci,
                version: Some(VersionNumber::FIRST),
                synced_at: Some(world.ports.clock.now()),
                error: None,
            })
        );
    }

    // TC: SYNC-01 (every operation asks `authorize` with its action)
    #[tokio::test]
    async fn asks_authorize_once_about_the_project_scope_when_synced() {
        let world = World::new().await;
        set(&world, "REGISTRY_PASSWORD", "hunter2", true).await;
        world.log().clear();
        let _synced = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        assert_eq!(
            world.log().authorized(),
            vec![(
                world.owner.id,
                Action::SecretSync,
                Resource::Secret(SecretScope::Project(world.shop.id))
            )]
        );
    }

    // TC: SYNC-01 (a refusal sends nothing)
    #[tokio::test]
    async fn forbidden_and_nothing_sent_when_authorize_denies() {
        let world = World::new().await;
        set(&world, "REGISTRY_PASSWORD", "hunter2", true).await;
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Deny("not yours".to_owned()));
        let result = world.secret_sync().sync(&world.owner, &shop_name()).await;
        assert!(
            matches!(result, Err(Error::Forbidden(_))) && sent(&world).is_empty(),
            "{result:?} {:?}",
            sent(&world)
        );
    }

    // TC: SYNC-01 (single-user mode: only the admin owns the project)
    #[tokio::test]
    async fn forbidden_when_actor_does_not_own_the_project() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        set(&world, "REGISTRY_PASSWORD", "hunter2", true).await;
        let result = world.secret_sync().sync(&carol, &shop_name()).await;
        assert!(
            matches!(result, Err(Error::Forbidden(_))) && sent(&world).is_empty(),
            "{result:?}"
        );
    }

    // TC: SYNC-01
    #[tokio::test]
    async fn not_found_when_project_is_unknown() {
        let world = World::new().await;
        let result = world
            .secret_sync()
            .sync(&world.owner, &ProjectName::new("nope").unwrap())
            .await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }

    // TC: SYNC-02
    #[tokio::test]
    async fn records_error_and_sends_the_rest_when_forge_fails_on_one() {
        let world = World::new().await;
        set(&world, "A_TOKEN", "a", true).await;
        set(&world, "B_TOKEN", "b", true).await;
        world.ports.git.fail_next(
            "put_ci_secret",
            Error::upstream(System::Forge, "502 Bad Gateway"),
        );
        let synced = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        assert_eq!(
            (synced, on_forge(&world, "B_TOKEN")),
            (
                report(&["B_TOKEN"], &[], &[("A_TOKEN", "forge: 502 Bad Gateway")]),
                Some("b".to_owned())
            )
        );
    }

    // TC: SYNC-02 (`secret_syncs.error`)
    #[tokio::test]
    async fn stores_forge_error_in_secret_syncs_when_forge_fails() {
        let world = World::new().await;
        set(&world, "A_TOKEN", "a", true).await;
        world.ports.git.fail_next(
            "put_ci_secret",
            Error::upstream(System::Forge, "502 Bad Gateway"),
        );
        let _synced = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        let row = sync_row(&world, "A_TOKEN").await.unwrap();
        assert_eq!(
            (row.version, row.synced_at, row.error),
            (None, None, Some("forge: 502 Bad Gateway".to_owned()))
        );
    }

    // TC: SYNC-02 (a failed attempt keeps what the forge holds)
    #[tokio::test]
    async fn keeps_last_sent_version_when_forge_fails_after_a_sync() {
        let world = World::new().await;
        set(&world, "A_TOKEN", "a", true).await;
        let _first = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        let sent_at = world.ports.clock.now();
        world.ports.clock.advance(Duration::minutes(5));
        set(&world, "A_TOKEN", "b", true).await;
        world.ports.git.fail_next(
            "put_ci_secret",
            Error::upstream(System::Forge, "502 Bad Gateway"),
        );
        let _second = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        let row = sync_row(&world, "A_TOKEN").await.unwrap();
        assert_eq!(
            (
                row.version,
                row.synced_at,
                row.error,
                on_forge(&world, "A_TOKEN")
            ),
            (
                Some(VersionNumber::FIRST),
                Some(sent_at),
                Some("forge: 502 Bad Gateway".to_owned()),
                Some("a".to_owned())
            )
        );
    }

    // TC: SYNC-02 (a failed secret is not "unchanged" next time)
    #[tokio::test]
    async fn sends_failed_secret_again_when_synced_again() {
        let world = World::new().await;
        set(&world, "A_TOKEN", "a", true).await;
        set(&world, "B_TOKEN", "b", true).await;
        world.ports.git.fail_next(
            "put_ci_secret",
            Error::upstream(System::Forge, "502 Bad Gateway"),
        );
        let _first = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        let second = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        assert_eq!(
            (second, on_forge(&world, "A_TOKEN")),
            (
                report(&["A_TOKEN"], &["B_TOKEN"], &[]),
                Some("a".to_owned())
            )
        );
    }

    // TC: SYNC-03
    #[tokio::test]
    async fn sends_nothing_when_synced_again_without_change() {
        let world = World::new().await;
        set(&world, "REGISTRY_PASSWORD", "hunter2", true).await;
        let _first = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        world.log().clear();
        let second = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        assert_eq!(
            (second, sent(&world)),
            (
                report(&[], &["REGISTRY_PASSWORD"], &[]),
                Vec::<String>::new()
            )
        );
    }

    // TC: SYNC-03 (`token rotate` sent `LIFTOFF_TOKEN` itself and stores
    // nothing else: the sync after it sends nothing)
    #[tokio::test]
    async fn sends_nothing_when_synced_after_token_rotate() {
        let world = World::new().await;
        let _token = world
            .deploys()
            .rotate_token(&world.owner, &shop_name())
            .await
            .unwrap();
        world.log().clear();
        let synced = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        assert_eq!(
            (synced, sent(&world)),
            (report(&[], &["LIFTOFF_TOKEN"], &[]), Vec::<String>::new())
        );
    }

    // TC: SYNC-04
    #[tokio::test]
    async fn sends_new_value_when_ci_secret_set_again_after_sync() {
        let world = World::new().await;
        set(&world, "REGISTRY_PASSWORD", "hunter2", true).await;
        let _first = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        set(&world, "REGISTRY_PASSWORD", "hunter3", true).await;
        let second = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        assert_eq!(
            (second, on_forge(&world, "REGISTRY_PASSWORD")),
            (
                report(&["REGISTRY_PASSWORD"], &[], &[]),
                Some("hunter3".to_owned())
            )
        );
    }

    // TC: SYNC-04 (`set --ci` on an existing secret turns the flag on)
    #[tokio::test]
    async fn sends_existing_secret_when_set_with_ci_after_sync() {
        let world = World::new().await;
        let first = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        set(&world, "LOG_LEVEL", "debug", true).await;
        let second = world
            .secret_sync()
            .sync(&world.owner, &shop_name())
            .await
            .unwrap();
        assert_eq!(
            (first, second, on_forge(&world, "LOG_LEVEL")),
            (
                report(&[], &[], &[]),
                report(&["LOG_LEVEL"], &[], &[]),
                Some("debug".to_owned())
            )
        );
    }
}
