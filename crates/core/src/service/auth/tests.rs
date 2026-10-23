//! Tests of [`AuthService`] over the shared [`World`].
//!
//! The world's clock starts at 2026-10-13T00:00:00Z (unix 1 791 849 600);
//! `owner` is the single local admin. Expected instants are written as
//! unix seconds with the date they stand for.

use time::{Duration, OffsetDateTime};

use super::*;
use crate::domain::{SessionKind, Timestamp, UserRole};
use crate::fakes::{Call, HookCall, World};
use crate::ports::AuthResult;

/// 2026-10-13T00:00:00Z + 29 days = 2026-11-11T00:00:00Z.
const NOV_11: i64 = 1_794_355_200;
/// 2026-10-13T00:00:00Z + 30 days = 2026-11-12T00:00:00Z.
const NOV_12: i64 = 1_794_441_600;
/// 2026-11-11T00:00:00Z + 30 days = 2026-12-11T00:00:00Z.
const DEC_11: i64 = 1_796_947_200;

fn at(unix: i64) -> Timestamp {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

/// `GET /api/v1/me` as a hook sees it.
fn request() -> HookRequest {
    HookRequest {
        method: "GET".to_owned(),
        path: "/api/v1/me".to_owned(),
        headers: Vec::new(),
    }
}

fn bearer(secret: &SessionSecret) -> Bearer {
    Bearer::Session(secret.clone())
}

async fn authenticate(world: &World, secret: &SessionSecret) -> Result<Principal, Error> {
    world
        .auth()
        .authenticate(&request(), Some(&bearer(secret)))
        .await
}

async fn stored_session(world: &World, secret: &SessionSecret) -> Option<Session> {
    world
        .ports
        .store
        .session_by_hash(&secret.hash())
        .await
        .unwrap()
}

fn store_writes(world: &World, op: &'static str) -> usize {
    world.log().filter(|call| *call == Call::Store(op)).len()
}

mod exchange {
    use super::*;

    // TC: LOGN-01
    #[tokio::test]
    async fn opens_thirty_day_cli_session_of_owner_when_setup_token_live() {
        let world = World::new().await;
        let setup = world.setup_token(&world.owner).await.unwrap();
        let grant = world.auth().exchange(&setup, "laptop").await.unwrap();
        let session = stored_session(&world, &grant.token).await.unwrap();
        assert_eq!(
            (
                session.user_id,
                session.kind,
                session.name.as_str(),
                session.expires_at,
                session.last_used_at,
            ),
            (
                world.owner.id,
                SessionKind::Cli,
                "laptop",
                at(NOV_12),
                at(1_791_849_600)
            )
        );
    }

    // TC: LOGN-01
    #[tokio::test]
    async fn names_owner_and_hands_out_lfs_token_when_setup_token_live() {
        let world = World::new().await;
        let setup = world.setup_token(&world.owner).await.unwrap();
        let grant = world.auth().exchange(&setup, "laptop").await.unwrap();
        assert_eq!(
            (grant.user, grant.token.expose().starts_with("lfs_")),
            (world.owner.clone(), true)
        );
    }

    // TC: LOGN-01
    #[tokio::test]
    async fn unauthenticated_when_setup_token_exchanged_twice() {
        let world = World::new().await;
        let setup = world.setup_token(&world.owner).await.unwrap();
        world.auth().exchange(&setup, "laptop").await.unwrap();
        let second = world.auth().exchange(&setup, "laptop").await;
        assert!(
            matches!(second, Err(Error::Unauthenticated(_))),
            "a setup token is single-use, got {second:?}"
        );
    }

    // TC: LOGN-01
    #[tokio::test]
    async fn opens_no_second_session_when_setup_token_exchanged_twice() {
        let world = World::new().await;
        let setup = world.setup_token(&world.owner).await.unwrap();
        world.auth().exchange(&setup, "laptop").await.unwrap();
        let _second = world.auth().exchange(&setup, "laptop").await;
        assert_eq!(
            world
                .ports
                .store
                .sessions_of_user(world.owner.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    // TC: LOGN-01
    #[tokio::test]
    async fn unauthenticated_when_setup_token_never_minted() {
        let world = World::new().await;
        let _live = world.setup_token(&world.owner).await.unwrap();
        let (_, stranger) = world.records.setup_token(&world.owner);
        let result = world.auth().exchange(&stranger, "laptop").await;
        assert!(
            matches!(result, Err(Error::Unauthenticated(_))),
            "an unknown setup token opens nothing, got {result:?}"
        );
    }

    // TC: LOGN-01 (a failed guess does not burn the live token)
    #[tokio::test]
    async fn live_token_still_works_when_wrong_one_tried_first() {
        let world = World::new().await;
        let live = world.setup_token(&world.owner).await.unwrap();
        let (_, stranger) = world.records.setup_token(&world.owner);
        let _refused = world.auth().exchange(&stranger, "laptop").await;
        let grant = world.auth().exchange(&live, "laptop").await;
        assert!(grant.is_ok(), "the live token survives, got {grant:?}");
    }
    // TC: LOGN-01 (a failed exchange does not burn the only credential)
    #[tokio::test]
    async fn setup_token_still_live_when_session_write_fails() {
        let world = World::new().await;
        let setup = world.setup_token(&world.owner).await.unwrap();
        world
            .ports
            .store
            .fail_next("exchange_setup_token", Error::internal_msg("disk full"));
        let failed = world.auth().exchange(&setup, "laptop").await;
        let retried = world.auth().exchange(&setup, "laptop").await;
        assert_eq!(
            (failed.is_err(), retried.map(|grant| grant.user.id).ok()),
            (true, Some(world.owner.id)),
            "the token survives a failed session write"
        );
    }
}

mod issue_setup_token {
    use super::*;

    // TC: LOGN-01 (the token `liftoffd setup` prints)
    #[tokio::test]
    async fn lft_token_exchangeable_for_owner_when_issued() {
        let world = World::new().await;
        let setup = world.auth().issue_setup_token(&world.owner).await.unwrap();
        let grant = world.auth().exchange(&setup, "laptop").await.unwrap();
        assert_eq!(
            (setup.expose().starts_with("lft_"), grant.user.id),
            (true, world.owner.id)
        );
    }

    // TC: LOGN-01 (one live token in the settings: a new one replaces it)
    #[tokio::test]
    async fn earlier_token_dead_when_issued_again() {
        let world = World::new().await;
        let first = world.auth().issue_setup_token(&world.owner).await.unwrap();
        let _second = world.auth().issue_setup_token(&world.owner).await.unwrap();
        let result = world.auth().exchange(&first, "laptop").await;
        assert!(
            matches!(result, Err(Error::Unauthenticated(_))),
            "only the newest setup token is live, got {result:?}"
        );
    }
}

mod create_admin_token {
    use super::*;

    // TC: LOGN-02
    #[tokio::test]
    async fn new_single_use_token_opens_session_when_admin_mints() {
        let world = World::new().await;
        let token = world.auth().create_admin_token(&world.owner).await.unwrap();
        let grant = world.auth().exchange(&token, "desktop").await.unwrap();
        let session = stored_session(&world, &grant.token).await.unwrap();
        assert_eq!(
            (
                token.expose().starts_with("lft_"),
                session.user_id,
                session.kind,
                session.name.as_str()
            ),
            (true, world.owner.id, SessionKind::Cli, "desktop")
        );
    }

    // TC: LOGN-02
    #[tokio::test]
    async fn unauthenticated_when_minted_token_exchanged_twice() {
        let world = World::new().await;
        let token = world.auth().create_admin_token(&world.owner).await.unwrap();
        world.auth().exchange(&token, "desktop").await.unwrap();
        let second = world.auth().exchange(&token, "desktop").await;
        assert!(
            matches!(second, Err(Error::Unauthenticated(_))),
            "an admin token is single-use too, got {second:?}"
        );
    }

    // TC: LOGN-02
    #[tokio::test]
    async fn forbidden_and_nothing_stored_when_actor_not_admin() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        world.log().clear();
        let result = world.auth().create_admin_token(&carol).await;
        assert_eq!(
            (
                matches!(result, Err(Error::Forbidden(_))),
                store_writes(&world, "put_setup_token")
            ),
            (true, 0),
            "got {result:?}"
        );
    }

    // TC: LOGN-02 (the machine that minted it stays logged in)
    #[tokio::test]
    async fn existing_session_still_valid_when_admin_mints() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        world.auth().create_admin_token(&world.owner).await.unwrap();
        let principal = authenticate(&world, &secret).await.unwrap();
        assert_eq!(principal.user().id, world.owner.id);
    }
}

mod authenticate {
    use super::*;

    // TC: LOGN-03
    #[tokio::test]
    async fn owner_with_session_when_session_live() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        let principal = authenticate(&world, &secret).await.unwrap();
        assert_eq!(
            (
                principal.user().clone(),
                principal.session().map(|s| s.token_hash)
            ),
            (world.owner.clone(), Some(secret.hash()))
        );
    }

    // TC: LOGN-03
    #[tokio::test]
    async fn stores_expiry_thirty_days_after_use_when_session_used() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        world.ports.clock.advance(Duration::days(29));
        authenticate(&world, &secret).await.unwrap();
        let stored = stored_session(&world, &secret).await.unwrap();
        assert_eq!(
            (stored.expires_at, stored.last_used_at),
            (at(DEC_11), at(NOV_11))
        );
    }

    // TC: LOGN-03
    #[tokio::test]
    async fn returns_prolonged_session_when_session_used() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        world.ports.clock.advance(Duration::days(29));
        let principal = authenticate(&world, &secret).await.unwrap();
        assert_eq!(
            principal.session().map(|s| (s.expires_at, s.last_used_at)),
            Some((at(DEC_11), at(NOV_11)))
        );
    }

    // TC: LOGN-03 (only `cli` sessions are prolonged; a browser one keeps 24 hours)
    #[tokio::test]
    async fn keeps_expiry_when_browser_session_used() {
        let world = World::new().await;
        let (mut session, secret) = world.records.session(&world.owner);
        session.kind = SessionKind::Browser;
        session.expires_at = at(1_791_849_600).saturating_add(Duration::hours(24));
        world.ports.store.insert_session(&session).await.unwrap();
        world.ports.clock.advance(Duration::hours(1));
        authenticate(&world, &secret).await.unwrap();
        let stored = stored_session(&world, &secret).await.unwrap();
        assert_eq!(
            (stored.expires_at, stored.last_used_at),
            (
                session.expires_at,
                at(1_791_849_600).saturating_add(Duration::hours(1))
            )
        );
    }

    // TC: LOGN-03
    #[tokio::test]
    async fn still_valid_past_first_expiry_when_used_in_between() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        world.ports.clock.advance(Duration::days(20));
        authenticate(&world, &secret).await.unwrap();
        world.ports.clock.advance(Duration::days(20));
        let result = authenticate(&world, &secret).await;
        assert!(
            result.is_ok(),
            "40 days in, 20 after the last use: alive, got {result:?}"
        );
    }

    // TC: LOGN-03
    #[tokio::test]
    async fn unauthenticated_when_unused_for_more_than_thirty_days() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        world.ports.clock.advance(Duration::days(31));
        let result = authenticate(&world, &secret).await;
        assert!(
            matches!(result, Err(Error::Unauthenticated(_))),
            "got {result:?}"
        );
    }

    // TC: LOGN-04 (no session → 401)
    #[tokio::test]
    async fn unauthenticated_when_no_bearer() {
        let world = World::new().await;
        let result = world.auth().authenticate(&request(), None).await;
        assert!(
            matches!(result, Err(Error::Unauthenticated(_))),
            "got {result:?}"
        );
    }

    // TC: LOGN-04 (no session → 401)
    #[tokio::test]
    async fn unauthenticated_when_session_unknown() {
        let world = World::new().await;
        let (_, never_stored) = world.records.session(&world.owner);
        let result = authenticate(&world, &never_stored).await;
        assert!(
            matches!(result, Err(Error::Unauthenticated(_))),
            "got {result:?}"
        );
    }

    // TC: LOGN-05
    #[tokio::test]
    async fn hook_actor_without_session_when_bearer_unknown() {
        let world = World::new().await;
        world
            .ports
            .hooks
            .authenticate_next(AuthResult::Actor(world.owner.id));
        let (_, never_stored) = world.records.session(&world.owner);
        let principal = authenticate(&world, &never_stored).await.unwrap();
        assert_eq!(
            (principal.user().clone(), principal.session().cloned()),
            (world.owner.clone(), None)
        );
    }

    // TC: LOGN-05
    #[tokio::test]
    async fn hook_actor_when_no_bearer() {
        let world = World::new().await;
        world
            .ports
            .hooks
            .authenticate_next(AuthResult::Actor(world.owner.id));
        let principal = world.auth().authenticate(&request(), None).await.unwrap();
        assert_eq!(principal.user().id, world.owner.id);
    }

    // TC: LOGN-05
    #[tokio::test]
    async fn session_not_prolonged_when_hook_names_actor() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        world.log().clear();
        world
            .ports
            .hooks
            .authenticate_next(AuthResult::Actor(world.owner.id));
        world.ports.clock.advance(Duration::days(1));
        authenticate(&world, &secret).await.unwrap();
        assert_eq!(
            (
                store_writes(&world, "update_session"),
                stored_session(&world, &secret).await.map(|s| s.expires_at)
            ),
            (0, Some(at(NOV_12)))
        );
    }

    // TC: LOGN-05
    #[tokio::test]
    async fn asks_hook_with_request_before_session_when_hook_continues() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        world.log().clear();
        authenticate(&world, &secret).await.unwrap();
        assert_eq!(
            world.log().calls(),
            vec![
                Call::Hook(HookCall::Authenticate("/api/v1/me".to_owned())),
                Call::Store("update_session"),
            ]
        );
    }

    // TC: LOGN-05
    #[tokio::test]
    async fn unauthenticated_and_session_untouched_when_hook_denies() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        world.log().clear();
        world
            .ports
            .hooks
            .authenticate_next(AuthResult::Deny("blocked".to_owned()));
        let result = authenticate(&world, &secret).await;
        assert_eq!(
            (
                matches!(result, Err(Error::Unauthenticated(_))),
                store_writes(&world, "update_session")
            ),
            (true, 0),
            "got {result:?}"
        );
    }
}

