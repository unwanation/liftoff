//! Tests of [`McpAuthService`] over the shared [`World`] (MCPH-02…04, 06): the modules `oidc` and `mcp` registered in
//! the world's runtime and answered by [`FakeModules`], the `oidc` row of
//! the store configured with [`ISSUER`]. The world's clock starts at
//! 2026-10-13T00:00:00Z and drives every expiry here.

use std::collections::BTreeMap;

use time::Duration;

use super::*;
use crate::domain::{
    Bearer, InstalledModule, Login, ModuleConfigRecord, ModuleVersion, UserRole, UserSource,
};
use crate::fakes::{FakeModules, WithHooks, World};
use crate::module::{ModuleHooks, ModuleManifest};
use crate::ports::{HookRequest, McpResource};
use crate::service::Principal;

/// The issuer the admin configured for `oidc`.
const ISSUER: &str = "https://id.example";
/// The `resource` of the module `mcp`.
const RESOURCE: &str = "https://liftoff.test/mcp";
/// The audience the module `mcp` allows.
const AUDIENCE: &str = "liftoff-mcp";
/// An access token; [`FakeModules::verify_as`] answers whatever it is.
const TOKEN: &str = "at-1";

const OIDC: &str =
    r#"{"name":"oidc","version":"0.1.0","hooks":["verify_token"],"replaces":["login"]}"#;
const MCP: &str = r#"{"name":"mcp","version":"0.1.0","hooks":["mcp_resource"]}"#;

type McpPorts = WithHooks<ModuleHooks<FakeModules>>;

/// The world with `oidc` and `mcp` installed, and the fake behind them.
struct Mcp {
    world: World,
    fake: FakeModules,
    ports: Arc<McpPorts>,
}

impl Mcp {
    fn service(&self) -> McpAuthService<McpPorts> {
        McpAuthService::new(Arc::clone(&self.ports))
    }

    async fn verify(&self, bearer: Option<&str>) -> Result<McpGrant, Error> {
        self.service().verify_oauth(bearer).await
    }

    /// `verify_token` of `oidc` answers `identity` from now on.
    fn verifies(&self, identity: OAuthIdentity) {
        self.fake.verify_as("oidc", Ok(Some(identity)));
    }

    fn now(&self) -> Timestamp {
        self.world.ports.clock.now()
    }

    /// The `mcp` sessions in the store, by id.
    async fn mcp_sessions(&self) -> Vec<Session> {
        let mut sessions = Vec::new();
        for user in self.world.ports.store.users().await.unwrap() {
            sessions.extend(
                self.world
                    .ports
                    .store
                    .sessions_of_user(user.id)
                    .await
                    .unwrap()
                    .into_iter()
                    .filter(|session| session.kind == SessionKind::Mcp),
            );
        }
        sessions
    }

    /// Who the REST API takes the bearer `secret` for.
    async fn authenticate(&self, secret: &SessionSecret) -> Result<Principal, Error> {
        let request = HookRequest {
            method: "GET".to_owned(),
            path: "/api/v1/me".to_owned(),
            headers: Vec::new(),
        };
        AuthService::new(Arc::clone(&self.ports))
            .authenticate(&request, Some(&Bearer::Session(secret.clone())))
            .await
    }
}

fn name(module: &str) -> ModuleName {
    ModuleName::new(module).unwrap()
}

async fn install(world: &World, module: &str, config: ModuleConfigRecord) {
    world
        .ports
        .store
        .insert_module(&InstalledModule {
            name: name(module),
            version: ModuleVersion::new("0.1.0").unwrap(),
            sha256: "ab".repeat(32),
            enabled: true,
            config,
            error: None,
            installed_at: Timestamp::UNIX_EPOCH,
        })
        .await
        .unwrap();
}

/// The world with `oidc` (issuer [`ISSUER`]) and, when `with_mcp`, `mcp`
/// installed and registered.
async fn world_with(with_mcp: bool) -> Mcp {
    let world = World::new().await;
    let registry = world.ports.modules.shared_registry();
    registry.register(name("oidc"), ModuleManifest::parse(OIDC).unwrap());
    install(
        &world,
        "oidc",
        BTreeMap::from([(
            "issuer".to_owned(),
            ConfigValue::Plain(Value::String(ISSUER.to_owned())),
        )]),
    )
    .await;
    let fake = FakeModules::new();
    if with_mcp {
        registry.register(name("mcp"), ModuleManifest::parse(MCP).unwrap());
        install(&world, "mcp", BTreeMap::new()).await;
        fake.keep_resource(
            "mcp",
            McpResource {
                resource: RESOURCE.to_owned(),
                audiences: vec![AUDIENCE.to_owned()],
            },
        );
    }
    let ports = Arc::new(WithHooks::new(
        Arc::clone(&world.ports),
        ModuleHooks::new(registry, fake.clone()),
    ));
    Mcp { world, fake, ports }
}

