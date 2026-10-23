//! REST-08 and SEC-10: secrets (`/secrets`, `/secrets/{key}` with the scope
//! in `?scope=&id=`, `versions`, `rollback`, `sync`, `resolve`).

use http::StatusCode;
use liftoff_core::domain::{RepoRef, SecretKey, UserRole};
use serde_json::{Value, json};

use crate::support::{WorldCtx, detail, is_problem, strings};

const SHOP_SCOPE: &str = "scope=project&id=shop";

fn put(key: &str, value: &str) -> Value {
    json!({"scope": "project", "id": "shop", "key": key, "value": value})
}

mod access {
    use super::*;

    // TC: SEC-10
    #[tokio::test]
    async fn unauthorized_when_listed_without_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call("GET", &format!("/api/v1/secrets?{SHOP_SCOPE}"), None, None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            reply.text()
        );
    }

    // TC: SEC-10 (the hook point: the actor does not own the scope)
    #[tokio::test]
    async fn forbidden_without_the_value_when_actor_does_not_own_the_scope() {
        let ctx = WorldCtx::new().await;
        let carol = ctx.world.add_user("carol", UserRole::User).await.unwrap();
        let secret = ctx.world.login(&carol).await.unwrap();
        let reply = ctx
            .call(
                "GET",
                &format!("/api/v1/secrets/DATABASE_URL?{SHOP_SCOPE}"),
                Some(secret.expose()),
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                reply.text().contains("postgres://")
            ),
            (StatusCode::FORBIDDEN, true, false),
            "{}",
            reply.text()
        );
    }
}

mod list {
    use super::*;

    // TC: REST-08, SEC-04 (the REST half: no values)
    #[tokio::test]
    async fn keys_without_values_when_project_scope_listed() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", &format!("/api/v1/secrets?{SHOP_SCOPE}"), None)
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                strings(&body, "key"),
                strings(&body, "scope"),
                reply.text().contains("postgres://")
            ),
            (
                StatusCode::OK,
                vec!["DATABASE_URL".to_owned(), "LOG_LEVEL".to_owned()],
                vec!["project".to_owned(), "project".to_owned()],
                false
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-08
    #[tokio::test]
    async fn qa_keys_when_environment_scope_listed() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/secrets?scope=environment&id=shop/qa", None)
            .await;
        assert_eq!(
            (reply.status, strings(&reply.json(), "key")),
            (StatusCode::OK, vec!["DATABASE_URL".to_owned()]),
            "{}",
            reply.text()
        );
    }

    // TC: REST-08
    #[tokio::test]
    async fn not_found_when_project_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/secrets?scope=project&id=nope", None)
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("nope")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-08, REST-10, TEN-11 (a project outside every tenant, i.e. no `tenants` module → 424)
    #[tokio::test]
    async fn module_required_naming_tenants_when_project_has_no_tenant() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/secrets?scope=tenant&id=shop", None)
            .await;
        assert_eq!(
            (reply.status.as_u16(), detail(&reply).contains("tenants")),
            (424, true),
            "{}",
            reply.text()
        );
    }
}

// TC: REST-08, TEN-11 (`scope=tenant&id=<project>` is the
// tenant that project is in)
#[tokio::test]
async fn writes_and_lists_tenant_scope_when_project_is_in_a_tenant() {
    use liftoff_core::domain::TenantId;
    use liftoff_core::ports::Store as _;

    let ctx = WorldCtx::new().await;
    let mut shop = ctx.world.shop.clone();
    shop.tenant_id = Some(TenantId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap());
    ctx.world.ports.store.update_project(&shop).await.unwrap();
    let body = json!({"scope": "tenant", "id": "shop", "key": "REGION", "value": "eu"});
    let put = ctx.owner("PUT", "/api/v1/secrets", Some(&body)).await;
    let listed = ctx
        .owner("GET", "/api/v1/secrets?scope=tenant&id=shop", None)
        .await;
    assert_eq!(
        (
            put.status,
            put.json()["scope"].clone(),
            strings(&listed.json(), "key")
        ),
        (StatusCode::OK, json!("tenant"), vec!["REGION".to_owned()]),
        "{} {}",
        put.text(),
        listed.text()
    );
}

mod get {
    use super::*;

    // TC: REST-08, SEC-04 (the REST half: `get` returns the current value)
    #[tokio::test]
    async fn current_value_when_owner_gets_it() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "GET",
                &format!("/api/v1/secrets/DATABASE_URL?{SHOP_SCOPE}"),
                None,
            )
            .await;
        assert_eq!(
            (reply.status, reply.json()),
            (
                StatusCode::OK,
                json!({"key": "DATABASE_URL", "value": "postgres://db/shop", "version": 1})
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-08
    #[tokio::test]
    async fn not_found_naming_the_key_when_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", &format!("/api/v1/secrets/NOPE?{SHOP_SCOPE}"), None)
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("NOPE")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }
}

mod set {
    use super::*;

