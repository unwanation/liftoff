//! OTL-01…OTL-05: one-time links.

use time::Duration;

use super::*;
use crate::domain::{ArtifactId, Environment, EnvironmentId, OneTimeLink, SessionKind, UserRole};
use crate::fakes::World;
use crate::ports::{HookResult, Resource};

/// The prefix of every link of the world (its API host is
/// `liftoff.test`).
const LINK_PREFIX: &str = "https://liftoff.test/api/v1/auth/links/";

/// The tunnel environment `shop-dev` of `shop`, owned by `owner`.
async fn tunnel(world: &World) -> Environment {
    world
        .add_tunnel(&world.shop, "shop-dev", &world.owner)
        .await
        .unwrap()
}

fn target(environment: &Environment) -> LinkTarget {
    LinkTarget::Environment(environment.id)
}

/// The token at the end of `link`'s URL.
fn token_of(link: &IssuedLink) -> LinkSecret {
    let raw = link
        .url
        .strip_prefix(LINK_PREFIX)
        .unwrap_or_else(|| panic!("{} is not a link of the API host", link.url));
    LinkSecret::parse(raw).unwrap()
}

/// A link of `owner` to `environment`, issued now; its token.
async fn issued(world: &World, environment: &Environment) -> LinkSecret {
    let link = world
        .links()
        .issue(&world.owner, &target(environment))
        .await
        .unwrap();
    token_of(&link)
}

mod issue {
    use super::*;