async fn world() -> Mcp {
    world_with(true).await
}

/// `bob`'s identity (subject `z1`) of [`ISSUER`], valid for an hour of
/// the world's clock.
fn bob(mcp: &Mcp) -> OAuthIdentity {
    OAuthIdentity {
        issuer: ISSUER.to_owned(),
        subject: "z1".to_owned(),
        login: Login::new("bob").unwrap(),
        email: "bob@example.com".to_owned(),
        role: UserRole::User,
        expires_at: mcp.now().saturating_add(Duration::hours(1)),
    }
}

mod resource_metadata {
    use super::*;

    // TC: MCPH-02 (the resource of `mcp`, the issuer of `oidc`, and the
    // scopes a client asks the provider for: oidc's defaults with
    // offline_access when oidc has no `scopes`)
    #[tokio::test]
    async fn names_resource_and_oidc_issuer_when_mcp_installed() {
        let mcp = world().await;
        assert_eq!(
            mcp.service().resource_metadata().await.unwrap(),
            ProtectedResource {
                resource: RESOURCE.to_owned(),
                authorization_servers: vec![ISSUER.to_owned()],
                scopes_supported: [
                    "openid",
                    "profile",
                    "email",
                    "urn:zitadel:iam:org:project:roles",
                    "offline_access",
                ]
                .map(str::to_owned)
                .to_vec(),
            }
        );
    }

    // TC: MCPH-02 (the scopes configured for oidc are the ones offered,
    // offline_access added once)
    #[tokio::test]
    async fn offers_oidcs_configured_scopes_when_set() {
        let mcp = world().await;
        let oidc = name("oidc");
        let mut row = mcp.world.ports.store.module(&oidc).await.unwrap().unwrap();
        row.config.insert(
            "scopes".to_owned(),
            ConfigValue::Plain(serde_json::json!(["openid", "email", "offline_access"])),
        );
        mcp.world.ports.store.update_module(&row).await.unwrap();
        assert_eq!(
            mcp.service()
                .resource_metadata()
                .await
                .unwrap()
                .scopes_supported,
            ["openid", "email", "offline_access"]
                .map(str::to_owned)
                .to_vec()
        );
    }

    // TC: MCPH-06
    #[tokio::test]
    async fn module_required_when_mcp_not_installed() {
        let mcp = world_with(false).await;
        let answer = mcp.service().resource_metadata().await;
        assert!(
            matches!(&answer, Err(Error::ModuleRequired(module)) if module == "mcp"),
            "{answer:?}"
        );
    }
}

mod verify_oauth {
    use super::*;

    // TC: MCPH-06 (without the module, even a valid token is 424)
    #[tokio::test]
    async fn module_required_when_mcp_not_installed() {
        let mcp = world_with(false).await;
        mcp.verifies(bob(&mcp));
        let answer = mcp.verify(Some(TOKEN)).await;
        assert!(
            matches!(&answer, Err(Error::ModuleRequired(module)) if module == "mcp"),
            "{answer:?}"
        );
    }