    // TC: REST-08
    #[tokio::test]
    async fn version_two_when_existing_key_set() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("PUT", "/api/v1/secrets", Some(&put("LOG_LEVEL", "debug")))
            .await;
        let value = ctx
            .owner(
                "GET",
                &format!("/api/v1/secrets/LOG_LEVEL?{SHOP_SCOPE}"),
                None,
            )
            .await
            .json();
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["key"].as_str(),
                body["version"].as_u64(),
                body["ci"].as_bool(),
                reply.text().contains("debug"),
                value["value"].as_str()
            ),
            (
                StatusCode::OK,
                Some("LOG_LEVEL"),
                Some(2),
                Some(false),
                false,
                Some("debug")
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-08 (validate at the boundary: the secret key regex)
    #[tokio::test]
    async fn bad_request_naming_the_key_when_key_is_lowercase() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("PUT", "/api/v1/secrets", Some(&put("log_level", "debug")))
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("key")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-08 (`deny_unknown_fields`)
    #[tokio::test]
    async fn bad_request_when_body_has_unknown_field() {
        let ctx = WorldCtx::new().await;
        let mut body = put("LOG_LEVEL", "debug");
        body["tenant"] = json!("acme");
        let reply = ctx.owner("PUT", "/api/v1/secrets", Some(&body)).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }
}

mod delete {
    use super::*;

    // TC: REST-08
    #[tokio::test]
    async fn deleted_and_gone_when_key_exists() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "DELETE",
                &format!("/api/v1/secrets/LOG_LEVEL?{SHOP_SCOPE}"),
                None,
            )
            .await;
        let again = ctx
            .owner(
                "GET",
                &format!("/api/v1/secrets/LOG_LEVEL?{SHOP_SCOPE}"),
                None,
            )
            .await;
        assert_eq!(
            (reply.status, reply.json(), again.status),
            (
                StatusCode::OK,
                json!({"deleted": "LOG_LEVEL"}),
                StatusCode::NOT_FOUND
            ),
            "{}",
            reply.text()
        );
    }
}

mod versions {
    use super::*;

    // TC: REST-08
    #[tokio::test]
    async fn newest_first_without_values_when_set_twice() {
        let ctx = WorldCtx::new().await;
        let _set = ctx
            .owner("PUT", "/api/v1/secrets", Some(&put("LOG_LEVEL", "debug")))
            .await;
        let reply = ctx
            .owner(
                "GET",
                &format!("/api/v1/secrets/LOG_LEVEL/versions?{SHOP_SCOPE}"),
                None,
            )
            .await;
        let versions: Vec<u64> = reply
            .json()
            .as_array()
            .map(|items| items.iter().filter_map(|v| v["version"].as_u64()).collect())
            .unwrap_or_default();
        assert_eq!(
            (
                reply.status,
                versions,
                reply.text().contains("debug") || reply.text().contains("info")
            ),
            (StatusCode::OK, vec![2, 1], false),
            "{}",
            reply.text()
        );
    }

    // TC: REST-08, SEC-05 (the REST half)
    #[tokio::test]
    async fn version_three_with_value_of_one_when_rolled_back_to_one() {
        let ctx = WorldCtx::new().await;
        let _set = ctx
            .owner("PUT", "/api/v1/secrets", Some(&put("LOG_LEVEL", "debug")))
            .await;
        let reply = ctx
            .owner(
                "POST",
                &format!("/api/v1/secrets/LOG_LEVEL/rollback?{SHOP_SCOPE}"),
                Some(&json!({"to": 1})),
            )
            .await;
        let value = ctx
            .owner(
                "GET",
                &format!("/api/v1/secrets/LOG_LEVEL?{SHOP_SCOPE}"),
                None,
            )
            .await
            .json();
        assert_eq!(
            (
                reply.status,
                reply.json()["version"].as_u64(),
                value["value"].as_str()
            ),
            (StatusCode::OK, Some(3), Some("info")),
            "{}",
            reply.text()
        );
    }

    // TC: REST-08
    #[tokio::test]
    async fn not_found_when_rolled_back_to_unknown_version() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                &format!("/api/v1/secrets/LOG_LEVEL/rollback?{SHOP_SCOPE}"),
                Some(&json!({"to": 9})),
            )
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("version 9")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }
}

mod resolve {
    use super::*;

    /// The value of `key` in a `ResolvedEnv` body.
    fn var<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
        body["vars"].as_array()?.iter().find(|v| v["key"] == key)?["value"].as_str()
    }

    // TC: REST-08, SEC-06 (the REST half), CLIX-07 (the server half)
    #[tokio::test]
    async fn qa_overrides_project_and_system_vars_added_when_resolved() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/secrets/resolve?project=shop&env=qa", None)
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                var(&body, "DATABASE_URL"),
                var(&body, "LOG_LEVEL"),
                var(&body, "LIFTOFF_PROJECT"),
                var(&body, "LIFTOFF_ENV")
            ),
            (
                StatusCode::OK,
                Some("postgres://db/shop_qa"),
                Some("info"),
                Some("shop"),
                Some("qa")
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-08
    #[tokio::test]
    async fn not_found_when_environment_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/secrets/resolve?project=shop&env=nope", None)
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("nope")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-08
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call(
                "GET",
                "/api/v1/secrets/resolve?project=shop&env=qa",
                None,
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }
}

