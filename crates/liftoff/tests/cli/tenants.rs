//! The commands of the `tenants` module ("the `tenants` module adds `lof tenant create|ls|use|rm|members …`, `lof protect|unprotect [--env]`, `lof admin tenant set`", and the commands of modules look like native ones) against the local stub of the API.
//!
//! A module command whose manifest name has several words (`tenant
//! members add`) is reached by those words at the top level of `lof`, as
//! a built-in command would be (also below a built-in word: `lof admin
//! tenant set`), and runs as `POST /api/v1/m/<module>/<words joined by
//! />` with the arguments as a JSON object, like every command of a
//! module (`crate::modules`, REG-08).

use serde_json::{Value, json};

use crate::fixtures::RESOLVED_PROJECT;
use crate::support::{Cli, Seen, Stub, json as json_reply};

/// `GET /modules` with `tenants` installed and its commands as its
/// manifest declares them.
const MODULES: &str = r#"[{"name":"tenants","version":"0.1.0","enabled":true,"error":null,"installed_at":"2026-10-19T10:00:00Z","commands":[
  {"name":"tenant create","description":"Creates a tenant","args":[{"name":"slug","type":"string","required":true,"hint":"Tenant slug"},{"name":"name","type":"string","required":false,"hint":null}]},
  {"name":"tenant members add","description":"Adds a member","args":[{"name":"tenant","type":"string","required":true,"hint":"Tenant slug"},{"name":"login","type":"string","required":true,"hint":"Login"},{"name":"owner","type":"boolean","required":false,"hint":null}]},
  {"name":"protect","description":"Puts an environment behind the gate","args":[{"name":"project","type":"project","required":true,"hint":"Project"},{"name":"env","type":"string","required":true,"hint":"Environment"}]},
  {"name":"unprotect","description":"Takes an environment from behind the gate","args":[{"name":"project","type":"project","required":true,"hint":"Project"},{"name":"env","type":"string","required":true,"hint":"Environment"}]},
  {"name":"admin tenant set","description":"Sets a tenant's quotas","args":[{"name":"tenant","type":"string","required":true,"hint":"Tenant slug"},{"name":"max_projects","type":"integer","required":false,"hint":null},{"name":"env_cpus","type":"string","required":false,"hint":null},{"name":"env_memory_mb","type":"integer","required":false,"hint":null}]}
],"routes":[],"tools":[],"config_schema":null}]"#;

/// A stub with `tenants` installed, each command's route answering `{}`.
async fn stub() -> Stub {
    let mut stub = Stub::start()
        .await
        .route("GET", "/modules", vec![json_reply(200, MODULES)]);
    for path in [
        "/m/tenants/tenant/create",
        "/m/tenants/tenant/members/add",
        "/m/tenants/protect",
        "/m/tenants/unprotect",
        "/m/tenants/admin/tenant/set",
    ] {
        stub = stub.route("POST", path, vec![json_reply(200, "{}")]);
    }
    stub
}

/// The exit code and the JSON bodies `POST <path>` received for `args`.
async fn sent(args: &[&str], path: &str) -> (u8, Vec<Value>) {
    let stub = stub().await;
    let cli = Cli::logged_in(&stub.url);
    let outcome = cli.run(args).await;
    let bodies = stub.seen_at("POST", path).iter().map(Seen::json).collect();
    (outcome.code, bodies)
}

mod protect {
    use super::*;

    // TC: TEN-06 (`lof protect --env prod`)
    #[tokio::test]
    async fn posts_project_and_env_when_protect_given() {
        assert_eq!(
            sent(
                &["protect", "--project", "blog", "--env", "prod"],
                "/m/tenants/protect"
            )
            .await,
            (0, vec![json!({"project": "blog", "env": "prod"})])
        );
    }

