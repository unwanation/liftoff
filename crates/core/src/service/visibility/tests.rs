//! VIS-01…VIS-06: who may share an environment, and who may open a
//! private one.

use time::Duration;

use super::*;
use crate::domain::{EnvironmentId, GateSecret, UserRole, Visibility};
use crate::fakes::{HookCall, World};
use crate::ports::{HookResult, Resource};

/// The tunnel environment `shop-dev` of `shop`, owned by `owner`.
async fn tunnel_of(world: &World, owner: &User) -> Environment {
    world
        .add_tunnel(&world.shop, "shop-dev", owner)
        .await
        .unwrap()
}

/// `name` as an [`EnvName`].
fn env(name: &str) -> EnvName {
    EnvName::new(name).unwrap()
}

/// The visibility of `environment` as stored now.
async fn stored_visibility(world: &World, environment: &Environment) -> Visibility {
    world
        .ports
        .store
        .environment(environment.id)
        .await
        .unwrap()
        .unwrap()
        .visibility
}

/// Stores `environment` with `visibility`.
async fn set_visibility(world: &World, environment: &Environment, visibility: Visibility) {
    let changed = Environment {
        visibility,
        ..environment.clone()
    };
    world
        .ports
        .store
        .update_environment(&changed)
        .await
        .unwrap();
}

mod share {
    use super::*;

    // TC: VIS-01
    #[tokio::test]
    async fn shared_when_owner_shares_private_tunnel() {
        let world = World::new().await;
        let tunnel = tunnel_of(&world, &world.owner).await;
        let shared = world
            .visibility()
            .share(&world.owner, &world.shop.name, &env("shop-dev"))
            .await
            .map(|environment| environment.visibility);
        assert_eq!(
            (shared.ok(), stored_visibility(&world, &tunnel).await),
            (Some(Visibility::Shared), Visibility::Shared)
        );
    }

    // TC: VIS-01 (one question to `authorize`, about the environment)
    #[tokio::test]
    async fn asks_authorize_once_about_environment_when_shared() {
        let world = World::new().await;
        let tunnel = tunnel_of(&world, &world.owner).await;
        world.log().clear();
        world
            .visibility()
            .share(&world.owner, &world.shop.name, &env("shop-dev"))
            .await
            .unwrap();
        assert_eq!(
            world.log().authorized(),
            vec![(
                world.owner.id,
                Action::EnvironmentShare,
                Resource::Environment(tunnel.id)
            )]
        );
    }

    // TC: VIS-06 (a member the `authorize` hook lets in still may not
    // share another user's tunnel)
    #[tokio::test]
    async fn forbidden_and_kept_private_when_tunnel_is_another_users() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = tunnel_of(&world, &world.owner).await;
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let result = world
            .visibility()
            .share(&alice, &world.shop.name, &env("shop-dev"))
            .await;
        assert_eq!(
            (
                matches!(result, Err(Error::Forbidden(_))),
                stored_visibility(&world, &tunnel).await
            ),
            (true, Visibility::Private),
            "{result:?}"
        );
    }

    // TC: VIS-06 (without a module nobody but the admin passes
    // `authorize`)
    #[tokio::test]
    async fn forbidden_when_actor_not_admin_and_no_module_answers() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        let result = world
            .visibility()
            .share(&carol, &world.shop.name, &world.prod.name)
            .await;
        assert!(
            matches!(&result, Err(Error::Forbidden(reason)) if reason == "carol may not environment.share"),
            "{result:?}"
        );
    }

    // TC: VIS-06 (the owner who is not the admin shares their own tunnel)
    #[tokio::test]
    async fn shared_when_non_admin_owner_shares_own_tunnel() {
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
        let shared = world
            .visibility()
            .share(&alice, &world.shop.name, &env("shop-alice"))
            .await
            .map(|environment| environment.visibility);
        assert_eq!(
            (shared.ok(), stored_visibility(&world, &tunnel).await),
            (Some(Visibility::Shared), Visibility::Shared)
        );
    }

    // TC: VIS-06 (the admin may share any environment)
    #[tokio::test]
    async fn shared_when_admin_shares_another_users_tunnel() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = world
            .add_tunnel(&world.shop, "shop-alice", &alice)
            .await
            .unwrap();
        world
            .visibility()
            .share(&world.owner, &world.shop.name, &env("shop-alice"))
            .await
            .unwrap();
        assert_eq!(stored_visibility(&world, &tunnel).await, Visibility::Shared);
    }

    // TC: VIS-06
    #[tokio::test]
    async fn not_found_when_environment_unknown() {
        let world = World::new().await;
        let result = world
            .visibility()
            .share(&world.owner, &world.shop.name, &env("nope"))
            .await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }
}

