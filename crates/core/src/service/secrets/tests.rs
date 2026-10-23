//! Tests of [`SecretsService`] over the shared [`World`].
//!
//! The world holds `shop` secrets `DATABASE_URL = postgres://db/shop`,
//! `LOG_LEVEL = info` and the `qa` secret `DATABASE_URL =
//! postgres://db/shop_qa`, each at version 1; `shop.apps.test` is bound to
//! `prod`, nothing to `qa`.

use super::*;
use crate::domain::{Audit, Project, Secret, SecretScope, SecretVersion, UserRole};
use crate::fakes::World;

fn project(raw: &str) -> ProjectName {
    ProjectName::new(raw).unwrap()
}

fn env(raw: &str) -> EnvName {
    EnvName::new(raw).unwrap()
}

fn key(raw: &str) -> SecretKey {
    SecretKey::new(raw).unwrap()
}

fn value(raw: &str) -> SecretValue {
    SecretValue::new(raw.to_owned())
}

fn version(number: u32) -> VersionNumber {
    VersionNumber::new(number).unwrap()
}

fn tag(raw: &str) -> ImageTag {
    ImageTag::new(raw).unwrap()
}

fn shop() -> ScopeRef {
    ScopeRef::Project(project("shop"))
}

fn qa() -> ScopeRef {
    ScopeRef::Environment(project("shop"), env("qa"))
}

/// `(key, value)` of a resolved env, for literal comparison.
fn pairs(env: &Env) -> Vec<(String, String)> {
    env.vars()
        .iter()
        .map(|(key, value)| (key.to_string(), value.expose().to_owned()))
        .collect()
}