mod logout {
    use super::*;

    // TC: LOGN-04
    #[tokio::test]
    async fn unauthenticated_when_session_used_after_logout() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        let principal = authenticate(&world, &secret).await.unwrap();
        world.auth().logout(&principal).await.unwrap();
        let result = authenticate(&world, &secret).await;
        assert!(
            matches!(result, Err(Error::Unauthenticated(_))),
            "got {result:?}"
        );
    }

    // TC: LOGN-04
    #[tokio::test]
    async fn deletes_session_row_when_logged_out() {
        let world = World::new().await;
        let secret = world.login(&world.owner).await.unwrap();
        let principal = authenticate(&world, &secret).await.unwrap();
        world.auth().logout(&principal).await.unwrap();
        assert_eq!(stored_session(&world, &secret).await, None);
    }

    // TC: LOGN-04
    #[tokio::test]
    async fn other_session_still_valid_when_one_logged_out() {
        let world = World::new().await;
        let laptop = world.login(&world.owner).await.unwrap();
        let desktop = world.login(&world.owner).await.unwrap();
        let principal = authenticate(&world, &laptop).await.unwrap();
        world.auth().logout(&principal).await.unwrap();
        let result = authenticate(&world, &desktop).await;
        assert!(result.is_ok(), "got {result:?}");
    }
}