    // TC: OTL-01
    #[tokio::test]
    async fn expires_ten_minutes_after_issue_when_issued() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let now = world.ports.clock.now();
        let link = world
            .links()
            .issue(&world.owner, &target(&tunnel))
            .await
            .unwrap();
        assert_eq!(link.expires_at, now.saturating_add(Duration::minutes(10)));
    }

    // TC: OTL-01 (the link is `GET /auth/links/{token}` on the API host)
    #[tokio::test]
    async fn points_at_auth_links_on_api_host_when_issued() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let link = world
            .links()
            .issue(&world.owner, &target(&tunnel))
            .await
            .unwrap();
        let token = link.url.strip_prefix(LINK_PREFIX).unwrap_or_default();
        assert_eq!(
            (
                token.starts_with(LinkSecret::PREFIX),
                LinkSecret::parse(token).is_ok()
            ),
            (true, true),
            "{}",
            link.url
        );
    }

    // TC: OTL-05
    #[tokio::test]
    async fn stores_unused_link_by_hash_of_token_when_issued() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let now = world.ports.clock.now();
        let token = issued(&world, &tunnel).await;
        let stored = world
            .ports
            .store
            .one_time_link(&token.hash())
            .await
            .unwrap();
        assert_eq!(
            stored,
            Some(OneTimeLink {
                token_hash: token.hash(),
                user_id: world.owner.id,
                target: target(&tunnel),
                expires_at: now.saturating_add(Duration::minutes(10)),
                used_at: None,
                created_at: now,
            })
        );
    }

    // TC: OTL-05 (the token itself is nowhere in what is stored)
    #[tokio::test]
    async fn keeps_token_out_of_store_when_issued() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        let stored = world
            .ports
            .store
            .one_time_link(&token.hash())
            .await
            .unwrap();
        let shown = format!("{stored:?}");
        assert_eq!(
            (stored.is_some(), shown.contains(token.expose())),
            (true, false),
            "{shown}"
        );
    }

    // TC: OTL-03 (a link is only issued for a target the actor may open)
    #[tokio::test]
    async fn forbidden_when_actor_may_not_open_target() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        let tunnel = tunnel(&world).await;
        let result = world.links().issue(&carol, &target(&tunnel)).await;
        assert!(
            matches!(&result, Err(Error::Forbidden(reason)) if reason == "carol may not link.issue"),
            "{result:?}"
        );
    }

    // TC: OTL-03 (one question to `authorize`, about the target)
    #[tokio::test]
    async fn asks_authorize_once_about_target_when_issued() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        world.log().clear();
        world
            .links()
            .issue(&world.owner, &target(&tunnel))
            .await
            .unwrap();
        assert_eq!(
            world.log().authorized(),
            vec![(
                world.owner.id,
                Action::LinkIssue,
                Resource::Environment(tunnel.id)
            )]
        );
    }

    // TC: VIS-06, OTL-03 (a module may let a member open another user's
    // tunnel, but no link to it is handed out but by its owner or the
    // admin)
    #[tokio::test]
    async fn forbidden_and_nothing_stored_when_tunnel_is_another_users_and_authorize_allows() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let bob = world.add_user("bob", UserRole::User).await.unwrap();
        let tunnel = world
            .add_tunnel(&world.shop, "shop-alice", &alice)
            .await
            .unwrap();
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        world.log().clear();
        let result = world.links().issue(&bob, &target(&tunnel)).await;
        let stored = world
            .log()
            .calls()
            .contains(&crate::fakes::Call::Store("insert_one_time_link"));
        assert!(
            matches!(&result, Err(Error::Forbidden(_))) && !stored,
            "{result:?} {:?}",
            world.log().calls()
        );
    }

    // TC: VIS-06 (the owner of the tunnel issues links to it)
    #[tokio::test]
    async fn issued_when_actor_owns_tunnel_and_authorize_allows() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = world
            .add_tunnel(&world.shop, "shop-alice", &alice)
            .await
            .unwrap();
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let result = world.links().issue(&alice, &target(&tunnel)).await;
        assert!(result.is_ok(), "{result:?}");
    }

    // TC: OTL-03
    #[tokio::test]
    async fn not_found_when_target_unknown() {
        let world = World::new().await;
        let missing = LinkTarget::Environment(EnvironmentId::new(ulid::Ulid::from_parts(9, 9)));
        let result = world.links().issue(&world.owner, &missing).await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }

    /// Alice's private artifact (the hook lets her publish).
    async fn alices_artifact(world: &World) -> (User, ArtifactId) {
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let published = world
            .artifacts()
            .publish(
                &alice,
                crate::service::ArtifactContent {
                    html: b"<h1>v1</h1>".to_vec(),
                    ..crate::service::ArtifactContent::default()
                },
            )
            .await
            .unwrap();
        (alice, published.artifact.id)
    }

    // TC: ART-01 (`lof artifact open`: a link to an artifact for its
    // owner, asked about the artifact)
    #[tokio::test]
    async fn issued_to_artifact_when_owner_asks_and_authorize_allows() {
        let world = World::new().await;
        let (alice, id) = alices_artifact(&world).await;
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        world.log().clear();
        let result = world.links().issue(&alice, &LinkTarget::Artifact(id)).await;
        assert_eq!(
            (result.is_ok(), world.log().authorized()),
            (
                true,
                vec![(alice.id, Action::LinkIssue, Resource::Artifact(id))]
            ),
            "{result:?}"
        );
    }

    // TC: ART-11 (another user's artifact: no link, whatever the hook
    // says)
    #[tokio::test]
    async fn forbidden_when_artifact_is_another_users_and_authorize_allows() {
        let world = World::new().await;
        let (_alice, id) = alices_artifact(&world).await;
        let bob = world.add_user("bob", UserRole::User).await.unwrap();
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let result = world.links().issue(&bob, &LinkTarget::Artifact(id)).await;
        assert!(matches!(result, Err(Error::Forbidden(_))), "{result:?}");
    }
}

mod redeem {
    use super::*;