fn owned(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// The stored row of `key` in `qa`.
async fn stored_qa(world: &World, raw: &str) -> Option<Secret> {
    world
        .ports
        .store
        .secret(&SecretScope::Environment(world.qa.id), &key(raw))
        .await
        .unwrap()
}

/// The stored versions of a secret, newest first.
async fn versions(world: &World, secret: &Secret) -> Vec<SecretVersion> {
    world.ports.store.secret_versions(secret.id).await.unwrap()
}

/// The stored versions as `(number, plaintext)`, newest first.
async fn opened_versions(world: &World, secret: &Secret) -> Vec<(u32, String)> {
    versions(world, secret)
        .await
        .into_iter()
        .map(|v| {
            (
                v.version.get(),
                world.ports.secrets.decrypt(&v.value).unwrap(),
            )
        })
        .collect()
}

/// The audit log as `(actor, action, resource)`.
async fn audit(world: &World) -> Vec<(Option<crate::domain::UserId>, String, String)> {
    world
        .ports
        .store
        .audit_log()
        .await
        .unwrap()
        .into_iter()
        .map(|entry: Audit| (entry.actor_user_id, entry.action, entry.resource))
        .collect()
}

fn audited(
    world: &World,
    action: &str,
    resource: &str,
) -> Vec<(Option<crate::domain::UserId>, String, String)> {
    vec![(Some(world.owner.id), action.to_owned(), resource.to_owned())]
}

mod set {
    use super::*;

    // TC: SEC-02
    #[tokio::test]
    async fn creates_version_1_when_key_new() {
        let world = World::new().await;
        let entry = world
            .secrets()
            .set(&world.owner, &qa(), key("API_KEY"), value("k-1"), false)
            .await
            .unwrap();
        assert_eq!(
            entry,
            SecretEntry {
                key: key("API_KEY"),
                scope: ScopeKind::Environment,
                version: version(1),
                ci: false,
                updated_by: UpdatedBy::User(world.owner.id),
                updated_at: world.ports.clock.now(),
            }
        );
    }

    // TC: SEC-02
    #[tokio::test]
    async fn creates_version_2_when_set_again() {
        let world = World::new().await;
        let secrets = world.secrets();
        secrets
            .set(&world.owner, &qa(), key("API_KEY"), value("k-1"), false)
            .await
            .unwrap();
        let entry = secrets
            .set(&world.owner, &qa(), key("API_KEY"), value("k-2"), false)
            .await
            .unwrap();
        assert_eq!(entry.version, version(2));
    }

    // TC: SEC-02
    #[tokio::test]
    async fn keeps_both_versions_when_set_twice() {
        let world = World::new().await;
        let secrets = world.secrets();
        secrets
            .set(&world.owner, &qa(), key("API_KEY"), value("k-1"), false)
            .await
            .unwrap();
        secrets
            .set(&world.owner, &qa(), key("API_KEY"), value("k-2"), false)
            .await
            .unwrap();
        let stored = stored_qa(&world, "API_KEY").await.unwrap();
        assert_eq!(
            opened_versions(&world, &stored).await,
            vec![(2, "k-2".to_owned()), (1, "k-1".to_owned())]
        );
    }

    // TC: SEC-02
    #[tokio::test]
    async fn stores_version_2_of_existing_world_secret_when_overwritten() {
        let world = World::new().await;
        world
            .secrets()
            .set(
                &world.owner,
                &qa(),
                key("DATABASE_URL"),
                value("postgres://db/shop_qa2"),
                false,
            )
            .await
            .unwrap();
        let stored = stored_qa(&world, "DATABASE_URL").await.unwrap();
        assert_eq!(
            (
                stored.id,
                stored.version.get(),
                world.ports.secrets.decrypt(&stored.value).unwrap()
            ),
            (
                world.qa_secrets[0].id,
                2,
                "postgres://db/shop_qa2".to_owned()
            )
        );
    }

    // TC: SEC-03
    #[tokio::test]
    async fn keeps_20_newest_versions_when_set_25_times() {
        let world = World::new().await;
        let secrets = world.secrets();
        for n in 1..=25 {
            secrets
                .set(
                    &world.owner,
                    &qa(),
                    key("API_KEY"),
                    value(&format!("k-{n}")),
                    false,
                )
                .await
                .unwrap();
        }
        let stored = stored_qa(&world, "API_KEY").await.unwrap();
        let kept: Vec<u32> = versions(&world, &stored)
            .await
            .iter()
            .map(|v| v.version.get())
            .collect();
        assert_eq!(kept, (6..=25).rev().collect::<Vec<u32>>());
    }

    // TC: SEC-03
    #[tokio::test]
    async fn keeps_all_versions_when_set_exactly_20_times() {
        let world = World::new().await;
        let secrets = world.secrets();
        for n in 1..=20 {
            secrets
                .set(
                    &world.owner,
                    &qa(),
                    key("API_KEY"),
                    value(&format!("k-{n}")),
                    false,
                )
                .await
                .unwrap();
        }
        let stored = stored_qa(&world, "API_KEY").await.unwrap();
        assert_eq!(versions(&world, &stored).await.len(), 20);
    }

    // TC: SEC-03
    #[tokio::test]
    async fn numbers_version_21_after_pruning_when_set_21_times() {
        let world = World::new().await;
        let secrets = world.secrets();
        let mut last = None;
        for n in 1..=21 {
            last = Some(
                secrets
                    .set(
                        &world.owner,
                        &qa(),
                        key("API_KEY"),
                        value(&format!("k-{n}")),
                        false,
                    )
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(last.map(|entry| entry.version.get()), Some(21));
    }

    // TC: SEC-04 (`ls` shows the `ci` flag: it is stored)
    #[tokio::test]
    async fn stores_ci_flag_when_set_with_ci() {
        let world = World::new().await;
        world
            .secrets()
            .set(
                &world.owner,
                &shop(),
                key("REGISTRY_TOKEN"),
                value("r-1"),
                true,
            )
            .await
            .unwrap();
        let stored = world
            .ports
            .store
            .secret(&SecretScope::Project(world.shop.id), &key("REGISTRY_TOKEN"))
            .await
            .unwrap()
            .unwrap();
        assert!(stored.ci, "the ci flag must be stored");
    }

    // TC: SYNC-04 (only project secrets reach CI: `--ci` elsewhere would
    // never be sent)
    #[tokio::test]
    async fn validation_on_ci_when_scope_is_an_environment() {
        let world = World::new().await;
        let result = world
            .secrets()
            .set(&world.owner, &qa(), key("API_KEY"), value("k-1"), true)
            .await;
        assert!(
            matches!(&result, Err(Error::Validation(v))
                if v.field == "ci" && v.to_string().contains("--scope project")),
            "{result:?}"
        );
    }

    // TC: SYNC-04
    #[tokio::test]
    async fn stores_nothing_when_ci_is_refused() {
        let world = World::new().await;
        let _refused = world
            .secrets()
            .set(&world.owner, &qa(), key("API_KEY"), value("k-1"), true)
            .await;
        assert_eq!(stored_qa(&world, "API_KEY").await, None);
    }

    // TC: SEC-02
    #[tokio::test]
    async fn writes_project_scope_when_scope_is_project() {
        let world = World::new().await;
        world
            .secrets()
            .set(&world.owner, &shop(), key("API_KEY"), value("p-1"), false)
            .await
            .unwrap();
        let (project, qa) = (
            world
                .ports
                .store
                .secret(&SecretScope::Project(world.shop.id), &key("API_KEY"))
                .await
                .unwrap()
                .is_some(),
            stored_qa(&world, "API_KEY").await.is_some(),
        );
        assert_eq!((project, qa), (true, false));
    }

    // TC: SEC-02
    #[tokio::test]
    async fn not_found_when_environment_unknown() {
        let world = World::new().await;
        let error = world
            .secrets()
            .set(
                &world.owner,
                &ScopeRef::Environment(project("shop"), env("staging")),
                key("API_KEY"),
                value("x"),
                false,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    // TC: SEC-02
    #[tokio::test]
    async fn not_found_when_project_unknown() {
        let world = World::new().await;
        let error = world
            .secrets()
            .set(
                &world.owner,
                &ScopeRef::Project(project("blog")),
                key("API_KEY"),
                value("x"),
                false,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    // TC: SEC-10 (service half: only the scope owner writes)
    #[tokio::test]
    async fn forbidden_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let error = world
            .secrets()
            .set(&guest, &qa(), key("API_KEY"), value("x"), false)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Forbidden(_)), "{error:?}");
    }

    // TC: SEC-10 (service half: a refused write stores nothing)
    #[tokio::test]
    async fn stores_nothing_when_forbidden() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let _refused = world
            .secrets()
            .set(&guest, &qa(), key("API_KEY"), value("x"), false)
            .await;
        assert_eq!(stored_qa(&world, "API_KEY").await, None);
    }

    // TC: SEC-08 (the audit entry goes first: a failed audit stores nothing)
    #[tokio::test]
    async fn stores_nothing_when_audit_fails() {
        let world = World::new().await;
        world
            .ports
            .store
            .fail_next("insert_audit", Error::internal_msg("disk full"));
        let result = world
            .secrets()
            .set(&world.owner, &qa(), key("API_KEY"), value("x"), false)
            .await;
        assert_eq!(
            (result.is_err(), stored_qa(&world, "API_KEY").await),
            (true, None)
        );
    }
}

mod get {
    use super::*;

    // TC: SEC-04
    #[tokio::test]
    async fn returns_current_value_when_owner_asks() {
        let world = World::new().await;
        let got = world
            .secrets()
            .get(&world.owner, &qa(), &key("DATABASE_URL"))
            .await
            .unwrap();
        assert_eq!(got.expose(), "postgres://db/shop_qa");
    }

    // TC: SEC-04
    #[tokio::test]
    async fn returns_latest_value_when_set_twice() {
        let world = World::new().await;
        let secrets = world.secrets();
        for raw in ["k-1", "k-2"] {
            secrets
                .set(&world.owner, &shop(), key("API_KEY"), value(raw), false)
                .await
                .unwrap();
        }
        let got = secrets
            .get(&world.owner, &shop(), &key("API_KEY"))
            .await
            .unwrap();
        assert_eq!(got.expose(), "k-2");
    }

    // TC: SEC-04
    #[tokio::test]
    async fn returns_project_value_when_scope_is_project() {
        let world = World::new().await;
        let got = world
            .secrets()
            .get(&world.owner, &shop(), &key("DATABASE_URL"))
            .await
            .unwrap();
        assert_eq!(got.expose(), "postgres://db/shop");
    }

    // TC: SEC-04
    #[tokio::test]
    async fn not_found_when_key_unknown_in_scope() {
        let world = World::new().await;
        let error = world
            .secrets()
            .get(&world.owner, &qa(), &key("LOG_LEVEL"))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    // TC: SEC-10 (service half: in single-user mode only the admin owns
    // a scope)
    #[tokio::test]
    async fn forbidden_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let error = world
            .secrets()
            .get(&guest, &qa(), &key("DATABASE_URL"))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Forbidden(_)), "{error:?}");
    }

    // TC: SEC-10 (service half)
    #[tokio::test]
    async fn forbidden_without_value_in_message_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let error = world
            .secrets()
            .get(&guest, &qa(), &key("DATABASE_URL"))
            .await
            .unwrap_err();
        assert!(
            !format!("{error} {error:?}").contains("postgres://db/shop_qa"),
            "{error:?}"
        );
    }

    // TC: CRE-08 (CI-only keys are write-only: whoever may read the
    // project scope, a tenant's other members included, never reads the
    // deploy token or the registry login back)
    #[tokio::test]
    async fn forbidden_without_value_when_key_is_for_ci_only() {
        let world = World::new().await;
        let secrets = world.secrets();
        let mut answers = Vec::new();
        for name in [DEPLOY_TOKEN_KEY, REGISTRY_USER_KEY, REGISTRY_TOKEN_KEY] {
            let _set = secrets
                .set(
                    &world.owner,
                    &shop(),
                    key(name),
                    value("ci-only-value"),
                    true,
                )
                .await
                .unwrap();
            let got = secrets.get(&world.owner, &shop(), &key(name)).await;
            answers.push(match got {
                Err(Error::Forbidden(why)) => !why.contains("ci-only-value"),
                _ => false,
            });
        }
        assert_eq!(answers, vec![true; 3]);
    }
}

mod list {
    use super::*;