    // TC: MCPH-06 (a disabled module is no module)
    #[tokio::test]
    async fn module_required_when_mcp_disabled() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let _known = mcp
            .world
            .ports
            .modules
            .shared_registry()
            .disable(&name("mcp"), "trapped");
        let answer = mcp.verify(Some(TOKEN)).await;
        assert!(
            matches!(&answer, Err(Error::ModuleRequired(module)) if module == "mcp"),
            "{answer:?}"
        );
    }

    // TC: MCPH-01
    #[tokio::test]
    async fn unauthenticated_when_no_bearer() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let answer = mcp.verify(None).await;
        assert!(
            matches!(answer, Err(Error::Unauthenticated(_))),
            "{answer:?}"
        );
    }

    // TC: MCPH-03 (a token `oidc` refuses)
    #[tokio::test]
    async fn unauthenticated_when_oidc_refuses() {
        let mcp = world().await;
        mcp.fake.verify_as(
            "oidc",
            Err(Error::unauthenticated("the audience is not allowed")),
        );
        let answer = mcp.verify(Some(TOKEN)).await;
        assert!(
            matches!(answer, Err(Error::Unauthenticated(_))),
            "{answer:?}"
        );
    }

    // TC: MCPH-03 (only the issuer configured for `oidc` is trusted)
    #[tokio::test]
    async fn unauthenticated_when_issuer_is_not_oidcs() {
        let mcp = world().await;
        mcp.verifies(OAuthIdentity {
            issuer: "https://evil.example".to_owned(),
            ..bob(&mcp)
        });
        let answer = mcp.verify(Some(TOKEN)).await;
        assert!(
            matches!(answer, Err(Error::Unauthenticated(_))),
            "{answer:?}"
        );
    }

    // TC: MCPH-03 (a foreign issuer creates no user)
    #[tokio::test]
    async fn creates_no_user_when_issuer_is_not_oidcs() {
        let mcp = world().await;
        mcp.verifies(OAuthIdentity {
            issuer: "https://evil.example".to_owned(),
            ..bob(&mcp)
        });
        let _refused = mcp.verify(Some(TOKEN)).await;
        assert_eq!(
            mcp.world
                .ports
                .store
                .user_by_login(&Login::new("bob").unwrap())
                .await
                .unwrap(),
            None
        );
    }

    // TC: MCPH-03 (expiry by the core's clock: `exp` is now)
    #[tokio::test]
    async fn unauthenticated_when_token_expires_now() {
        let mcp = world().await;
        mcp.verifies(OAuthIdentity {
            expires_at: mcp.now(),
            ..bob(&mcp)
        });
        let answer = mcp.verify(Some(TOKEN)).await;
        assert!(
            matches!(answer, Err(Error::Unauthenticated(_))),
            "{answer:?}"
        );
    }

    // TC: MCPH-04 (a session reused until the token's `exp` passes on the
    // core's clock)
    #[tokio::test]
    async fn unauthenticated_when_clock_passes_token_expiry() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let _opened = mcp.verify(Some(TOKEN)).await.unwrap();
        mcp.world.ports.clock.advance(Duration::hours(1));
        let answer = mcp.verify(Some(TOKEN)).await;
        assert!(
            matches!(answer, Err(Error::Unauthenticated(_))),
            "{answer:?}"
        );
    }

    // TC: MCPH-03 (a token without a usable subject)
    #[tokio::test]
    async fn unauthenticated_when_subject_empty() {
        let mcp = world().await;
        mcp.verifies(OAuthIdentity {
            subject: String::new(),
            ..bob(&mcp)
        });
        let answer = mcp.verify(Some(TOKEN)).await;
        assert!(
            matches!(answer, Err(Error::Unauthenticated(_))),
            "{answer:?}"
        );
    }

    // TC: MCPH-04 (the user of `(oidc:<issuer>, sub)`, created as a login
    // does)
    #[tokio::test]
    async fn creates_provider_user_when_token_valid() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let grant = mcp.verify(Some(TOKEN)).await.unwrap();
        assert_eq!(
            (
                grant.user.login.as_str().to_owned(),
                grant.user.source.to_string(),
                grant.user.subject,
                grant.user.role,
            ),
            (
                "bob".to_owned(),
                UserSource::Oidc(OidcIssuer::new(ISSUER).unwrap()).to_string(),
                Some("z1".to_owned()),
                UserRole::User,
            )
        );
    }

    // TC: MCPH-04 (the role the module read, as the device flow does)
    #[tokio::test]
    async fn grants_admin_role_when_identity_has_admin_role() {
        let mcp = world().await;
        mcp.verifies(OAuthIdentity {
            role: UserRole::Admin,
            ..bob(&mcp)
        });
        let grant = mcp.verify(Some(TOKEN)).await.unwrap();
        assert_eq!(grant.user.role, UserRole::Admin);
    }

    // TC: MCPH-04 (one `mcp` session of the user, ending at `exp`)
    #[tokio::test]
    async fn opens_mcp_session_until_token_expiry_when_token_valid() {
        let mcp = world().await;
        let identity = bob(&mcp);
        let expires_at = identity.expires_at;
        mcp.verifies(identity);
        let grant = mcp.verify(Some(TOKEN)).await.unwrap();
        let sessions = mcp.mcp_sessions().await;
        assert_eq!(
            sessions
                .iter()
                .map(|session| (session.user_id, session.expires_at, session.token_hash))
                .collect::<Vec<_>>(),
            vec![(grant.user.id, expires_at, grant.token.hash())]
        );
    }

    // TC: MCPH-04 (the same token finds the same session)
    #[tokio::test]
    async fn reuses_session_when_same_token_repeats() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let first = mcp.verify(Some(TOKEN)).await.unwrap();
        let second = mcp.verify(Some(TOKEN)).await.unwrap();
        assert_eq!(
            (second.token.hash(), mcp.mcp_sessions().await.len()),
            (first.token.hash(), 1)
        );
    }

    // TC: MCPH-04 (a refreshed token is a new session of the same user)
    #[tokio::test]
    async fn opens_second_session_when_token_refreshed() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let first = mcp.verify(Some(TOKEN)).await.unwrap();
        let second = mcp.verify(Some("at-2")).await.unwrap();
        assert_eq!(
            (
                second.token.hash() == first.token.hash(),
                second.user.id == first.user.id,
                mcp.mcp_sessions().await.len()
            ),
            (false, true, 2)
        );
    }

    // TC: MCPH-04 (the expired `mcp` sessions of the user go when a new
    // token opens one: refreshes do not pile up rows)
    #[tokio::test]
    async fn deletes_expired_mcp_sessions_when_new_token_opens_session() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let _first = mcp.verify(Some(TOKEN)).await.unwrap();
        mcp.world.ports.clock.advance(Duration::hours(2));
        mcp.verifies(bob(&mcp));
        let second = mcp.verify(Some("at-2")).await.unwrap();
        assert_eq!(
            mcp.mcp_sessions()
                .await
                .into_iter()
                .map(|session| session.token_hash)
                .collect::<Vec<_>>(),
            vec![second.token.hash()]
        );
    }

    // TC: MCPH-04 (a session is bound to the account that opened it)
    #[tokio::test]
    async fn unauthenticated_when_same_token_names_other_subject() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let _bob = mcp.verify(Some(TOKEN)).await.unwrap();
        mcp.verifies(OAuthIdentity {
            subject: "z2".to_owned(),
            login: Login::new("carol").unwrap(),
            ..bob(&mcp)
        });
        let answer = mcp.verify(Some(TOKEN)).await;
        assert!(
            matches!(answer, Err(Error::Unauthenticated(_))),
            "{answer:?}"
        );
    }

    // TC: MCPH-04 (refused before a user is created for the other subject)
    #[tokio::test]
    async fn creates_no_user_when_same_token_names_other_subject() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let _bob = mcp.verify(Some(TOKEN)).await.unwrap();
        mcp.verifies(OAuthIdentity {
            subject: "z2".to_owned(),
            login: Login::new("carol").unwrap(),
            ..bob(&mcp)
        });
        let _refused = mcp.verify(Some(TOKEN)).await;
        assert_eq!(
            mcp.world
                .ports
                .store
                .user_by_login(&Login::new("carol").unwrap())
                .await
                .unwrap(),
            None
        );
    }

    // TC: MCPH-04 (another user holds the login: as a login, 409)
    #[tokio::test]
    async fn conflict_when_login_held_by_other_user() {
        let mcp = world().await;
        mcp.verifies(OAuthIdentity {
            login: mcp.world.owner.login.clone(),
            ..bob(&mcp)
        });
        let answer = mcp.verify(Some(TOKEN)).await;
        assert!(matches!(answer, Err(Error::Conflict(_))), "{answer:?}");
    }

    // TC: MCPH-04 (the session's secret is keyed by the installation: it
    // is not the bearer itself)
    #[tokio::test]
    async fn derives_secret_other_than_token_when_session_opened() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let grant = mcp.verify(Some(TOKEN)).await.unwrap();
        assert_ne!(grant.token.expose(), TOKEN);
    }
}