mod revoke_sessions {
    use super::*;

    // TC: REST-09 (`lof admin sessions revoke <login>`)
    #[tokio::test]
    async fn every_session_of_the_user_gone_when_admin_revokes() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        let _first = world.login(&carol).await.unwrap();
        let _second = world.login(&carol).await.unwrap();
        let revoked = world
            .auth()
            .revoke_sessions(&world.owner, &carol.login)
            .await
            .unwrap();
        let left = world.ports.store.sessions_of_user(carol.id).await.unwrap();
        assert_eq!((revoked, left), (2, vec![]));
    }

    // TC: REST-09
    #[tokio::test]
    async fn forbidden_when_not_admin() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        let error = world
            .auth()
            .revoke_sessions(&carol, &world.owner.login)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Forbidden(_)), "{error:?}");
    }
}

mod bootstrap_admin {
    use super::*;
    use crate::fakes::FakePorts;

    // TC: SETUP-02 (one `local` admin in single-user mode)
    #[tokio::test]
    async fn creates_one_local_admin_when_the_store_has_no_user() {
        let ports = FakePorts::new();
        let (admin, _token) = bootstrap_admin(&ports.store, &ports.clock, &ports.rng)
            .await
            .unwrap();
        let users = ports.store.users().await.unwrap();
        assert_eq!(
            (
                users,
                admin.login.as_str(),
                admin.role,
                admin.source.clone()
            ),
            (
                vec![admin.clone()],
                "admin",
                UserRole::Admin,
                UserSource::Local
            )
        );
    }