mod unshare {
    use super::*;

    // TC: VIS-01
    #[tokio::test]
    async fn private_when_owner_unshares_shared_tunnel() {
        let world = World::new().await;
        let tunnel = tunnel_of(&world, &world.owner).await;
        set_visibility(&world, &tunnel, Visibility::Shared).await;
        let unshared = world
            .visibility()
            .unshare(&world.owner, &world.shop.name, &env("shop-dev"))
            .await
            .map(|environment| environment.visibility);
        assert_eq!(
            (unshared.ok(), stored_visibility(&world, &tunnel).await),
            (Some(Visibility::Private), Visibility::Private)
        );
    }

    // TC: VIS-01 (one question to `authorize`, about the environment)
    #[tokio::test]
    async fn asks_authorize_once_about_environment_when_unshared() {
        let world = World::new().await;
        world.log().clear();
        world
            .visibility()
            .unshare(&world.owner, &world.shop.name, &world.prod.name)
            .await
            .unwrap();
        assert_eq!(
            world.log().authorized(),
            vec![(
                world.owner.id,
                Action::EnvironmentUnshare,
                Resource::Environment(world.prod.id)
            )]
        );
    }

    // TC: VIS-06
    #[tokio::test]
    async fn forbidden_and_kept_shared_when_tunnel_is_another_users() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = tunnel_of(&world, &world.owner).await;
        set_visibility(&world, &tunnel, Visibility::Shared).await;
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let result = world
            .visibility()
            .unshare(&alice, &world.shop.name, &env("shop-dev"))
            .await;
        assert_eq!(
            (
                matches!(result, Err(Error::Forbidden(_))),
                stored_visibility(&world, &tunnel).await
            ),
            (true, Visibility::Shared),
            "{result:?}"
        );
    }
}

mod check {
    use super::*;

    /// The dev address of `shop-dev`.
    const DEV_HOST: &str = "shop-dev.apps.test";

    /// What a browser sends.
    const BROWSER_ACCEPT: &str = "text/html,application/xhtml+xml,*/*;q=0.8";

    /// The tunnel `shop-dev` of `owner` with its dev address.
    async fn dev_tunnel(world: &World, owner: &User) -> Environment {
        let tunnel = tunnel_of(world, owner).await;
        world
            .add_address(&world.shop, &tunnel, DEV_HOST)
            .await
            .unwrap();
        tunnel
    }

    /// A request for `GET /` on `host` with `cookie`, `bearer` and
    /// `accept`.
    fn request_to(
        host: &str,
        cookie: Option<GateSecret>,
        bearer: Option<Bearer>,
        accept: Option<&str>,
    ) -> AccessRequest {
        AccessRequest {
            host: Host::new(host).unwrap(),
            cookie,
            session: None,
            bearer,
            accept: accept.map(ToOwned::to_owned),
            request: HookRequest {
                method: "GET".to_owned(),
                path: "/".to_owned(),
                headers: Vec::new(),
            },
        }
    }

    /// A request for `GET /` on the dev address.
    fn request(
        cookie: Option<GateSecret>,
        bearer: Option<Bearer>,
        accept: Option<&str>,
    ) -> AccessRequest {
        request_to(DEV_HOST, cookie, bearer, accept)
    }

    /// A browser's request with the cookie `cookie`.
    fn from_browser(cookie: Option<GateSecret>) -> AccessRequest {
        request(cookie, None, Some(BROWSER_ACCEPT))
    }

    /// `user`'s session on the dev address, the cookie a browser holds
    /// there.
    async fn cookie_of(world: &World, user: &User) -> GateSecret {
        world.host_login(user, DEV_HOST).await.unwrap()
    }

    fn target(environment: &Environment) -> LinkTarget {
        LinkTarget::Environment(environment.id)
    }