    // TC: TEN-06 (`lof protect --env prod`
    // in a project's directory: the project is the repository's, as the
    // server resolves it)
    #[tokio::test]
    async fn posts_resolved_project_when_protect_runs_in_linked_repo() {
        let stub = stub().await.route(
            "POST",
            "/projects/-/resolve",
            vec![json_reply(200, RESOLVED_PROJECT)],
        );
        let cli = Cli::logged_in(&stub.url).in_repo("git@git.example.com:acme/shop.git");
        let outcome = cli.run(&["protect", "--env", "prod"]).await;
        let bodies: Vec<Value> = stub
            .seen_at("POST", "/m/tenants/protect")
            .iter()
            .map(Seen::json)
            .collect();
        let resolved: Vec<Value> = stub
            .seen_at("POST", "/projects/-/resolve")
            .iter()
            .map(Seen::json)
            .collect();
        assert_eq!(
            (outcome.code, bodies, resolved),
            (
                0,
                vec![json!({"project": "web-app", "env": "prod"})],
                vec![json!({"git_host": "git.example.com", "repo": "acme/shop"})]
            ),
            "{outcome:?}"
        );
    }

    // TC: TEN-06 (the global `--project` names it too, as for built-in
    // commands)
    #[tokio::test]
    async fn posts_global_project_when_protect_given_project_before_command() {
        assert_eq!(
            sent(
                &["--project", "blog", "protect", "--env", "prod"],
                "/m/tenants/protect"
            )
            .await,
            (0, vec![json!({"project": "blog", "env": "prod"})])
        );
    }

    // TC: TEN-09
    #[tokio::test]
    async fn posts_project_and_env_when_unprotect_given() {
        assert_eq!(
            sent(
                &["unprotect", "--project", "blog", "--env", "prod"],
                "/m/tenants/unprotect"
            )
            .await,
            (0, vec![json!({"project": "blog", "env": "prod"})])
        );
    }

    // TC: TEN-06 (REG-08 rules for a top-level module command: a missing
    // required argument without a TTY → 64, nothing sent)
    #[tokio::test]
    async fn exits_64_listing_env_when_protect_env_missing_without_tty() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["protect", "--project", "blog"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.as_str(),
                stub.seen_at("POST", "/m/tenants/protect").len()
            ),
            (64, "error: missing required arguments: --env\n", 0),
            "{outcome:?}"
        );
    }
}

mod tenant {
    use super::*;

    // TC: TEN-03 (`lof tenant create`)
    #[tokio::test]
    async fn posts_slug_and_name_when_tenant_create_given() {
        assert_eq!(
            sent(
                &["tenant", "create", "--slug", "acme", "--name", "Acme"],
                "/m/tenants/tenant/create"
            )
            .await,
            (0, vec![json!({"slug": "acme", "name": "Acme"})])
        );
    }

    // TC: TEN-03 (`lof tenant members add`, three words)
    #[tokio::test]
    async fn posts_member_when_tenant_members_add_given() {
        assert_eq!(
            sent(
                &[
                    "tenant", "members", "add", "--tenant", "acme", "--login", "carol", "--owner"
                ],
                "/m/tenants/tenant/members/add"
            )
            .await,
            (
                0,
                vec![json!({"tenant": "acme", "login": "carol", "owner": true})]
            )
        );
    }
}

mod admin_tenant_set {
    use super::*;

    // TC: TEN-04, TEN-05 (`lof admin tenant set`: below the built-in
    // `admin`)
    #[tokio::test]
    async fn posts_quotas_when_admin_tenant_set_given() {
        assert_eq!(
            sent(
                &[
                    "admin",
                    "tenant",
                    "set",
                    "--tenant",
                    "acme",
                    "--max_projects",
                    "5",
                    "--env_cpus",
                    "0.5",
                    "--env_memory_mb",
                    "256"
                ],
                "/m/tenants/admin/tenant/set"
            )
            .await,
            (
                0,
                vec![json!({
                    "tenant": "acme",
                    "max_projects": 5,
                    "env_cpus": "0.5",
                    "env_memory_mb": 256
                })]
            )
        );
    }
}