mod rest_bearer {
    use super::*;

    // TC: MCPH-04 (the transport calls the API with the `mcp` session)
    #[tokio::test]
    async fn authenticates_mcp_session_when_mcp_installed() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let grant = mcp.verify(Some(TOKEN)).await.unwrap();
        let principal = mcp.authenticate(&grant.token).await;
        assert!(
            matches!(&principal, Ok(Principal::User { user, .. }) if user.id == grant.user.id),
            "{principal:?}"
        );
    }

    // TC: MCPH-06 (removing `mcp` ends its sessions at the REST API too)
    #[tokio::test]
    async fn unauthenticated_when_mcp_session_and_mcp_removed() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let grant = mcp.verify(Some(TOKEN)).await.unwrap();
        let _known = mcp
            .world
            .ports
            .modules
            .shared_registry()
            .remove(&name("mcp"));
        let principal = mcp.authenticate(&grant.token).await;
        assert!(
            matches!(principal, Err(Error::Unauthenticated(_))),
            "{principal:?}"
        );
    }

    // TC: MCPH-06 (a disabled `mcp` too)
    #[tokio::test]
    async fn unauthenticated_when_mcp_session_and_mcp_disabled() {
        let mcp = world().await;
        mcp.verifies(bob(&mcp));
        let grant = mcp.verify(Some(TOKEN)).await.unwrap();
        let _known = mcp
            .world
            .ports
            .modules
            .shared_registry()
            .disable(&name("mcp"), "trapped");
        let principal = mcp.authenticate(&grant.token).await;
        assert!(
            matches!(principal, Err(Error::Unauthenticated(_))),
            "{principal:?}"
        );
    }
}