mod sync {
    use liftoff_core::error::{Error, System};

    use super::*;

    /// Stores the project secret `key = value` of `shop` with `ci = true`.
    async fn put_ci(ctx: &WorldCtx, key: &str, value: &str) {
        let mut body = put(key, value);
        body["ci"] = json!(true);
        let reply = ctx.owner("PUT", "/api/v1/secrets", Some(&body)).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    }

    async fn sync(ctx: &WorldCtx) -> crate::support::Reply {
        ctx.owner(
            "POST",
            "/api/v1/secrets/sync",
            Some(&json!({"project": "shop"})),
        )
        .await
    }

    // TC: SYNC-01, REST-08 (only `ci = true` goes to the forge)
    #[tokio::test]
    async fn only_ci_secrets_sent_to_the_repository_when_synced() {
        let ctx = WorldCtx::new().await;
        put_ci(&ctx, "REGISTRY_PASSWORD", "hunter2").await;
        let reply = sync(&ctx).await;
        let git = &ctx.world.ports.git;
        let repo = RepoRef::new("owner/shop").unwrap();
        let sent = git
            .ci_secret(&repo, &SecretKey::new("REGISTRY_PASSWORD").unwrap())
            .map(|value| value.expose().to_owned());
        let leaked = git.ci_secret(&repo, &SecretKey::new("DATABASE_URL").unwrap());
        assert_eq!(
            (reply.status, reply.json(), sent, leaked.is_none()),
            (
                StatusCode::OK,
                json!({"synced": ["REGISTRY_PASSWORD"], "unchanged": [], "failed": []}),
                Some("hunter2".to_owned()),
                true
            ),
            "{}",
            reply.text()
        );
    }

    // TC: SYNC-02 (a forge error is reported per key; the rest is sent)
    #[tokio::test]
    async fn reports_failed_key_and_sends_the_rest_when_forge_fails_on_one() {
        let ctx = WorldCtx::new().await;
        put_ci(&ctx, "A_TOKEN", "a").await;
        put_ci(&ctx, "B_TOKEN", "b").await;
        ctx.world.ports.git.fail_next(
            "put_ci_secret",
            Error::upstream(System::Forge, "502 Bad Gateway"),
        );
        let reply = sync(&ctx).await;
        assert_eq!(
            (reply.status, reply.json()),
            (
                StatusCode::OK,
                json!({
                    "synced": ["B_TOKEN"],
                    "unchanged": [],
                    "failed": [{"key": "A_TOKEN", "error": "forge: 502 Bad Gateway"}]
                })
            ),
            "{}",
            reply.text()
        );
    }

    // TC: SYNC-03
    #[tokio::test]
    async fn nothing_sent_when_synced_again_without_change() {
        let ctx = WorldCtx::new().await;
        put_ci(&ctx, "REGISTRY_PASSWORD", "hunter2").await;
        let _first = sync(&ctx).await;
        let second = sync(&ctx).await;
        assert_eq!(
            (second.status, second.json()),
            (
                StatusCode::OK,
                json!({"synced": [], "unchanged": ["REGISTRY_PASSWORD"], "failed": []})
            ),
            "{}",
            second.text()
        );
    }

    // TC: SYNC-04 (`lof secret set --ci` on an existing key)
    #[tokio::test]
    async fn existing_secret_sent_when_set_with_ci_after_sync() {
        let ctx = WorldCtx::new().await;
        let _first = sync(&ctx).await;
        put_ci(&ctx, "LOG_LEVEL", "debug").await;
        let second = sync(&ctx).await;
        assert_eq!(
            (second.status, second.json()),
            (
                StatusCode::OK,
                json!({"synced": ["LOG_LEVEL"], "unchanged": [], "failed": []})
            ),
            "{}",
            second.text()
        );
    }

    // TC: SYNC-01 (single-user mode: only the admin owns the project)
    #[tokio::test]
    async fn forbidden_and_nothing_sent_when_actor_does_not_own_the_project() {
        let ctx = WorldCtx::new().await;
        put_ci(&ctx, "REGISTRY_PASSWORD", "hunter2").await;
        let carol = ctx.world.add_user("carol", UserRole::User).await.unwrap();
        let secret = ctx.world.login(&carol).await.unwrap();
        let reply = ctx
            .call(
                "POST",
                "/api/v1/secrets/sync",
                Some(secret.expose()),
                Some(&json!({"project": "shop"})),
            )
            .await;
        let sent = ctx.world.ports.git.ci_secret(
            &RepoRef::new("owner/shop").unwrap(),
            &SecretKey::new("REGISTRY_PASSWORD").unwrap(),
        );
        assert_eq!(
            (reply.status, is_problem(&reply), sent.is_none()),
            (StatusCode::FORBIDDEN, true, true),
            "{}",
            reply.text()
        );
    }
}