    // TC: SETUP-02 (the printed token is the live one)
    #[tokio::test]
    async fn makes_the_returned_token_live_when_bootstrapped() {
        let ports = FakePorts::new();
        let (admin, token) = bootstrap_admin(&ports.store, &ports.clock, &ports.rng)
            .await
            .unwrap();
        let live = ports.store.setup_token(&token.hash()).await.unwrap();
        assert_eq!(live.map(|live| live.user_id), Some(admin.id));
    }

    // TC: SETUP-04 (`--force` keeps the admin and replaces its token)
    #[tokio::test]
    async fn keeps_the_admin_and_replaces_the_token_when_bootstrapped_twice() {
        let ports = FakePorts::new();
        let (first, old) = bootstrap_admin(&ports.store, &ports.clock, &ports.rng)
            .await
            .unwrap();
        let (second, _new) = bootstrap_admin(&ports.store, &ports.clock, &ports.rng)
            .await
            .unwrap();
        let old_live = ports.store.setup_token(&old.hash()).await.unwrap();
        assert_eq!(
            (
                first.id == second.id,
                ports.store.users().await.unwrap().len(),
                old_live
            ),
            (true, 1, None)
        );
    }

    // TC: SETUP-04 (an existing `owner` is the admin; none is added)
    #[tokio::test]
    async fn takes_the_existing_admin_when_the_store_has_one() {
        let world = World::new().await;
        let before = world.ports.store.users().await.unwrap().len();
        let (admin, _token) =
            bootstrap_admin(&world.ports.store, &world.ports.clock, &world.ports.rng)
                .await
                .unwrap();
        assert_eq!(
            (admin.id, world.ports.store.users().await.unwrap().len()),
            (world.owner.id, before)
        );
    }
}