    // TC: SEC-04
    #[tokio::test]
    async fn lists_keys_by_key_with_versions_when_scope_has_secrets() {
        let world = World::new().await;
        let listed = world.secrets().list(&world.owner, &shop()).await.unwrap();
        let shown: Vec<(String, ScopeKind, u32, bool)> = listed
            .iter()
            .map(|e| (e.key.to_string(), e.scope, e.version.get(), e.ci))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("DATABASE_URL".to_owned(), ScopeKind::Project, 1, false),
                ("LOG_LEVEL".to_owned(), ScopeKind::Project, 1, false),
            ]
        );
    }

    // TC: SEC-04
    #[tokio::test]
    async fn carries_no_values_when_listed() {
        let world = World::new().await;
        let listed = world.secrets().list(&world.owner, &shop()).await.unwrap();
        let shown = format!("{listed:?}");
        let sealed = world.ports.secrets.encrypt("postgres://db/shop").unwrap();
        assert!(
            !shown.contains("postgres://db/shop")
                && !shown.contains("info")
                && !shown.contains(sealed.as_str()),
            "{shown}"
        );
    }

    // TC: SEC-04
    #[tokio::test]
    async fn lists_only_environment_scope_when_scope_is_environment() {
        let world = World::new().await;
        let listed = world.secrets().list(&world.owner, &qa()).await.unwrap();
        let shown: Vec<(String, ScopeKind)> = listed
            .iter()
            .map(|e| (e.key.to_string(), e.scope))
            .collect();
        assert_eq!(
            shown,
            vec![("DATABASE_URL".to_owned(), ScopeKind::Environment)]
        );
    }

    // TC: SEC-04
    #[tokio::test]
    async fn shows_ci_flag_when_set_with_ci() {
        let world = World::new().await;
        let secrets = world.secrets();
        secrets
            .set(
                &world.owner,
                &shop(),
                key("REGISTRY_TOKEN"),
                value("r-1"),
                true,
            )
            .await
            .unwrap();
        let listed = secrets.list(&world.owner, &shop()).await.unwrap();
        let flags: Vec<(String, bool)> = listed.iter().map(|e| (e.key.to_string(), e.ci)).collect();
        assert_eq!(
            flags,
            vec![
                ("DATABASE_URL".to_owned(), false),
                ("LOG_LEVEL".to_owned(), false),
                ("REGISTRY_TOKEN".to_owned(), true),
            ]
        );
    }

    // TC: SEC-04
    #[tokio::test]
    async fn not_found_when_environment_unknown() {
        let world = World::new().await;
        let error = world
            .secrets()
            .list(
                &world.owner,
                &ScopeRef::Environment(project("shop"), env("staging")),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }
    // TC: SEC-10 (service half: a foreign scope's inventory is off limits)
    #[tokio::test]
    async fn forbidden_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let error = world.secrets().list(&guest, &qa()).await.unwrap_err();
        assert!(matches!(error, Error::Forbidden(_)), "{error:?}");
    }
}

mod remove {
    use super::*;

    // TC: SEC-04 (`remove`: the key leaves `ls`)
    #[tokio::test]
    async fn drops_key_from_list_when_removed() {
        let world = World::new().await;
        let secrets = world.secrets();
        secrets
            .remove(&world.owner, &shop(), &key("LOG_LEVEL"))
            .await
            .unwrap();
        let keys: Vec<String> = secrets
            .list(&world.owner, &shop())
            .await
            .unwrap()
            .iter()
            .map(|e| e.key.to_string())
            .collect();
        assert_eq!(keys, vec!["DATABASE_URL".to_owned()]);
    }

    // TC: SEC-02 (`remove`: the history goes with the key)
    #[tokio::test]
    async fn deletes_versions_when_removed() {
        let world = World::new().await;
        let id = world.qa_secrets[0].id;
        world
            .secrets()
            .remove(&world.owner, &qa(), &key("DATABASE_URL"))
            .await
            .unwrap();
        assert_eq!(world.ports.store.secret_versions(id).await.unwrap(), vec![]);
    }

    // TC: SEC-06 (removing the override lets the project value through)
    #[tokio::test]
    async fn resolves_project_value_when_environment_override_removed() {
        let world = World::new().await;
        let secrets = world.secrets();
        secrets
            .remove(&world.owner, &qa(), &key("DATABASE_URL"))
            .await
            .unwrap();
        let resolved = secrets
            .resolve(&world.owner, &project("shop"), &env("qa"), None)
            .await
            .unwrap();
        assert_eq!(
            pairs(&resolved).first(),
            Some(&("DATABASE_URL".to_owned(), "postgres://db/shop".to_owned()))
        );
    }

    // TC: SEC-04
    #[tokio::test]
    async fn not_found_when_key_unknown() {
        let world = World::new().await;
        let error = world
            .secrets()
            .remove(&world.owner, &qa(), &key("LOG_LEVEL"))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    // TC: SEC-10 (service half: only the scope owner deletes)
    #[tokio::test]
    async fn forbidden_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let error = world
            .secrets()
            .remove(&guest, &qa(), &key("DATABASE_URL"))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Forbidden(_)), "{error:?}");
    }

    // TC: SEC-10 (service half: a refused delete keeps the secret)
    #[tokio::test]
    async fn keeps_secret_when_forbidden() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let _refused = world
            .secrets()
            .remove(&guest, &qa(), &key("DATABASE_URL"))
            .await;
        assert!(stored_qa(&world, "DATABASE_URL").await.is_some());
    }

    // TC: SEC-08 (the audit entry goes first: a failed audit deletes nothing)
    #[tokio::test]
    async fn keeps_secret_when_audit_fails() {
        let world = World::new().await;
        world
            .ports
            .store
            .fail_next("insert_audit", Error::internal_msg("disk full"));
        let result = world
            .secrets()
            .remove(&world.owner, &qa(), &key("DATABASE_URL"))
            .await;
        assert_eq!(
            (
                result.is_err(),
                stored_qa(&world, "DATABASE_URL").await.is_some()
            ),
            (true, true)
        );
    }
}

mod history {
    use super::*;

    // TC: SEC-02
    #[tokio::test]
    async fn lists_versions_newest_first_when_set_twice() {
        let world = World::new().await;
        let secrets = world.secrets();
        secrets
            .set(
                &world.owner,
                &qa(),
                key("DATABASE_URL"),
                value("postgres://db/shop_qa2"),
                false,
            )
            .await
            .unwrap();
        let history = secrets
            .history(&world.owner, &qa(), &key("DATABASE_URL"))
            .await
            .unwrap();
        let numbers: Vec<u32> = history.iter().map(|r| r.version.get()).collect();
        assert_eq!(numbers, vec![2, 1]);
    }

    // TC: SEC-02
    #[tokio::test]
    async fn names_writer_and_time_when_listed() {
        let world = World::new().await;
        let history = world
            .secrets()
            .history(&world.owner, &qa(), &key("DATABASE_URL"))
            .await
            .unwrap();
        assert_eq!(
            history,
            vec![SecretRevision {
                version: version(1),
                updated_by: UpdatedBy::User(world.owner.id),
                updated_at: world.ports.clock.now(),
            }]
        );
    }

    // TC: SEC-03
    #[tokio::test]
    async fn shows_20_versions_when_set_25_times() {
        let world = World::new().await;
        let secrets = world.secrets();
        for n in 1..=25 {
            secrets
                .set(
                    &world.owner,
                    &qa(),
                    key("API_KEY"),
                    value(&format!("k-{n}")),
                    false,
                )
                .await
                .unwrap();
        }
        let history = secrets
            .history(&world.owner, &qa(), &key("API_KEY"))
            .await
            .unwrap();
        let ends = (
            history.len(),
            history.first().map(|r| r.version.get()),
            history.last().map(|r| r.version.get()),
        );
        assert_eq!(ends, (20, Some(25), Some(6)));
    }

    // TC: SEC-04
    #[tokio::test]
    async fn not_found_when_key_unknown() {
        let world = World::new().await;
        let error = world
            .secrets()
            .history(&world.owner, &qa(), &key("API_KEY"))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }
    // TC: SEC-10 (service half: a foreign scope's history is off limits)
    #[tokio::test]
    async fn forbidden_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let error = world
            .secrets()
            .history(&guest, &qa(), &key("DATABASE_URL"))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Forbidden(_)), "{error:?}");
    }
}