    // TC: OTL-04
    #[tokio::test]
    async fn opens_browser_session_of_link_user_when_redeemed() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        let opened = world
            .links()
            .redeem(&token, Some(&target(&tunnel)))
            .await
            .map(|opened| (opened.session.kind, opened.session.user_id));
        assert_eq!(opened.ok(), Some((SessionKind::Browser, world.owner.id)));
    }

    // TC: OTL-04 (24 hours)
    #[tokio::test]
    async fn session_lives_twenty_four_hours_when_redeemed() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        world.ports.clock.advance(Duration::minutes(3));
        let now = world.ports.clock.now();
        let opened = world
            .links()
            .redeem(&token, Some(&target(&tunnel)))
            .await
            .map(|opened| opened.session.expires_at);
        assert_eq!(opened.ok(), Some(now.saturating_add(Duration::hours(24))));
    }

    // TC: OTL-04 (the cookie's secret finds the stored session)
    #[tokio::test]
    async fn stores_session_under_hash_of_cookie_when_redeemed() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        let opened = world
            .links()
            .redeem(&token, Some(&target(&tunnel)))
            .await
            .unwrap();
        let stored = world
            .ports
            .store
            .session_by_hash(&opened.token.hash())
            .await
            .unwrap();
        assert_eq!(stored, Some(opened.session));
    }

    // TC: OTL-04 (the browser goes on to the link's target)
    #[tokio::test]
    async fn names_link_target_when_redeemed() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        let opened = world
            .links()
            .redeem(&token, Some(&target(&tunnel)))
            .await
            .map(|opened| opened.target);
        assert_eq!(opened.ok(), Some(target(&tunnel)));
    }

    // TC: OTL-04 (`GET /auth/links/{token}` names no target: the link's
    // own is taken)
    #[tokio::test]
    async fn opens_session_for_links_own_target_when_none_expected() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        let opened = world
            .links()
            .redeem(&token, None)
            .await
            .map(|opened| (opened.session.kind, opened.target));
        assert_eq!(opened.ok(), Some((SessionKind::Browser, target(&tunnel))));
    }

    // TC: OTL-01
    #[tokio::test]
    async fn gone_when_redeemed_after_ten_minutes() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        world
            .ports
            .clock
            .advance(Duration::minutes(10) + Duration::seconds(1));
        let result = world.links().redeem(&token, Some(&target(&tunnel))).await;
        assert!(matches!(result, Err(Error::Gone(_))), "{result:?}");
    }

    // TC: OTL-01 (the link is good until its ten minutes are up)
    #[tokio::test]
    async fn opens_session_when_redeemed_just_before_ten_minutes() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        world
            .ports
            .clock
            .advance(Duration::minutes(10) - Duration::seconds(1));
        let result = world.links().redeem(&token, Some(&target(&tunnel))).await;
        assert!(result.is_ok(), "{result:?}");
    }

    // TC: OTL-02
    #[tokio::test]
    async fn gone_when_redeemed_twice() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        world
            .links()
            .redeem(&token, Some(&target(&tunnel)))
            .await
            .unwrap();
        let second = world.links().redeem(&token, Some(&target(&tunnel))).await;
        assert!(matches!(second, Err(Error::Gone(_))), "{second:?}");
    }

    // TC: OTL-02 (the store keeps when it was used)
    #[tokio::test]
    async fn marks_link_used_when_redeemed() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        world.ports.clock.advance(Duration::minutes(1));
        let now = world.ports.clock.now();
        world
            .links()
            .redeem(&token, Some(&target(&tunnel)))
            .await
            .unwrap();
        let used_at = world
            .ports
            .store
            .one_time_link(&token.hash())
            .await
            .unwrap()
            .and_then(|link| link.used_at);
        assert_eq!(used_at, Some(now));
    }

    // TC: OTL-02 (`lof admin sessions revoke` ends the links of the user
    // too: a link issued before opens no new session after)
    #[tokio::test]
    async fn gone_when_sessions_of_link_user_revoked_after_issue() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        world
            .auth()
            .revoke_sessions(&world.owner, &world.owner.login)
            .await
            .unwrap();
        let result = world.links().redeem(&token, None).await;
        assert!(matches!(result, Err(Error::Gone(_))), "{result:?}");
    }

    // TC: OTL-02 (a token that was never issued opens nothing either)
    #[tokio::test]
    async fn gone_when_token_never_issued() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token =
            LinkSecret::parse(&format!("{}{}", LinkSecret::PREFIX, "a".repeat(40))).unwrap();
        let result = world.links().redeem(&token, Some(&target(&tunnel))).await;
        assert!(matches!(result, Err(Error::Gone(_))), "{result:?}");
    }

    // TC: OTL-03
    #[tokio::test]
    async fn forbidden_when_redeemed_for_another_target() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        let result = world
            .links()
            .redeem(&token, Some(&target(&world.prod)))
            .await;
        assert!(matches!(result, Err(Error::Forbidden(_))), "{result:?}");
    }

    // TC: OTL-03 (a try for another target does not use the link up)
    #[tokio::test]
    async fn opens_session_for_own_target_after_a_try_for_another() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        let _refused = world
            .links()
            .redeem(&token, Some(&target(&world.prod)))
            .await;
        let result = world.links().redeem(&token, Some(&target(&tunnel))).await;
        assert!(result.is_ok(), "{result:?}");
    }
}