mod open_session {
    use super::*;
    use crate::domain::{OidcIssuer, UserSource};

    fn issuer() -> OidcIssuer {
        OidcIssuer::new("https://id.example.com").unwrap()
    }

    fn bob(email: &str, role: UserRole) -> ProviderLogin {
        ProviderLogin {
            issuer: issuer(),
            subject: "z1".to_owned(),
            login: Login::new("bob").unwrap(),
            email: email.to_owned(),
            role,
        }
    }

    async fn open(world: &World, login: ProviderLogin, kind: SessionKind) -> LoginGrant {
        world
            .auth()
            .open_session(login, "laptop", kind)
            .await
            .unwrap()
    }

    // TC: OIDC-01 (a first login creates the user of the provider)
    #[tokio::test]
    async fn creates_user_of_provider_when_subject_is_new() {
        let world = World::new().await;
        let grant = open(
            &world,
            bob("bob@example.com", UserRole::User),
            SessionKind::Cli,
        )
        .await;
        assert_eq!(
            (
                grant.user.login.as_str(),
                grant.user.source.clone(),
                grant.user.subject.as_deref(),
                grant.user.role
            ),
            (
                "bob",
                UserSource::Oidc(issuer()),
                Some("z1"),
                UserRole::User
            )
        );
    }

    // TC: OIDC-02 (the same subject is the same user, refreshed)
    #[tokio::test]
    async fn refreshes_user_when_subject_logs_in_again() {
        let world = World::new().await;
        let first = open(
            &world,
            bob("bob@example.com", UserRole::Admin),
            SessionKind::Cli,
        )
        .await;
        let again = open(
            &world,
            bob("new@example.com", UserRole::User),
            SessionKind::Cli,
        )
        .await;
        let stored = world.ports.store.user(first.user.id).await.unwrap();
        assert_eq!(
            stored.map(|user| (user.id, user.email, user.role)),
            Some((again.user.id, "new@example.com".to_owned(), UserRole::User))
        );
    }

    // TC: OIDC-01, OIDC-07 (a `cli` session of 30 days, as the setup
    // token's exchange opens)
    #[tokio::test]
    async fn opens_thirty_day_cli_session_when_kind_is_cli() {
        let world = World::new().await;
        let grant = open(
            &world,
            bob("bob@example.com", UserRole::User),
            SessionKind::Cli,
        )
        .await;
        let session = stored_session(&world, &grant.token).await.unwrap();
        assert_eq!(
            (session.kind, session.name.as_str(), session.expires_at),
            (SessionKind::Cli, "laptop", at(NOV_12))
        );
    }

    // TC: OIDC-01 (a browser session lives 24 hours)
    #[tokio::test]
    async fn opens_day_long_browser_session_when_kind_is_browser() {
        let world = World::new().await;
        let grant = open(
            &world,
            bob("bob@example.com", UserRole::User),
            SessionKind::Browser,
        )
        .await;
        let session = stored_session(&world, &grant.token).await.unwrap();
        assert_eq!(
            session.expires_at,
            world.ports.clock.now() + Duration::hours(24)
        );
    }

    // TC: OIDC-01 (MCP sessions are not a login module's)
    #[tokio::test]
    async fn refuses_mcp_session_when_asked() {
        let world = World::new().await;
        let refused = world
            .auth()
            .open_session(bob("", UserRole::User), "laptop", SessionKind::Mcp)
            .await;
        assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
    }

    // TC: OIDC-01 (a login another user has is a conflict, nothing opened)
    #[tokio::test]
    async fn conflicts_when_login_belongs_to_another_user() {
        let world = World::new().await;
        let mut taken = bob("", UserRole::User);
        taken.login = world.owner.login.clone();
        let refused = world
            .auth()
            .open_session(taken, "laptop", SessionKind::Cli)
            .await;
        assert!(matches!(refused, Err(Error::Conflict(_))), "{refused:?}");
    }
}