mod rollback {
    use super::*;

    /// `API_KEY` of `qa` at versions 1 (`k-1`) and 2 (`k-2`).
    async fn two_versions(world: &World) {
        let secrets = world.secrets();
        for raw in ["k-1", "k-2"] {
            secrets
                .set(&world.owner, &qa(), key("API_KEY"), value(raw), false)
                .await
                .unwrap();
        }
    }

    // TC: SEC-05
    #[tokio::test]
    async fn creates_version_3_when_rolled_back_to_1() {
        let world = World::new().await;
        two_versions(&world).await;
        let entry = world
            .secrets()
            .rollback(&world.owner, &qa(), &key("API_KEY"), version(1))
            .await
            .unwrap();
        assert_eq!(entry.version, version(3));
    }

    // TC: SEC-05
    #[tokio::test]
    async fn serves_value_of_version_1_when_rolled_back_to_1() {
        let world = World::new().await;
        two_versions(&world).await;
        let secrets = world.secrets();
        secrets
            .rollback(&world.owner, &qa(), &key("API_KEY"), version(1))
            .await
            .unwrap();
        let got = secrets
            .get(&world.owner, &qa(), &key("API_KEY"))
            .await
            .unwrap();
        assert_eq!(got.expose(), "k-1");
    }

    // TC: SEC-05
    #[tokio::test]
    async fn keeps_versions_1_and_2_when_rolled_back() {
        let world = World::new().await;
        two_versions(&world).await;
        world
            .secrets()
            .rollback(&world.owner, &qa(), &key("API_KEY"), version(1))
            .await
            .unwrap();
        let stored = stored_qa(&world, "API_KEY").await.unwrap();
        assert_eq!(
            opened_versions(&world, &stored).await,
            vec![
                (3, "k-1".to_owned()),
                (2, "k-2".to_owned()),
                (1, "k-1".to_owned())
            ]
        );
    }

    // TC: SEC-05
    #[tokio::test]
    async fn not_found_when_version_never_existed() {
        let world = World::new().await;
        two_versions(&world).await;
        let error = world
            .secrets()
            .rollback(&world.owner, &qa(), &key("API_KEY"), version(7))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    // TC: SEC-05, SEC-03
    #[tokio::test]
    async fn not_found_when_version_pruned() {
        let world = World::new().await;
        let secrets = world.secrets();
        for n in 1..=21 {
            secrets
                .set(
                    &world.owner,
                    &qa(),
                    key("API_KEY"),
                    value(&format!("k-{n}")),
                    false,
                )
                .await
                .unwrap();
        }
        let error = secrets
            .rollback(&world.owner, &qa(), &key("API_KEY"), version(1))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    // TC: SEC-05
    #[tokio::test]
    async fn not_found_when_key_unknown() {
        let world = World::new().await;
        let error = world
            .secrets()
            .rollback(&world.owner, &qa(), &key("API_KEY"), version(1))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    // TC: SEC-10 (service half: only the scope owner rolls back)
    #[tokio::test]
    async fn forbidden_when_actor_is_not_admin() {
        let world = World::new().await;
        two_versions(&world).await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let error = world
            .secrets()
            .rollback(&guest, &qa(), &key("API_KEY"), version(1))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Forbidden(_)), "{error:?}");
    }

    // TC: SEC-10 (service half: a refused rollback writes no version)
    #[tokio::test]
    async fn keeps_version_2_when_forbidden() {
        let world = World::new().await;
        two_versions(&world).await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let _refused = world
            .secrets()
            .rollback(&guest, &qa(), &key("API_KEY"), version(1))
            .await;
        let current = stored_qa(&world, "API_KEY").await.unwrap();
        assert_eq!(current.version, version(2));
    }

    // TC: SEC-08 (the audit entry goes first: a failed audit writes nothing)
    #[tokio::test]
    async fn keeps_version_2_when_audit_fails() {
        let world = World::new().await;
        two_versions(&world).await;
        world
            .ports
            .store
            .fail_next("insert_audit", Error::internal_msg("disk full"));
        let result = world
            .secrets()
            .rollback(&world.owner, &qa(), &key("API_KEY"), version(1))
            .await;
        let current = stored_qa(&world, "API_KEY").await.unwrap();
        assert_eq!((result.is_err(), current.version), (true, version(2)));
    }
}

mod resolve {
    use rstest::rstest;

    use super::*;

    // TC: SEC-06
    #[tokio::test]
    async fn environment_overrides_project_with_system_vars_when_shop_qa() {
        let world = World::new().await;
        let resolved = world
            .secrets()
            .resolve(
                &world.owner,
                &project("shop"),
                &env("qa"),
                Some(&tag("sha-1")),
            )
            .await
            .unwrap();
        assert_eq!(
            pairs(&resolved),
            owned(&[
                ("DATABASE_URL", "postgres://db/shop_qa"),
                ("LOG_LEVEL", "info"),
                ("LIFTOFF_PROJECT", "shop"),
                ("LIFTOFF_ENV", "qa"),
                ("LIFTOFF_DOMAIN", ""),
                ("IMAGE_TAG", "sha-1"),
            ])
        );
    }

    // TC: SEC-06
    #[tokio::test]
    async fn uses_project_values_and_bound_domain_when_shop_prod() {
        let world = World::new().await;
        let resolved = world
            .secrets()
            .resolve(
                &world.owner,
                &project("shop"),
                &env("prod"),
                Some(&tag("sha-2")),
            )
            .await
            .unwrap();
        assert_eq!(
            pairs(&resolved),
            owned(&[
                ("DATABASE_URL", "postgres://db/shop"),
                ("LOG_LEVEL", "info"),
                ("LIFTOFF_PROJECT", "shop"),
                ("LIFTOFF_ENV", "prod"),
                ("LIFTOFF_DOMAIN", "shop.apps.test"),
                ("IMAGE_TAG", "sha-2"),
            ])
        );
    }

    // TC: SEC-06
    #[tokio::test]
    async fn omits_image_tag_when_no_tag_given() {
        let world = World::new().await;
        let resolved = world
            .secrets()
            .resolve(&world.owner, &project("shop"), &env("qa"), None)
            .await
            .unwrap();
        assert_eq!(
            pairs(&resolved),
            owned(&[
                ("DATABASE_URL", "postgres://db/shop_qa"),
                ("LOG_LEVEL", "info"),
                ("LIFTOFF_PROJECT", "shop"),
                ("LIFTOFF_ENV", "qa"),
                ("LIFTOFF_DOMAIN", ""),
            ])
        );
    }

    // TC: SEC-06 (the first binding alphabetically)
    #[tokio::test]
    async fn picks_first_bound_host_alphabetically_when_bound_twice() {
        let world = World::new().await;
        let store = &world.ports.store;
        let zeta = world
            .add_domain(&world.shop, "zeta.apps.test")
            .await
            .unwrap();
        let alpha = world
            .add_domain(&world.shop, "alpha.apps.test")
            .await
            .unwrap();
        for (domain, port) in [(&zeta, 20_001), (&alpha, 20_002)] {
            let binding = world
                .records
                .binding(&world.qa, domain, "web", 3000, port)
                .unwrap();
            store.insert_binding(&binding).await.unwrap();
        }
        let resolved = world
            .secrets()
            .resolve(&world.owner, &project("shop"), &env("qa"), None)
            .await
            .unwrap();
        assert!(
            pairs(&resolved).contains(&("LIFTOFF_DOMAIN".to_owned(), "alpha.apps.test".to_owned())),
            "{:?}",
            pairs(&resolved)
        );
    }