mod prune {
    use super::*;

    // TC: OTL-01 (expired links, used or not, are deleted; a live one
    // stays redeemable)
    #[tokio::test]
    async fn deletes_expired_links_and_keeps_live_one_when_pruned() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let used = issued(&world, &tunnel).await;
        world.links().redeem(&used, None).await.unwrap();
        let unused = issued(&world, &tunnel).await;
        world.ports.clock.advance(Duration::minutes(5));
        let live = issued(&world, &tunnel).await;
        world
            .ports
            .clock
            .advance(Duration::minutes(5) + Duration::seconds(1));
        let pruned = world.links().prune().await.unwrap();
        let store = &world.ports.store;
        let kept = (
            store.one_time_link(&used.hash()).await.unwrap().is_some(),
            store.one_time_link(&unused.hash()).await.unwrap().is_some(),
            store.one_time_link(&live.hash()).await.unwrap().is_some(),
        );
        assert_eq!((pruned, kept), (2, (false, false, true)));
    }
}

mod open {
    use super::*;
    use crate::domain::{EnvState, Visibility};

    /// The address of `shop-dev` in these tests.
    const DEV_HOST: &str = "shop-dev.apps.test";

    /// `shop-dev` at [`DEV_HOST`].
    async fn tunnel_at_address(world: &World) -> Environment {
        let tunnel = Environment {
            state: EnvState::Running,
            visibility: Visibility::Private,
            ..tunnel(world).await
        };
        world.ports.store.update_environment(&tunnel).await.unwrap();
        world
            .add_address(&world.shop, &tunnel, DEV_HOST)
            .await
            .unwrap();
        tunnel
    }

    // TC: OTL-07 (the code is the gate's for the target's address, and it
    // carries the path to return to there)
    #[tokio::test]
    async fn issues_handover_code_for_target_address_when_opened() {
        let world = World::new().await;
        let tunnel = tunnel_at_address(&world).await;
        let token = issued(&world, &tunnel).await;
        let opened = world
            .links()
            .open(&token, None, ReturnPath::root())
            .await
            .unwrap();
        let code = world
            .ports
            .store
            .take_gate_code(&opened.code.hash())
            .await
            .unwrap()
            .map(|code| (code.host, code.rd, code.session_id));
        assert_eq!(
            (opened.host.as_str(), code),
            (
                DEV_HOST,
                Some((
                    Host::new(DEV_HOST).unwrap(),
                    ReturnPath::root(),
                    opened.browser.session.id
                ))
            )
        );
    }

    // TC: OTL-07 (a link pasted on the page of its address returns to the
    // path the browser came from)
    #[tokio::test]
    async fn code_returns_to_asked_path_when_opened_at_address() {
        let world = World::new().await;
        let tunnel = tunnel_at_address(&world).await;
        let token = issued(&world, &tunnel).await;
        let rd = ReturnPath::new("/items?x=1").unwrap();
        let host = Host::new(DEV_HOST).unwrap();
        let opened = world
            .links()
            .open(&token, Some(&host), rd.clone())
            .await
            .unwrap();
        let code = world
            .ports
            .store
            .take_gate_code(&opened.code.hash())
            .await
            .unwrap();
        assert_eq!(code.map(|code| code.rd), Some(rd));
    }

