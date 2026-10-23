//! `POST /git/create-plan`: the plan of
//! `lof create` in server mode, built and validated by the server
//! (`CRE-01`, `CRE-02`, `CRE-09`, `CRE-10` through the router), and the
//! `template` of `POST /projects` its `create_project` step carries;
//! `POST /git/generate`, its `generate_repo` step run by the
//! server.

use http::StatusCode;
use liftoff_core::domain::{Host, RepoRef, TemplateRef, TenantId};
use liftoff_core::fakes::World;
use liftoff_core::ports::{RepoRole, Rng, Store as _};
use serde_json::{Value, json};

use crate::support::{WorldCtx, is_problem};

const PLAN: &str = "/api/v1/git/create-plan";

/// The `liftoff.toml` of a template with one generated secret.
const APP_TEMPLATE: &str = "[template]\ngroup = \"web\"\nname = \"acme-app\"\ndescription = \"SolidJS\"\n\n[service]\ncompose = \"deploy/compose.yaml\"\nexpose = \"web:3000\"\n\n[secrets]\nSESSION_SECRET = { generate = \"random:32\" }\n\n[substitute]\nfiles = [\"README.md\"]\n";

/// A repository's manifest without `[template]` or `[secrets]`.
const SITE: &str = "[service]\ncompose = \"deploy/compose.yaml\"\nexpose = \"web:3000\"\n";

fn repo(raw: &str) -> RepoRef {
    RepoRef::new(raw).unwrap()
}

/// A world where `owner` reads the template `acme/app`.
async fn with_template() -> WorldCtx {
    let ctx = WorldCtx::new().await;
    ctx.world
        .ports
        .git
        .template(&repo("acme/app"), APP_TEMPLATE);
    ctx.world
        .ports
        .git
        .grant(&ctx.world.owner.login, &repo("acme/app"), RepoRole::Read);
    ctx
}