    // TC: SEC-06
    #[tokio::test]
    async fn equal_when_resolved_twice() {
        let world = World::new().await;
        let secrets = world.secrets();
        let first = secrets
            .resolve(
                &world.owner,
                &project("shop"),
                &env("qa"),
                Some(&tag("sha-1")),
            )
            .await
            .unwrap();
        let second = secrets
            .resolve(
                &world.owner,
                &project("shop"),
                &env("qa"),
                Some(&tag("sha-1")),
            )
            .await
            .unwrap();
        assert_eq!(first, second);
    }

    // TC: SEC-06
    #[tokio::test]
    async fn not_found_when_environment_unknown() {
        let world = World::new().await;
        let error = world
            .secrets()
            .resolve(&world.owner, &project("shop"), &env("staging"), None)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    /// Stores `vars` as version-1 secrets of `scope`, in the order given.
    async fn store_all(world: &World, scope: SecretScope, vars: &[(&str, &str)]) {
        for (raw_key, raw_value) in vars {
            let secret = world
                .records
                .secret(
                    scope,
                    raw_key,
                    world.ports.secrets.encrypt(raw_value).unwrap(),
                    &world.owner,
                )
                .unwrap();
            world.ports.store.put_secret(&secret).await.unwrap();
        }
    }

    /// Project `blog` with the unbound environment `dev`.
    async fn blog(world: &World) -> Project {
        let blog = world.add_project("blog", "owner/blog").await.unwrap();
        world.add_environment(&blog, "dev").await.unwrap();
        blog
    }

    // TC: SEC-06 (table-driven merge)
    #[rstest]
    #[case::nothing_stored(
        &[],
        &[],
        &[("LIFTOFF_PROJECT", "blog"), ("LIFTOFF_ENV", "dev"), ("LIFTOFF_DOMAIN", ""), ("IMAGE_TAG", "t1")],
    )]
    #[case::project_only(
        &[("B", "pb"), ("A", "pa")],
        &[],
        &[("A", "pa"), ("B", "pb"), ("LIFTOFF_PROJECT", "blog"), ("LIFTOFF_ENV", "dev"), ("LIFTOFF_DOMAIN", ""), ("IMAGE_TAG", "t1")],
    )]
    #[case::environment_only(
        &[],
        &[("TOKEN", "et")],
        &[("TOKEN", "et"), ("LIFTOFF_PROJECT", "blog"), ("LIFTOFF_ENV", "dev"), ("LIFTOFF_DOMAIN", ""), ("IMAGE_TAG", "t1")],
    )]
    #[case::environment_wins(
        &[("DB", "project-db"), ("MODE", "p")],
        &[("DB", "env-db")],
        &[("DB", "env-db"), ("MODE", "p"), ("LIFTOFF_PROJECT", "blog"), ("LIFTOFF_ENV", "dev"), ("LIFTOFF_DOMAIN", ""), ("IMAGE_TAG", "t1")],
    )]
    #[case::sorted_across_levels(
        &[("ZED", "pz"), ("MID", "pm")],
        &[("ALPHA", "ea"), ("YAK", "ey")],
        &[("ALPHA", "ea"), ("MID", "pm"), ("YAK", "ey"), ("ZED", "pz"), ("LIFTOFF_PROJECT", "blog"), ("LIFTOFF_ENV", "dev"), ("LIFTOFF_DOMAIN", ""), ("IMAGE_TAG", "t1")],
    )]
    #[case::system_wins_over_environment(
        &[],
        &[("LIFTOFF_ENV", "spoofed"), ("APP", "a")],
        &[("APP", "a"), ("LIFTOFF_PROJECT", "blog"), ("LIFTOFF_ENV", "dev"), ("LIFTOFF_DOMAIN", ""), ("IMAGE_TAG", "t1")],
    )]
    #[case::system_wins_over_project(
        &[("IMAGE_TAG", "latest"), ("LIFTOFF_DOMAIN", "evil.test")],
        &[],
        &[("LIFTOFF_PROJECT", "blog"), ("LIFTOFF_ENV", "dev"), ("LIFTOFF_DOMAIN", ""), ("IMAGE_TAG", "t1")],
    )]
    #[tokio::test]
    async fn merges_in_deterministic_order_when_table_case(
        #[case] project_vars: &[(&str, &str)],
        #[case] env_vars: &[(&str, &str)],
        #[case] expected: &[(&str, &str)],
    ) {
        let world = World::new().await;
        let blog = blog(&world).await;
        let dev = world
            .ports
            .store
            .environment_by_name(blog.id, &env("dev"))
            .await
            .unwrap()
            .unwrap();
        store_all(&world, SecretScope::Project(blog.id), project_vars).await;
        store_all(&world, SecretScope::Environment(dev.id), env_vars).await;
        let resolved = world
            .secrets()
            .resolve(
                &world.owner,
                &project("blog"),
                &env("dev"),
                Some(&tag("t1")),
            )
            .await
            .unwrap();
        assert_eq!(pairs(&resolved), owned(expected));
    }

    // TC: SEC-10 (service half: resolve hands out every value, so only the
    // scope owner may call it)
    #[tokio::test]
    async fn forbidden_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let error = world
            .secrets()
            .resolve(&guest, &project("shop"), &env("qa"), None)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Forbidden(_)), "{error:?}");
    }

    // TC: SEC-10 (service half: refused before anything is opened)
    #[tokio::test]
    async fn forbidden_before_decrypting_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        world
            .ports
            .secrets
            .fail_always("decrypt", Error::internal_msg("decrypt called"));
        let error = world
            .secrets()
            .resolve(&guest, &project("shop"), &env("qa"), None)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Forbidden(_)), "{error:?}");
    }
}

mod at_rest {
    use super::*;

    // TC: SEC-07
    #[tokio::test]
    async fn stores_no_plaintext_in_current_row_when_set() {
        let world = World::new().await;
        world
            .secrets()
            .set(
                &world.owner,
                &qa(),
                key("API_KEY"),
                value("hunter2-plaintext"),
                false,
            )
            .await
            .unwrap();
        let stored = stored_qa(&world, "API_KEY").await.unwrap();
        assert!(
            !stored.value.as_str().contains("hunter2"),
            "{}",
            stored.value.as_str()
        );
    }

    // TC: SEC-07
    #[tokio::test]
    async fn seals_with_secrets_port_when_set() {
        let world = World::new().await;
        world
            .secrets()
            .set(
                &world.owner,
                &qa(),
                key("API_KEY"),
                value("hunter2-plaintext"),
                false,
            )
            .await
            .unwrap();
        let stored = stored_qa(&world, "API_KEY").await.unwrap();
        assert_eq!(
            world.ports.secrets.decrypt(&stored.value).unwrap(),
            "hunter2-plaintext"
        );
    }

    // TC: SEC-07
    #[tokio::test]
    async fn stores_no_plaintext_in_history_when_set_twice() {
        let world = World::new().await;
        let secrets = world.secrets();
        for raw in ["hunter2-first", "hunter2-second"] {
            secrets
                .set(&world.owner, &qa(), key("API_KEY"), value(raw), false)
                .await
                .unwrap();
        }
        let stored = stored_qa(&world, "API_KEY").await.unwrap();
        let leaked: Vec<u32> = versions(&world, &stored)
            .await
            .iter()
            .filter(|v| v.value.as_str().contains("hunter2"))
            .map(|v| v.version.get())
            .collect();
        assert_eq!(leaked, Vec::<u32>::new());
    }