    // TC: VIS-02
    #[tokio::test]
    async fn allows_without_session_when_target_shared() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        set_visibility(&world, &tunnel, Visibility::Shared).await;
        let decision = world
            .access()
            .check(&from_browser(None), &target(&tunnel))
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Allow { user: None }));
    }

    // TC: VIS-03 (a browser goes to the login: 302)
    #[tokio::test]
    async fn login_when_private_and_browser_has_no_session() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let decision = world
            .access()
            .check(&from_browser(None), &target(&tunnel))
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Login));
    }

    // TC: VIS-03 (a client that asks for JSON gets 401)
    #[tokio::test]
    async fn unauthenticated_when_private_and_json_client_has_no_session() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let decision = world
            .access()
            .check(
                &request(None, None, Some("application/json")),
                &target(&tunnel),
            )
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Unauthenticated));
    }

    // TC: VIS-03 (the host session ends with its browser session, 24
    // hours)
    #[tokio::test]
    async fn login_when_private_and_host_session_expired() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let cookie = cookie_of(&world, &world.owner).await;
        world
            .ports
            .clock
            .advance(Duration::hours(24) + Duration::seconds(1));
        let decision = world
            .access()
            .check(&from_browser(Some(cookie)), &target(&tunnel))
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Login));
    }

    // TC: VIS-03 (a cookie that is no stored host session counts as none)
    #[tokio::test]
    async fn login_when_private_and_cookie_unknown() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let (session, _api_cookie) = world.records.browser_session(&world.owner);
        let (_never_stored, cookie) = world.records.host_session(&session, DEV_HOST).unwrap();
        let decision = world
            .access()
            .check(&from_browser(Some(cookie)), &target(&tunnel))
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Login));
    }

    // TC: VIS-03 (the cookie is a session of this host only: one of
    // another host counts as none)
    #[tokio::test]
    async fn login_when_private_and_cookie_is_session_of_another_host() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let elsewhere = world
            .host_login(&world.owner, "other-dev.apps.test")
            .await
            .unwrap();
        let decision = world
            .access()
            .check(&from_browser(Some(elsewhere)), &target(&tunnel))
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Login));
    }

    // TC: VIS-04
    #[tokio::test]
    async fn allows_owner_when_cookie_is_owners_host_session() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let cookie = cookie_of(&world, &world.owner).await;
        let decision = world
            .access()
            .check(&from_browser(Some(cookie)), &target(&tunnel))
            .await;
        assert_eq!(
            decision.ok(),
            Some(AccessDecision::Allow {
                user: Some(world.owner.clone())
            })
        );
    }

    // TC: VIS-04 (a `cli` session as the bearer, as `curl` from the
    // owner's machine sends it)
    #[tokio::test]
    async fn allows_owner_when_bearer_is_owners_cli_session() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let cli = world.login(&world.owner).await.unwrap();
        let decision = world
            .access()
            .check(
                &request(None, Some(Bearer::Session(cli)), Some("application/json")),
                &target(&tunnel),
            )
            .await;
        assert_eq!(
            decision.ok(),
            Some(AccessDecision::Allow {
                user: Some(world.owner.clone())
            })
        );
    }

    // TC: VIS-04 (the bearer is a `cli` session; a `browser` one there
    // counts as none)
    #[tokio::test]
    async fn unauthenticated_when_bearer_is_browser_session() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let browser = world.browser_login(&world.owner).await.unwrap();
        let decision = world
            .access()
            .check(
                &request(
                    None,
                    Some(Bearer::Session(browser)),
                    Some("application/json"),
                ),
                &target(&tunnel),
            )
            .await;
        assert_eq!(decision.ok(), Some(AccessDecision::Unauthenticated));
    }

    // TC: VIS-04 (the admin opens every private environment)
    #[tokio::test]
    async fn allows_admin_when_tunnel_is_another_users() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = world
            .add_tunnel(&world.shop, "shop-alice", &alice)
            .await
            .unwrap();
        world
            .add_address(&world.shop, &tunnel, DEV_HOST)
            .await
            .unwrap();
        let cookie = cookie_of(&world, &world.owner).await;
        let decision = world
            .access()
            .check(&from_browser(Some(cookie)), &target(&tunnel))
            .await;
        assert_eq!(
            decision.ok(),
            Some(AccessDecision::Allow {
                user: Some(world.owner.clone())
            })
        );
    }

    // TC: VIS-04 (without a module someone else's session does not open
    // a private environment)
    #[tokio::test]
    async fn forbidden_when_session_of_another_user_and_no_module_answers() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let cookie = cookie_of(&world, &alice).await;
        let decision = world
            .access()
            .check(&from_browser(Some(cookie)), &target(&tunnel))
            .await;
        assert!(
            matches!(decision, Ok(AccessDecision::Forbidden(_))),
            "{decision:?}"
        );
    }

    // TC: VIS-05 (`tenants`: a member of the tenant passes)
    #[tokio::test]
    async fn allows_member_when_gate_hook_allows() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let cookie = cookie_of(&world, &alice).await;
        world.ports.hooks.answer_next("gate", HookResult::Allow);
        let decision = world
            .access()
            .check(&from_browser(Some(cookie)), &target(&tunnel))
            .await;
        assert_eq!(
            decision.ok(),
            Some(AccessDecision::Allow {
                user: Some(alice.clone())
            })
        );
    }

    // TC: VIS-05 (`tenants`: someone outside the tenant gets 403)
    #[tokio::test]
    async fn forbidden_when_gate_hook_denies() {
        let world = World::new().await;
        let bob = world.add_user("bob", UserRole::User).await.unwrap();
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let cookie = cookie_of(&world, &bob).await;
        world.ports.hooks.answer_next(
            "gate",
            HookResult::Deny("bob is not a member of default".to_owned()),
        );
        let decision = world
            .access()
            .check(&from_browser(Some(cookie)), &target(&tunnel))
            .await;
        assert!(
            matches!(&decision, Ok(AccessDecision::Forbidden(reason)) if reason.contains("bob is not a member of default")),
            "{decision:?}"
        );
    }

    // TC: VIS-05 (the decision is the phase-2 `gate` hook's, asked about
    // the host of the request)
    #[tokio::test]
    async fn asks_gate_hook_about_host_when_session_is_not_owners() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let cookie = cookie_of(&world, &alice).await;
        world.log().clear();
        world.ports.hooks.answer_next("gate", HookResult::Allow);
        world
            .access()
            .check(&from_browser(Some(cookie)), &target(&tunnel))
            .await
            .unwrap();
        assert_eq!(
            world.ports.hooks.calls(),
            vec![HookCall::Gate {
                host: Host::new(DEV_HOST).unwrap(),
                path: "/".to_owned()
            }]
        );
    }

    /// Project `blog` with a tunnel `blog-dev` of `owner` at the address
    /// `blog-dev.apps.test`, which `user` holds a session of; the host.
    async fn address_of_another_project(world: &World, user: &User) -> (&'static str, GateSecret) {
        const BLOG_HOST: &str = "blog-dev.apps.test";
        let blog = world.add_project("blog", "owner/blog").await.unwrap();
        let blog_dev = world
            .add_tunnel(&blog, "blog-dev", &world.owner)
            .await
            .unwrap();
        let domain = world.add_domain(&blog, BLOG_HOST).await.unwrap();
        let binding = world
            .records
            .binding(&blog_dev, &domain, "web", 80, 20_200)
            .unwrap();
        world.ports.store.insert_binding(&binding).await.unwrap();
        (BLOG_HOST, world.host_login(user, BLOG_HOST).await.unwrap())
    }

    // TC: VIS-05 (the `gate` hook judges the host, so the host must be
    // the target's: a member of `blog` whose request names a `blog` host
    // does not reach a private tunnel of `shop`)
    #[tokio::test]
    async fn forbidden_without_asking_gate_when_host_is_address_of_another_project() {
        let world = World::new().await;
        let alice = world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let (blog_host, cookie) = address_of_another_project(&world, &alice).await;
        world.log().clear();
        world.ports.hooks.answer_next("gate", HookResult::Allow);
        let decision = world
            .access()
            .check(
                &request_to(blog_host, Some(cookie), None, Some(BROWSER_ACCEPT)),
                &target(&tunnel),
            )
            .await;
        assert_eq!(
            (
                matches!(decision, Ok(AccessDecision::Forbidden(_))),
                world.ports.hooks.calls()
            ),
            (true, vec![]),
            "{decision:?}"
        );
    }

    // TC: VIS-02 (a shared target opens on its own address only)
    #[tokio::test]
    async fn forbidden_when_target_shared_and_host_is_not_its_address() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        set_visibility(&world, &tunnel, Visibility::Shared).await;
        let decision = world
            .access()
            .check(
                &request_to("unknown.apps.test", None, None, Some(BROWSER_ACCEPT)),
                &target(&tunnel),
            )
            .await;
        assert!(
            matches!(decision, Ok(AccessDecision::Forbidden(_))),
            "{decision:?}"
        );
    }

    // TC: VIS-04 (an address of another environment of the same project
    // does not open the target, even for its owner)
    #[tokio::test]
    async fn forbidden_when_host_is_address_of_another_environment() {
        let world = World::new().await;
        let tunnel = dev_tunnel(&world, &world.owner).await;
        let prod_host = world.domain.host.as_str().to_owned();
        let cookie = world.host_login(&world.owner, &prod_host).await.unwrap();
        let decision = world
            .access()
            .check(
                &request_to(&prod_host, Some(cookie), None, Some(BROWSER_ACCEPT)),
                &target(&tunnel),
            )
            .await;
        assert!(
            matches!(decision, Ok(AccessDecision::Forbidden(_))),
            "{decision:?}"
        );
    }

    // TC: VIS-02 (a target that does not exist is not found)
    #[tokio::test]
    async fn not_found_when_target_unknown() {
        let world = World::new().await;
        let missing = LinkTarget::Environment(EnvironmentId::new(ulid::Ulid::from_parts(9, 9)));
        let decision = world.access().check(&from_browser(None), &missing).await;
        assert!(matches!(decision, Err(Error::NotFound(_))), "{decision:?}");
    }
}