/// `(side, step)` of every step of a plan.
fn steps(plan: &Value) -> Vec<(String, String)> {
    plan["steps"]
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .map(|step| {
                    (
                        step["side"].as_str().unwrap_or_default().to_owned(),
                        step["step"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The step `name` of a plan.
fn step(plan: &Value, name: &str) -> Value {
    plan["steps"]
        .as_array()
        .and_then(|steps| steps.iter().find(|step| step["step"] == name))
        .cloned()
        .unwrap_or(Value::Null)
}

/// The `tenants`-like answer of the next `project_tenant`: the tenant the
/// request names exists and the caller may create projects in it.
fn admit_tenant(ctx: &WorldCtx) {
    let tenant = TenantId::new(Rng::ulid(&ctx.world.ports.rng));
    ctx.world
        .ports
        .hooks
        .answer_project_tenant(Ok(Some(tenant)));
}

fn pairs(raw: &[(&str, &str)]) -> Vec<(String, String)> {
    raw.iter()
        .map(|(side, step)| ((*side).to_owned(), (*step).to_owned()))
        .collect()
}

mod from_template {
    use super::*;

    // TC: CRE-01
    #[tokio::test]
    async fn every_step_in_spec_order_when_owner_plans_from_template() {
        let ctx = with_template().await;
        let reply = ctx
            .owner(
                "POST",
                PLAN,
                Some(&json!({"name": "blog", "template": "git.example.com/acme/app"})),
            )
            .await;
        let plan = reply.json();
        assert_eq!(
            (
                reply.status,
                plan["repo"].clone(),
                plan["domain"].clone(),
                steps(&plan)
            ),
            (
                StatusCode::OK,
                json!("owner/blog"),
                json!("blog.apps.test"),
                pairs(&[
                    ("forge", "generate_repo"),
                    ("local", "clone"),
                    ("local", "substitute"),
                    ("local", "ask_secrets"),
                    ("local", "write_ci_snippet"),
                    ("local", "commit"),
                    ("local", "push"),
                    ("server", "create_project"),
                    ("server", "add_environment"),
                    ("server", "add_domain"),
                    ("server", "bind"),
                    ("server", "issue_token"),
                    ("server", "store_secrets"),
                    ("server", "sync_ci"),
                    ("local", "write_liftoff_file"),
                ])
            ),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-01 (each step carries what its runner needs)
    #[tokio::test]
    async fn steps_carry_their_data_when_owner_plans_from_template() {
        let ctx = with_template().await;
        let reply = ctx
            .owner(
                "POST",
                PLAN,
                Some(&json!({"name": "blog", "template": "git.example.com/acme/app", "public": true})),
            )
            .await;
        let plan = reply.json();
        assert_eq!(
            (
                step(&plan, "generate_repo"),
                step(&plan, "ask_secrets"),
                step(&plan, "create_project"),
                step(&plan, "bind"),
                step(&plan, "write_ci_snippet")["path"].clone(),
            ),
            (
                json!({"side": "forge", "step": "generate_repo", "template": "acme/app",
                       "repo": "owner/blog", "private": false}),
                json!({"side": "local", "step": "ask_secrets", "secrets": [
                    {"key": "SESSION_SECRET", "hint": null, "generate": "random:32",
                     "scope": "project", "ci": false},
                    {"key": "REGISTRY_USER",
                     "hint": "the git.example.com account the workflow pushes the image as, e.g. owner",
                     "generate": null, "scope": "project", "ci": true},
                    {"key": "REGISTRY_TOKEN",
                     "hint": "a token of that account with only the package scope (write:package), not the `lof git connect` token",
                     "generate": null, "scope": "project", "ci": true}
                ]}),
                json!({"side": "server", "step": "create_project", "project": {
                    "name": "blog", "git_host": "git.example.com", "repo": "owner/blog",
                    "manifest": APP_TEMPLATE,
                    "template": {"group": "web", "name": "acme-app"}
                }}),
                json!({"side": "server", "step": "bind", "env": "prod", "service": "web",
                       "port": 3000, "host": "blog.apps.test"}),
                json!(".forgejo/workflows/deploy.yml"),
            ),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-17 (the request's `tenant`, admitted by the module, is the
    // `tenant` of the `create_project` step: `POST /projects {tenant}`)
    #[tokio::test]
    async fn carries_tenant_into_create_project_when_body_names_one() {
        let ctx = with_template().await;
        admit_tenant(&ctx);
        let reply = ctx
            .owner(
                "POST",
                PLAN,
                Some(&json!({"name": "blog", "template": "git.example.com/acme/app", "tenant": "acme"})),
            )
            .await;
        assert_eq!(
            (
                reply.status,
                step(&reply.json(), "create_project")["project"]["tenant"].clone()
            ),
            (StatusCode::OK, json!("acme")),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-02
    #[tokio::test]
    async fn conflict_when_name_is_taken() {
        let ctx = with_template().await;
        let reply = ctx
            .owner(
                "POST",
                PLAN,
                Some(&json!({"name": "shop", "template": "git.example.com/acme/app"})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::CONFLICT, true),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-01 (exactly one of `template` and `existing`)
    #[tokio::test]
    async fn bad_request_when_neither_template_nor_existing() {
        let ctx = with_template().await;
        let reply = ctx
            .owner("POST", PLAN, Some(&json!({"name": "blog"})))
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-01
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = with_template().await;
        let reply = ctx
            .call(
                "POST",
                PLAN,
                None,
                Some(&json!({"name": "blog", "template": "acme/app"})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }
}

mod for_existing {
    use super::*;

    // TC: CRE-09
    #[tokio::test]
    async fn writes_manifest_first_when_it_was_detected() {
        let ctx = WorldCtx::new().await;
        ctx.world
            .ports
            .git
            .grant(&ctx.world.owner.login, &repo("owner/site"), RepoRole::Admin);
        let reply = ctx
            .owner(
                "POST",
                PLAN,
                Some(&json!({"name": "site", "existing": {
                    "git_host": "git.example.com", "repo": "owner/site",
                    "manifest": SITE, "detected": true
                }})),
            )
            .await;
        let plan = reply.json();
        assert_eq!(
            (
                reply.status,
                steps(&plan).first().cloned(),
                step(&plan, "write_manifest")["content"].clone()
            ),
            (
                StatusCode::OK,
                Some(("local".to_owned(), "write_manifest".to_owned())),
                json!(SITE)
            ),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-17 (an existing repository's plan carries the tenant too)
    #[tokio::test]
    async fn carries_tenant_into_create_project_when_existing_repo_names_one() {
        let ctx = WorldCtx::new().await;
        admit_tenant(&ctx);
        ctx.world
            .ports
            .git
            .grant(&ctx.world.owner.login, &repo("owner/site"), RepoRole::Admin);
        let reply = ctx
            .owner(
                "POST",
                PLAN,
                Some(&json!({"name": "site", "tenant": "acme", "existing": {
                    "git_host": "git.example.com", "repo": "owner/site",
                    "manifest": SITE, "detected": false
                }})),
            )
            .await;
        assert_eq!(
            (
                reply.status,
                step(&reply.json(), "create_project")["project"]["tenant"].clone()
            ),
            (StatusCode::OK, json!("acme")),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-10
    #[tokio::test]
    async fn conflict_naming_the_project_when_repo_already_is_one() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                PLAN,
                Some(&json!({"name": "shop2", "existing": {
                    "git_host": "git.example.com", "repo": "owner/shop", "manifest": SITE
                }})),
            )
            .await;
        assert_eq!(
            (
                reply.status,
                reply.json()["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("project shop"))
            ),
            (StatusCode::CONFLICT, true),
            "{}",
            reply.text()
        );
    }
}

mod create_project {
    use super::*;

    // TC: CRE-01 (`create_project` records the template)
    #[tokio::test]
    async fn records_template_when_body_names_one() {
        let ctx = WorldCtx::new().await;
        ctx.world
            .ports
            .git
            .grant(&ctx.world.owner.login, &repo("owner/blog"), RepoRole::Admin);
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/projects",
                Some(
                    &json!({"name": "blog", "git_host": "git.example.com", "repo": "owner/blog",
                             "template": {"group": "web", "name": "acme-app"}}),
                ),
            )
            .await;
        let stored = ctx
            .world
            .ports
            .store
            .project_by_repo(&Host::new(World::GIT_HOST).unwrap(), &repo("owner/blog"))
            .await
            .unwrap()
            .and_then(|project| project.template);
        assert_eq!(
            (reply.status, stored),
            (
                StatusCode::CREATED,
                Some(TemplateRef {
                    group: "web".to_owned(),
                    name: "acme-app".to_owned(),
                })
            ),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-01 (the group is one of the catalogue's)
    #[tokio::test]
    async fn bad_request_when_template_group_is_unknown() {
        let ctx = WorldCtx::new().await;
        ctx.world
            .ports
            .git
            .grant(&ctx.world.owner.login, &repo("owner/blog"), RepoRole::Admin);
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/projects",
                Some(
                    &json!({"name": "blog", "git_host": "git.example.com", "repo": "owner/blog",
                             "template": {"group": "games", "name": "acme-app"}}),
                ),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }
}

/// `POST /git/generate`: the
/// forge step of a template's plan, run by the server through the
/// caller's connection there, for the CLI's `lof create`.
mod generate {
    use liftoff_core::fakes::GitCall;

    use super::*;

    const GENERATE: &str = "/api/v1/git/generate";

    /// The forge's `generate_from_template` calls of `ctx`.
    fn generated(ctx: &WorldCtx) -> Vec<GitCall> {
        ctx.world
            .ports
            .git
            .calls()
            .into_iter()
            .filter(|call| matches!(call, GitCall::GenerateFromTemplate { .. }))
            .collect()
    }

    // TC: CRE-12 (the repository of the plan, generated from its template
    // as the caller)
    #[tokio::test]
    async fn created_with_repository_when_template_planned() {
        let ctx = with_template().await;
        let reply = ctx
            .owner(
                "POST",
                GENERATE,
                Some(&json!({"name": "blog", "template": "git.example.com/acme/app"})),
            )
            .await;
        assert_eq!(
            (reply.status, reply.json(), generated(&ctx)),
            (
                StatusCode::CREATED,
                json!({"git_host": "git.example.com", "repo": "owner/blog"}),
                vec![GitCall::GenerateFromTemplate {
                    login: ctx.world.owner.login.clone(),
                    template: repo("acme/app"),
                    target: "owner/blog".to_owned(),
                }]
            ),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-02 (planned again: a taken name generates nothing)
    #[tokio::test]
    async fn conflict_without_generating_when_name_is_taken() {
        let ctx = with_template().await;
        let reply = ctx
            .owner(
                "POST",
                GENERATE,
                Some(&json!({"name": "shop", "template": "git.example.com/acme/app"})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), generated(&ctx)),
            (StatusCode::CONFLICT, true, vec![]),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-09 (an existing repository has nothing to generate)
    #[tokio::test]
    async fn bad_request_when_body_names_existing_repository() {
        let ctx = with_template().await;
        let reply = ctx
            .owner(
                "POST",
                GENERATE,
                Some(&json!({"name": "site", "existing": {
                    "git_host": "git.example.com", "repo": "owner/site", "manifest": SITE,
                }})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), generated(&ctx)),
            (StatusCode::BAD_REQUEST, true, vec![]),
            "{}",
            reply.text()
        );
    }

    // TC: CRE-12
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = with_template().await;
        let reply = ctx
            .call(
                "POST",
                GENERATE,
                None,
                Some(&json!({"name": "blog", "template": "acme/app"})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }
}