    // TC: SEC-07
    #[tokio::test]
    async fn stores_no_plaintext_when_rolled_back() {
        let world = World::new().await;
        let secrets = world.secrets();
        for raw in ["hunter2-first", "hunter2-second"] {
            secrets
                .set(&world.owner, &qa(), key("API_KEY"), value(raw), false)
                .await
                .unwrap();
        }
        secrets
            .rollback(&world.owner, &qa(), &key("API_KEY"), version(1))
            .await
            .unwrap();
        let stored = stored_qa(&world, "API_KEY").await.unwrap();
        assert!(
            !stored.value.as_str().contains("hunter2"),
            "{}",
            stored.value.as_str()
        );
    }

    // TC: SEC-07
    #[tokio::test]
    async fn stores_nothing_when_sealing_fails() {
        let world = World::new().await;
        world
            .ports
            .secrets
            .fail_next("encrypt", Error::internal_msg("no randomness for a nonce"));
        let result = world
            .secrets()
            .set(&world.owner, &qa(), key("API_KEY"), value("x"), false)
            .await;
        assert_eq!(
            (result.is_err(), stored_qa(&world, "API_KEY").await),
            (true, None)
        );
    }
}

mod audit_log {
    use super::*;

    /// Every audit entry's debug form, joined.
    async fn shown(world: &World) -> String {
        format!("{:?}", world.ports.store.audit_log().await.unwrap())
    }

    // TC: SEC-08
    #[tokio::test]
    async fn records_set_when_set() {
        let world = World::new().await;
        world
            .secrets()
            .set(&world.owner, &qa(), key("API_KEY"), value("hunter2"), false)
            .await
            .unwrap();
        assert_eq!(
            audit(&world).await,
            audited(&world, "secret.set", "environment:shop/qa/API_KEY")
        );
    }

    // TC: SEC-08
    #[tokio::test]
    async fn records_no_value_when_set() {
        let world = World::new().await;
        world
            .secrets()
            .set(&world.owner, &qa(), key("API_KEY"), value("hunter2"), false)
            .await
            .unwrap();
        let sealed = world.ports.secrets.encrypt("hunter2").unwrap();
        let shown = shown(&world).await;
        assert!(
            !shown.contains("hunter2") && !shown.contains(sealed.as_str()),
            "{shown}"
        );
    }

    // TC: SEC-08
    #[tokio::test]
    async fn records_get_without_value_when_got() {
        let world = World::new().await;
        world
            .secrets()
            .get(&world.owner, &qa(), &key("DATABASE_URL"))
            .await
            .unwrap();
        let shown = shown(&world).await;
        assert_eq!(
            (audit(&world).await, shown.contains("postgres://db/shop_qa")),
            (
                audited(&world, "secret.get", "environment:shop/qa/DATABASE_URL"),
                false
            )
        );
    }

    // TC: SEC-08
    #[tokio::test]
    async fn records_list_when_listed() {
        let world = World::new().await;
        world.secrets().list(&world.owner, &shop()).await.unwrap();
        assert_eq!(
            audit(&world).await,
            audited(&world, "secret.list", "project:shop")
        );
    }

    // TC: SEC-08
    #[tokio::test]
    async fn records_remove_when_removed() {
        let world = World::new().await;
        world
            .secrets()
            .remove(&world.owner, &shop(), &key("LOG_LEVEL"))
            .await
            .unwrap();
        assert_eq!(
            audit(&world).await,
            audited(&world, "secret.remove", "project:shop/LOG_LEVEL")
        );
    }

    // TC: SEC-08
    #[tokio::test]
    async fn records_history_when_listed() {
        let world = World::new().await;
        world
            .secrets()
            .history(&world.owner, &shop(), &key("LOG_LEVEL"))
            .await
            .unwrap();
        assert_eq!(
            audit(&world).await,
            audited(&world, "secret.history", "project:shop/LOG_LEVEL")
        );
    }

    // TC: SEC-08
    #[tokio::test]
    async fn records_rollback_without_value_when_rolled_back() {
        let world = World::new().await;
        let secrets = world.secrets();
        secrets
            .set(
                &world.owner,
                &shop(),
                key("LOG_LEVEL"),
                value("debug"),
                false,
            )
            .await
            .unwrap();
        secrets
            .rollback(&world.owner, &shop(), &key("LOG_LEVEL"), version(1))
            .await
            .unwrap();
        let shown = shown(&world).await;
        let entries = audit(&world).await;
        assert_eq!(
            (
                entries.last().cloned(),
                shown.contains("debug"),
                shown.contains("info")
            ),
            (
                Some((
                    Some(world.owner.id),
                    "secret.rollback".to_owned(),
                    "project:shop/LOG_LEVEL".to_owned()
                )),
                false,
                false
            )
        );
    }

    // TC: SEC-08
    #[tokio::test]
    async fn records_resolve_without_values_when_resolved() {
        let world = World::new().await;
        world
            .secrets()
            .resolve(&world.owner, &project("shop"), &env("qa"), None)
            .await
            .unwrap();
        let shown = shown(&world).await;
        assert_eq!(
            (audit(&world).await, shown.contains("postgres://db/shop")),
            (
                audited(&world, "secret.resolve", "environment:shop/qa"),
                false
            )
        );
    }

    // TC: SEC-08
    #[tokio::test]
    async fn records_one_entry_per_operation_in_order_when_several_run() {
        let world = World::new().await;
        let secrets = world.secrets();
        secrets
            .set(&world.owner, &shop(), key("API_KEY"), value("k-1"), true)
            .await
            .unwrap();
        secrets
            .get(&world.owner, &shop(), &key("API_KEY"))
            .await
            .unwrap();
        secrets
            .remove(&world.owner, &shop(), &key("API_KEY"))
            .await
            .unwrap();
        let actions: Vec<String> = audit(&world)
            .await
            .into_iter()
            .map(|(_, action, _)| action)
            .collect();
        assert_eq!(actions, vec!["secret.set", "secret.get", "secret.remove"]);
    }