    // TC: OTL-03 (pasted for another address: refused, the link stays
    // unused)
    #[tokio::test]
    async fn forbidden_and_link_unused_when_opened_at_other_address() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let api = world.records.function(&world.shop, "api").unwrap();
        world.ports.store.insert_environment(&api).await.unwrap();
        world
            .add_address(&world.shop, &api, "api.apps.test")
            .await
            .unwrap();
        let token = issued(&world, &tunnel).await;
        let other = Host::new("api.apps.test").unwrap();
        let refused = world
            .links()
            .open(&token, Some(&other), ReturnPath::root())
            .await;
        let again = world.links().redeem(&token, None).await;
        assert_eq!(
            (matches!(refused, Err(Error::Forbidden(_))), again.is_ok()),
            (true, true)
        );
    }

    // TC: OTL-06 (a target without an address opens nothing and keeps the
    // link)
    #[tokio::test]
    async fn not_found_and_link_unused_when_target_has_no_address() {
        let world = World::new().await;
        let tunnel = tunnel(&world).await;
        let token = issued(&world, &tunnel).await;
        let opened = world.links().open(&token, None, ReturnPath::root()).await;
        let link = world
            .ports
            .store
            .one_time_link(&token.hash())
            .await
            .unwrap();
        assert_eq!(
            (
                matches!(opened, Err(Error::NotFound(_))),
                link.map(|link| link.used_at)
            ),
            (true, Some(None))
        );
    }

    // TC: OTL-02 (a used link opens nothing)
    #[tokio::test]
    async fn gone_when_opened_twice() {
        let world = World::new().await;
        let tunnel = tunnel_at_address(&world).await;
        let token = issued(&world, &tunnel).await;
        let _first = world
            .links()
            .open(&token, None, ReturnPath::root())
            .await
            .unwrap();
        let second = world.links().open(&token, None, ReturnPath::root()).await;
        assert!(matches!(second, Err(Error::Gone(_))), "{second:?}");
    }
}

mod open_own {
    use super::*;

    /// The token of the `login_url` of a private artifact the owner just
    /// published, with the artifact's id.
    async fn login_token(world: &World) -> (LinkSecret, ArtifactId) {
        let published = world
            .artifacts()
            .publish(
                &world.owner,
                crate::service::ArtifactContent {
                    html: b"<h1>v1</h1>".to_vec(),
                    ..crate::service::ArtifactContent::default()
                },
            )
            .await
            .unwrap();
        let url = published.login_url.unwrap().url;
        let token = url.rsplit('/').next().unwrap_or_default();
        (LinkSecret::parse(token).unwrap(), published.artifact.id)
    }

    // TC: ART-15 (the `login_url` of a publish opens a `browser` session
    // of the owner for the artifact, with no address to hand it over to)
    #[tokio::test]
    async fn opens_browser_session_for_artifact_when_link_is_artifacts() {
        let world = World::new().await;
        let (token, id) = login_token(&world).await;
        let opened = world.links().open_own(&token).await.unwrap();
        let OpenedOwn::Artifact { browser, artifact } = opened else {
            panic!("not an artifact's: {opened:?}");
        };
        assert_eq!(
            (artifact, browser.session.user_id, browser.session.kind),
            (id, world.owner.id, SessionKind::Browser)
        );
    }

    // TC: OTL-02 (once only)
    #[tokio::test]
    async fn gone_when_artifact_link_opened_twice() {
        let world = World::new().await;
        let (token, _id) = login_token(&world).await;
        let _first = world.links().open_own(&token).await.unwrap();
        let second = world.links().open_own(&token).await;
        assert!(matches!(second, Err(Error::Gone(_))), "{second:?}");
    }
}