    // TC: SEC-08, SEC-10 (a refused read is recorded, without the value)
    #[tokio::test]
    async fn records_denied_get_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let _refused = world
            .secrets()
            .get(&guest, &qa(), &key("DATABASE_URL"))
            .await;
        let shown = shown(&world).await;
        assert_eq!(
            (audit(&world).await, shown.contains("postgres://db/shop_qa")),
            (
                vec![(
                    Some(guest.id),
                    "secret.get.denied".to_owned(),
                    "environment:shop/qa/DATABASE_URL".to_owned()
                )],
                false
            )
        );
    }

    // TC: SEC-08, SEC-10
    #[tokio::test]
    async fn records_denied_resolve_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let _refused = world
            .secrets()
            .resolve(&guest, &project("shop"), &env("qa"), None)
            .await;
        assert_eq!(
            audit(&world).await,
            vec![(
                Some(guest.id),
                "secret.resolve.denied".to_owned(),
                "environment:shop/qa".to_owned()
            )]
        );
    }
    // TC: SEC-08, SEC-10
    #[tokio::test]
    async fn records_denied_set_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let _refused = world
            .secrets()
            .set(&guest, &qa(), key("API_KEY"), value("x"), false)
            .await;
        assert_eq!(
            audit(&world).await,
            vec![(
                Some(guest.id),
                "secret.set.denied".to_owned(),
                "environment:shop/qa/API_KEY".to_owned()
            )]
        );
    }

    // TC: SEC-08, SEC-10
    #[tokio::test]
    async fn records_denied_remove_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let _refused = world
            .secrets()
            .remove(&guest, &qa(), &key("DATABASE_URL"))
            .await;
        assert_eq!(
            audit(&world).await,
            vec![(
                Some(guest.id),
                "secret.remove.denied".to_owned(),
                "environment:shop/qa/DATABASE_URL".to_owned()
            )]
        );
    }

    // TC: SEC-08, SEC-10
    #[tokio::test]
    async fn records_denied_rollback_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let _refused = world
            .secrets()
            .rollback(&guest, &qa(), &key("DATABASE_URL"), version(1))
            .await;
        assert_eq!(
            audit(&world).await,
            vec![(
                Some(guest.id),
                "secret.rollback.denied".to_owned(),
                "environment:shop/qa/DATABASE_URL".to_owned()
            )]
        );
    }

    // TC: SEC-08, SEC-10
    #[tokio::test]
    async fn records_denied_list_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let _refused = world.secrets().list(&guest, &qa()).await;
        assert_eq!(
            audit(&world).await,
            vec![(
                Some(guest.id),
                "secret.list.denied".to_owned(),
                "environment:shop/qa".to_owned()
            )]
        );
    }

    // TC: SEC-08, SEC-10
    #[tokio::test]
    async fn records_denied_history_when_actor_is_not_admin() {
        let world = World::new().await;
        let guest = world.add_user("guest", UserRole::User).await.unwrap();
        let _refused = world
            .secrets()
            .history(&guest, &qa(), &key("DATABASE_URL"))
            .await;
        assert_eq!(
            audit(&world).await,
            vec![(
                Some(guest.id),
                "secret.history.denied".to_owned(),
                "environment:shop/qa/DATABASE_URL".to_owned()
            )]
        );
    }
}

mod environment_removal {
    use super::*;

    /// Removes `shop/qa` through the environment service.
    async fn remove_qa(world: &World) {
        world
            .environments()
            .remove(&world.owner, &project("shop"), &env("qa"), true)
            .await
            .unwrap();
    }

    // TC: SEC-09
    #[tokio::test]
    async fn deletes_environment_secrets_when_environment_removed() {
        let world = World::new().await;
        remove_qa(&world).await;
        assert_eq!(
            world
                .ports
                .store
                .secrets_of_scope(&SecretScope::Environment(world.qa.id))
                .await
                .unwrap(),
            vec![]
        );
    }

    // TC: SEC-09
    #[tokio::test]
    async fn deletes_environment_secret_versions_when_environment_removed() {
        let world = World::new().await;
        remove_qa(&world).await;
        assert_eq!(
            world
                .ports
                .store
                .secret_versions(world.qa_secrets[0].id)
                .await
                .unwrap(),
            vec![]
        );
    }

    // TC: SEC-09
    #[tokio::test]
    async fn keeps_project_secrets_when_environment_removed() {
        let world = World::new().await;
        remove_qa(&world).await;
        let keys: Vec<String> = world
            .secrets()
            .list(&world.owner, &shop())
            .await
            .unwrap()
            .iter()
            .map(|e| e.key.to_string())
            .collect();
        assert_eq!(
            keys,
            vec!["DATABASE_URL".to_owned(), "LOG_LEVEL".to_owned()]
        );
    }

    // TC: SEC-09
    #[tokio::test]
    async fn keeps_project_values_when_environment_removed() {
        let world = World::new().await;
        remove_qa(&world).await;
        let got = world
            .secrets()
            .get(&world.owner, &shop(), &key("DATABASE_URL"))
            .await
            .unwrap();
        assert_eq!(got.expose(), "postgres://db/shop");
    }

    // TC: SEC-09
    #[tokio::test]
    async fn not_found_listing_removed_environment_when_removed() {
        let world = World::new().await;
        remove_qa(&world).await;
        let error = world.secrets().list(&world.owner, &qa()).await.unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    // TC: SEC-09
    #[tokio::test]
    async fn keeps_environment_secrets_of_prod_when_qa_removed() {
        let world = World::new().await;
        world
            .secrets()
            .set(
                &world.owner,
                &ScopeRef::Environment(project("shop"), env("prod")),
                key("API_KEY"),
                value("prod-key"),
                false,
            )
            .await
            .unwrap();
        remove_qa(&world).await;
        let got = world
            .secrets()
            .get(
                &world.owner,
                &ScopeRef::Environment(project("shop"), env("prod")),
                &key("API_KEY"),
            )
            .await
            .unwrap();
        assert_eq!(got.expose(), "prod-key");
    }
}

mod deploy_token {
    use super::*;

    // TC: REST-07 (`LIFTOFF_TOKEN` is for CI, never for the containers)
    #[tokio::test]
    async fn left_out_when_resolved() {
        let world = World::new().await;
        let _set = world
            .secrets()
            .set(
                &world.owner,
                &shop(),
                key(DEPLOY_TOKEN_KEY),
                value("lf_0123456789abcdefghijABCDEFGHIJ0123456789"),
                true,
            )
            .await
            .unwrap();
        let resolved = world
            .secrets()
            .resolve(&world.owner, &project("shop"), &env("qa"), None)
            .await
            .unwrap();
        assert!(
            !resolved
                .vars()
                .iter()
                .any(|(name, _)| name.as_str() == DEPLOY_TOKEN_KEY),
            "{:?}",
            resolved.vars()
        );
    }
}

/// The registry login `lof create` stores for a Forgejo compose project
/// is the actor's forge account: CI only, like the deploy token.
mod registry_login {
    use super::*;
    use crate::snippet::{REGISTRY_TOKEN_KEY, REGISTRY_USER_KEY};

    // TC: CRE-08 (the forge token never reaches the containers)
    #[tokio::test]
    async fn left_out_when_resolved() {
        let world = World::new().await;
        for name in [REGISTRY_USER_KEY, REGISTRY_TOKEN_KEY] {
            let _set = world
                .secrets()
                .set(&world.owner, &shop(), key(name), value("forge"), true)
                .await
                .unwrap();
        }
        let resolved = world
            .secrets()
            .resolve(&world.owner, &project("shop"), &env("qa"), None)
            .await
            .unwrap();
        let names: Vec<String> = resolved
            .vars()
            .iter()
            .map(|(name, _)| name.to_string())
            .filter(|name| name.starts_with("REGISTRY_"))
            .collect();
        assert_eq!(names, Vec::<String>::new());
    }
}

/// The tenant scope: `ScopeRef::Tenant(project)`
/// names the tenant `project` is in.
mod tenant_scope {
    use super::*;
    use crate::domain::TenantId;
    use crate::ports::{HookResult, Resource};

    fn acme() -> TenantId {
        TenantId::new(ulid::Ulid::from_parts(7, 7))
    }

    fn of_shop() -> ScopeRef {
        ScopeRef::Tenant(project("shop"))
    }

    /// Puts `shop` into the tenant `acme` (what the `tenants` module
    /// leaves).
    async fn shop_in_acme(world: &World) {
        let mut shop = world.shop.clone();
        shop.tenant_id = Some(acme());
        world.ports.store.update_project(&shop).await.unwrap();
    }

    // TC: TEN-11 (without the module the project has no tenant)
    #[tokio::test]
    async fn module_required_naming_tenants_when_project_has_no_tenant() {
        let world = World::new().await;
        let result = world
            .secrets()
            .set(&world.owner, &of_shop(), key("REGION"), value("eu"), false)
            .await;
        assert!(
            matches!(&result, Err(Error::ModuleRequired(name)) if name == "tenants"),
            "{result:?}"
        );
        assert_eq!(
            world
                .ports
                .store
                .secrets_of_scope(&SecretScope::Tenant(acme()))
                .await
                .unwrap(),
            vec![],
            "nothing stored"
        );
    }

    // TC: TEN-11
    #[tokio::test]
    async fn writes_tenant_of_project_when_scope_is_tenant() {
        let world = World::new().await;
        shop_in_acme(&world).await;
        let entry = world
            .secrets()
            .set(&world.owner, &of_shop(), key("REGION"), value("eu"), false)
            .await
            .unwrap();
        let stored = world
            .ports
            .store
            .secret(&SecretScope::Tenant(acme()), &key("REGION"))
            .await
            .unwrap()
            .map(|secret| world.ports.secrets.decrypt(&secret.value).unwrap());
        assert_eq!(
            (entry.scope, stored),
            (ScopeKind::Tenant, Some("eu".to_owned()))
        );
    }

    // TC: TEN-11 (`authorize` about the tenant scope)
    #[tokio::test]
    async fn asks_authorize_about_tenant_scope_when_set() {
        let world = World::new().await;
        shop_in_acme(&world).await;
        world.log().clear();
        world
            .secrets()
            .set(&world.owner, &of_shop(), key("REGION"), value("eu"), false)
            .await
            .unwrap();
        assert_eq!(
            world.log().authorized(),
            vec![(
                world.owner.id,
                Action::SecretSet,
                Resource::Secret(SecretScope::Tenant(acme()))
            )]
        );
    }

    // TC: TEN-11 (a foreign tenant is refused)
    #[tokio::test]
    async fn forbidden_and_nothing_stored_when_hook_denies_the_tenant() {
        let world = World::new().await;
        shop_in_acme(&world).await;
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Deny("not a member".to_owned()));
        let result = world
            .secrets()
            .set(&world.owner, &of_shop(), key("REGION"), value("eu"), false)
            .await;
        assert!(matches!(result, Err(Error::Forbidden(_))), "{result:?}");
        assert_eq!(
            world
                .ports
                .store
                .secrets_of_scope(&SecretScope::Tenant(acme()))
                .await
                .unwrap(),
            vec![]
        );
    }

    // TC: TEN-11 (the admin is asked about a tenantless project as about
    // the installation, before the 424)
    #[tokio::test]
    async fn forbidden_when_non_admin_names_tenant_of_tenantless_project() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        let result = world.secrets().list(&carol, &of_shop()).await;
        assert!(matches!(result, Err(Error::Forbidden(_))), "{result:?}");
    }

    // TC: TEN-11 (tenant → project → environment)
    #[tokio::test]
    async fn merges_tenant_first_and_lets_project_and_environment_override_when_resolved() {
        let world = World::new().await;
        shop_in_acme(&world).await;
        for (raw_key, raw_value) in [
            ("DATABASE_URL", "postgres://db/acme"),
            ("LOG_LEVEL", "warn"),
            ("REGION", "eu"),
        ] {
            world
                .secrets()
                .set(
                    &world.owner,
                    &of_shop(),
                    key(raw_key),
                    value(raw_value),
                    false,
                )
                .await
                .unwrap();
        }
        let resolved = world
            .secrets()
            .resolve(&world.owner, &project("shop"), &env("qa"), None)
            .await
            .unwrap();
        assert_eq!(
            pairs(&resolved),
            owned(&[
                ("DATABASE_URL", "postgres://db/shop_qa"),
                ("LOG_LEVEL", "info"),
                ("REGION", "eu"),
                ("LIFTOFF_PROJECT", "shop"),
                ("LIFTOFF_ENV", "qa"),
                ("LIFTOFF_DOMAIN", ""),
            ])
        );
    }

    // TC: TEN-11 (every project of the tenant sees it; a tenantless one
    // does not)
    #[tokio::test]
    async fn reaches_other_projects_of_the_tenant_only_when_resolved() {
        let world = World::new().await;
        shop_in_acme(&world).await;
        let mut blog = world.add_project("blog", "owner/blog").await.unwrap();
        blog.tenant_id = Some(acme());
        world.ports.store.update_project(&blog).await.unwrap();
        world.add_environment(&blog, "dev").await.unwrap();
        let news = world.add_project("news", "owner/news").await.unwrap();
        world.add_environment(&news, "dev").await.unwrap();
        world
            .secrets()
            .set(&world.owner, &of_shop(), key("REGION"), value("eu"), false)
            .await
            .unwrap();
        let region = |env: Env| {
            pairs(&env)
                .into_iter()
                .find(|(key, _)| key == "REGION")
                .map(|(_, value)| value)
        };
        let of_blog = world
            .secrets()
            .resolve(&world.owner, &project("blog"), &env("dev"), None)
            .await
            .unwrap();
        let of_news = world
            .secrets()
            .resolve(&world.owner, &project("news"), &env("dev"), None)
            .await
            .unwrap();
        assert_eq!(
            (region(of_blog), region(of_news)),
            (Some("eu".to_owned()), None)
        );
    }

    // TC: TEN-11, SEC-08
    #[tokio::test]
    async fn records_tenant_of_project_when_set() {
        let world = World::new().await;
        shop_in_acme(&world).await;
        world
            .secrets()
            .set(&world.owner, &of_shop(), key("REGION"), value("eu"), false)
            .await
            .unwrap();
        assert_eq!(
            audit(&world).await,
            audited(&world, "secret.set", "tenant-of:shop/REGION")
        );
    }
}

/// The rules [`SecretsService`] shares with local mode (the CLI's store keeps sealed rows, the core decides).
mod rules {
    use super::*;

    // TC: SYNC-04
    #[test]
    fn refuses_ci_when_scope_is_environment() {
        let scope = ScopeRef::Environment(project("shop"), env("prod"));
        assert!(matches!(
            check_ci_scope(&scope, true),
            Err(Error::Validation(_))
        ));
    }

    // TC: SYNC-04
    #[test]
    fn accepts_ci_when_scope_is_project() {
        assert!(check_ci_scope(&ScopeRef::Project(project("shop")), true).is_ok());
    }

    // TC: SEC-06 (tenant, then project, then environment)
    #[test]
    fn lists_widest_scope_first_when_env_scopes_asked() {
        assert_eq!(
            env_scopes(&project("shop"), &env("prod")),
            [
                ScopeRef::Tenant(project("shop")),
                ScopeRef::Project(project("shop")),
                ScopeRef::Environment(project("shop"), env("prod")),
            ]
        );
    }

    // TC: SEC-06 (the lower scope wins)
    #[test]
    fn keeps_later_value_when_layers_share_a_key() {
        let merged = merge_layers([
            (key("DATABASE_URL"), value("project")),
            (key("DATABASE_URL"), value("environment")),
        ])
        .unwrap();
        assert_eq!(
            merged.get(&key("DATABASE_URL")).map(SecretValue::expose),
            Some("environment")
        );
    }

    // TC: SEC-06 (CI-only keys never reach an env)
    #[test]
    fn drops_ci_only_keys_when_merged() {
        let merged = merge_layers([
            (key("LIFTOFF_TOKEN"), value("token")),
            (key("REGISTRY_TOKEN"), value("registry")),
            (key("LOG_LEVEL"), value("info")),
        ])
        .unwrap();
        assert_eq!(
            merged.keys().map(SecretKey::as_str).collect::<Vec<_>>(),
            vec!["LOG_LEVEL"]
        );
    }
}
