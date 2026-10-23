//! `lof create` with a server: CRE-11…16 in server mode, the CLI side of CRE-07 (another domain
//! when it is taken) and CRE-09 (an existing repository's detected
//! manifest is confirmed and committed), and TPL-06 for the catalogue
//! `create` offers.
//!
//! The server is the recording stub. It answers the catalogue (`GET
//! /git/templates`), the plan (`POST /git/create-plan`, as
//! `CreateService::plan_from_template` builds it for [`MANIFEST`]), the
//! repository its forge generated (`POST /git/generate`, the same body as the plan's: the forge connection lives on the server) and
//! every server step of the plan. The "forge" behind the repository is a
//! bare repository under a tempdir ([`GitHome`]) that `https://github.com/`
//! is rewritten to: the CLI's real `git` clones, commits and pushes there.
//!
//! The interactive path is, in order: the
//! group (a list of the catalogue's groups), the template of that group
//! (a list showing each template's name and description), the project
//! name (an empty answer takes the template's name), the visibility (a
//! list: `private`, `public`), the owner (an empty answer is the
//! person's own forge account), the values of the `[secrets]` without
//! `generate` (without echo; `generate` is silent), and a yes to the plan.

use liftoff::local::{Local, RegistryKey};
use serde_json::{Value, json};

use crate::local::repo::GitHome;
use crate::support::{Cli, Outcome, Reply, Seen, Stub, json as reply, problem, say};

/// The template `create` is given or picks: the catalogue entry's host
/// and repository.
pub(crate) const TEMPLATE: &str = "github.com/acme/app";

/// `owner/name` of the repository the forge generates for `demo`.
pub(crate) const REPO: &str = "owner/demo";

/// The domain of `demo`.
const HOST: &str = "demo.apps.example.com";

/// What the person types for `DATABASE_URL`.
pub(crate) const DATABASE_URL: &str = "postgres://db/demo";

/// The template's `liftoff.toml`: a generated secret, one
/// without `generate` in the environment, `{{project}}`/`{{domain}}` in
/// `README.md`.
const MANIFEST: &str = r#"[template]
group = "web"
name = "acme-app"
description = "SolidJS + acme-ui, Vite, Bun"

[service]
compose = "deploy/compose.yaml"
expose = "web:3000"

[secrets]
SESSION_SECRET = { hint = "32+ random bytes", generate = "random:32" }
DATABASE_URL = { hint = "postgres://…", scope = "environment" }

[substitute]
files = ["README.md"]
"#;

/// The template's files besides `liftoff.toml`.
const README: &str = "# {{project}} at https://{{domain}}\n";
const GITIGNORE: &str = "node_modules\n";
const COMPOSE: &str = "services:\n  web:\n    image: ghcr.io/owner/demo:latest\n";

/// The CI workflow of the plan.
const SNIPPET_PATH: &str = ".github/workflows/deploy.yml";
const SNIPPET: &str = "name: deploy\non: push\n";

/// `GET /git/templates`: two groups, `web` with two templates by name.
pub(crate) const CATALOGUE: &str = r#"{"groups":[{"group":"web","templates":[{"host":"github.com","repo":"acme/astro-site","name":"astro-site","description":"Astro, static pages","kind":"service"},{"host":"github.com","repo":"acme/app","name":"acme-app","description":"SolidJS + acme-ui, Vite, Bun","kind":"service"}]},{"group":"api","templates":[{"host":"github.com","repo":"acme/hono-api","name":"hono-api","description":"Hono on Bun","kind":"service"}]}],"warnings":[]}"#;

/// `POST /git/create-plan` for `demo` from [`TEMPLATE`]: every step of
/// as the core orders them (CRE-01).
pub(crate) fn plan() -> Value {
    json!({
        "project": "demo",
        "git_host": "github.com",
        "repo": REPO,
        "domain": HOST,
        "steps": [
            {"side": "forge", "step": "generate_repo", "template": "acme/app", "repo": REPO, "private": true},
            {"side": "local", "step": "clone"},
            {"side": "local", "step": "substitute", "files": ["README.md"], "project": "demo", "domain": HOST},
            {"side": "local", "step": "ask_secrets", "secrets": [
                {"key": "SESSION_SECRET", "hint": "32+ random bytes", "generate": "random:32", "scope": "project", "ci": false},
                {"key": "DATABASE_URL", "hint": "postgres://…", "generate": null, "scope": "environment", "ci": false},
            ]},
            {"side": "local", "step": "write_ci_snippet", "path": SNIPPET_PATH, "content": SNIPPET},
            {"side": "local", "step": "commit", "message": "chore: set up liftoff"},
            {"side": "local", "step": "push"},
            {"side": "server", "step": "create_project", "project": {
                "name": "demo", "git_host": "github.com", "repo": REPO, "manifest": MANIFEST,
                "template": {"group": "web", "name": "acme-app"},
            }},
            {"side": "server", "step": "add_environment", "name": "prod", "kind": "compose"},
            {"side": "server", "step": "add_domain", "host": HOST, "label": "demo"},
            {"side": "server", "step": "bind", "env": "prod", "service": "web", "port": 3000, "host": HOST},
            {"side": "server", "step": "issue_token"},
            {"side": "server", "step": "store_secrets", "secrets": [
                {"key": "SESSION_SECRET", "scope": "project", "ci": false},
                {"key": "DATABASE_URL", "scope": "environment", "ci": false},
            ]},
            {"side": "server", "step": "sync_ci"},
            {"side": "local", "step": "write_liftoff_file"},
        ],
    })
}

const GENERATED: &str = r#"{"git_host":"github.com","repo":"owner/demo"}"#;
const PROJECT: &str = r#"{"id":"01J000000000000000000000P3","name":"demo","git_host":"github.com","repo":"owner/demo","manifest":null,"created_by":"u1","created_at":"2026-10-20T10:00:00Z"}"#;
const ENV: &str = r#"{"id":"01J000000000000000000000E3","project":"demo","name":"prod","kind":"compose","branch":"main","pinned_by":"u1","state":"stopped","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"public","created_at":"2026-10-20T10:00:00Z"}"#;
const DOMAIN: &str = r#"{"host":"demo.apps.example.com","kind":"managed","project":"demo","verified_at":"2026-10-20T10:00:00Z","record_lost_at":null,"proxied":false}"#;
const BINDING: &str = r#"{"host":"demo.apps.example.com","project":"demo","environment":"prod","service":"web","container_port":3000,"host_port":20001}"#;
const TOKEN: &str =
    r#"{"project":"demo","id":"01J000000000000000000000T3","created_at":"2026-10-20T10:00:00Z"}"#;
const SECRET: &str = r#"{"key":"SESSION_SECRET","scope":"project","version":1,"ci":false,"updated_by":"u1","updated_at":"2026-10-20T10:00:00Z"}"#;
const SYNCED: &str = r#"{"synced":["LIFTOFF_TOKEN"],"unchanged":[],"failed":[]}"#;

/// The server steps of `demo`'s plan, answered.
fn with_server_steps(stub: Stub) -> Stub {
    stub.route("POST", "/git/generate", vec![reply(201, GENERATED)])
        .route("POST", "/projects", vec![reply(201, PROJECT)])
        .route("POST", "/projects/demo/environments", vec![reply(201, ENV)])
        .route("POST", "/domains", vec![reply(201, DOMAIN)])
        .route(
            "POST",
            "/projects/demo/environments/prod/bindings",
            vec![reply(201, BINDING)],
        )
        .route(
            "POST",
            "/projects/demo/token/rotate",
            vec![reply(201, TOKEN)],
        )
        .route("PUT", "/secrets", vec![reply(200, SECRET)])
        .route("POST", "/secrets/sync", vec![reply(200, SYNCED)])
}

/// The server: the catalogue, `plans` in order to `POST
/// /git/create-plan` (the last repeats), and the server steps.
pub(crate) async fn server_planning(plans: Vec<crate::support::Answer>) -> Stub {
    with_server_steps(
        Stub::start()
            .await
            .route("GET", "/git/templates", vec![reply(200, CATALOGUE)])
            .route("POST", "/git/create-plan", plans),
    )
}

/// The server answering [`plan`].
pub(crate) async fn server() -> Stub {
    server_planning(vec![reply(200, &plan().to_string())]).await
}

/// Local settings that run `git` against `git`'s bare repositories only.
pub(crate) fn offline_git(git: &GitHome) -> Local {
    git.env().into_iter().fold(
        Local::new("http://127.0.0.1:9/index.json", RegistryKey::new("")),
        |local, (key, value)| local.with_git_env(key, value),
    )
}

/// A machine logged in to `server`, outside any repository, whose forge
/// holds what it generates for `demo` from the template; the git home
/// lives as long as the CLI.
pub(crate) fn machine(server: &Stub) -> (Cli, GitHome) {
    let git = GitHome::new();
    git.seed_remote(
        REPO,
        &[
            ("liftoff.toml", MANIFEST),
            ("README.md", README),
            (".gitignore", GITIGNORE),
            ("deploy/compose.yaml", COMPOSE),
        ],
    );
    let cli = Cli::new().local(offline_git(&git));
    cli.log_in(&server.url);
    (cli, git)
}

/// [`machine`] on a terminal whose user answers `replies`.
fn interactive(server: &Stub, replies: Vec<Reply>) -> (Cli, GitHome) {
    let git = GitHome::new();
    git.seed_remote(
        REPO,
        &[
            ("liftoff.toml", MANIFEST),
            ("README.md", README),
            (".gitignore", GITIGNORE),
            ("deploy/compose.yaml", COMPOSE),
        ],
    );
    let cli = Cli::new().local(offline_git(&git)).terminal(replies);
    cli.log_in(&server.url);
    (cli, git)
}

/// The answers of the whole interactive path for `demo` from `acme-app`,
/// private under the person's account.
fn wizard() -> Vec<Reply> {
    vec![
        Reply::Pick(0),
        Reply::Pick(1),
        say("demo"),
        Reply::Pick(0),
        say(""),
        say(DATABASE_URL),
        Reply::Yes,
    ]
}

/// The batch form of [`wizard`].
pub(crate) const BATCH: &[&str] = &[
    "create",
    "demo",
    "--template",
    TEMPLATE,
    "--private",
    "--secret",
    "DATABASE_URL=postgres://db/demo",
];

/// The JSON bodies `server` saw at `method path`.
pub(crate) fn bodies(server: &Stub, method: &str, path: &str) -> Vec<Value> {
    server
        .seen_at(method, path)
        .iter()
        .map(Seen::json)
        .collect()
}

/// `(method, path)` of a trail, as literals.
fn trail(steps: &[(&str, &str)]) -> Vec<(String, String)> {
    steps
        .iter()
        .map(|(method, path)| ((*method).to_owned(), (*path).to_owned()))
        .collect()
}

/// What a batch `create` from a template sends after the plan, in order.
const RUN: &[(&str, &str)] = &[
    ("POST", "/git/create-plan"),
    ("POST", "/git/generate"),
    ("POST", "/projects"),
    ("POST", "/projects/demo/environments"),
    ("POST", "/domains"),
    ("POST", "/projects/demo/environments/prod/bindings"),
    ("POST", "/projects/demo/token/rotate"),
    ("PUT", "/secrets"),
    ("PUT", "/secrets"),
    ("POST", "/secrets/sync"),
];

/// The plan request of [`wizard`] and [`BATCH`].
fn planned() -> Value {
    json!({"name": "demo", "template": TEMPLATE, "public": false})
}

/// Runs [`wizard`] against a fresh [`server`].
async fn run_wizard() -> (Stub, Cli, GitHome, Outcome) {
    let server = server().await;
    let (cli, git) = interactive(&server, wizard());
    let outcome = cli.run(&["create"]).await;
    (server, cli, git, outcome)
}

/// Runs [`BATCH`] (plus `extra` first) against a fresh [`server`].
async fn run_batch(extra: &[&str]) -> (Stub, Cli, GitHome, Outcome) {
    let server = server().await;
    let (cli, git) = machine(&server);
    let args: Vec<&str> = extra.iter().chain(BATCH).copied().collect();
    let outcome = cli.run(&args).await;
    (server, cli, git, outcome)
}

/// TC: CRE-11. On a terminal: group → template (name and description) →
/// name → visibility and owner → secrets (`generate` silently) → the
/// plan confirmed.
mod interactive_create {
    use super::*;

    // TC: CRE-11
    #[tokio::test]
    async fn exits_0_when_wizard_answered() {
        let (_server, _cli, _git, outcome) = run_wizard().await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
    }

    // TC: CRE-11 (the groups of the catalogue first, in its order)
    #[tokio::test]
    async fn offers_groups_of_catalogue_first_when_created_interactively() {
        let (_server, cli, _git, _outcome) = run_wizard().await;
        let first = cli.asked().first().cloned().unwrap_or_default();
        assert!(first.ends_with(r#"["web", "api"]"#), "{:?}", cli.asked());
    }

    // TC: CRE-11 (then the templates of the group with the name and description of their `liftoff.toml`)
    #[tokio::test]
    async fn shows_name_and_description_of_templates_when_group_picked() {
        let (_server, cli, _git, _outcome) = run_wizard().await;
        let second = cli.asked().get(1).cloned().unwrap_or_default();
        assert!(
            second.contains("acme-app")
                && second.contains("SolidJS + acme-ui, Vite, Bun")
                && second.contains("astro-site")
                && !second.contains("hono-api"),
            "{:?}",
            cli.asked()
        );
    }

    // TC: CRE-11 (the picked template, the typed name, private by default)
    #[tokio::test]
    async fn plans_picked_template_with_typed_name_when_created_interactively() {
        let (server, _cli, _git, _outcome) = run_wizard().await;
        assert_eq!(bodies(&server, "POST", "/git/create-plan"), vec![planned()]);
    }

    // TC: CRE-11 (the project name is the template's by default)
    #[tokio::test]
    async fn names_project_after_template_when_name_answer_empty() {
        let server = server().await;
        let mut replies = wizard();
        replies[2] = say("");
        let (cli, _git) = interactive(&server, replies);
        let _outcome = cli.run(&["create"]).await;
        let names: Vec<Value> = bodies(&server, "POST", "/git/create-plan")
            .iter()
            .map(|body| body["name"].clone())
            .collect();
        assert_eq!(names, vec![json!("acme-app")]);
    }

    // TC: CRE-11 (`public` when picked)
    #[tokio::test]
    async fn plans_public_repository_when_public_picked() {
        let server = server().await;
        let mut replies = wizard();
        replies[3] = Reply::Pick(1);
        let (cli, _git) = interactive(&server, replies);
        let _outcome = cli.run(&["create"]).await;
        let public: Vec<Value> = bodies(&server, "POST", "/git/create-plan")
            .iter()
            .map(|body| body["public"].clone())
            .collect();
        assert_eq!(public, vec![json!(true)]);
    }

    // TC: CRE-11 (the owner typed is the one planned)
    #[tokio::test]
    async fn plans_typed_owner_when_owner_answered() {
        let server = server().await;
        let mut replies = wizard();
        replies[4] = say("acme-org");
        let (cli, _git) = interactive(&server, replies);
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            bodies(&server, "POST", "/git/create-plan"),
            vec![
                json!({"name": "demo", "template": TEMPLATE, "public": false, "owner": "acme-org"})
            ],
            "{outcome:?}"
        );
    }

    // TC: CRE-11 (a secret with `generate` is not asked; the other is, without echo)
    #[tokio::test]
    async fn asks_only_secret_without_generate_when_created_interactively() {
        let (_server, cli, _git, _outcome) = run_wizard().await;
        let hidden = cli.asked_hidden();
        assert!(
            hidden.len() == 1
                && hidden
                    .iter()
                    .all(|question| question.contains("DATABASE_URL")),
            "{hidden:?}"
        );
    }

    // TC: CRE-11 (the typed value goes to `prod`, the generated one, 43
    // characters, to the project: `store_secrets`)
    #[tokio::test]
    async fn stores_typed_and_generated_secrets_when_created_interactively() {
        let (server, _cli, _git, _outcome) = run_wizard().await;
        let stored: Vec<(Value, Value, Value, usize)> = bodies(&server, "PUT", "/secrets")
            .iter()
            .map(|body| {
                (
                    body["key"].clone(),
                    body["scope"].clone(),
                    body["id"].clone(),
                    body["value"].as_str().map_or(0, str::len),
                )
            })
            .collect();
        assert_eq!(
            stored,
            vec![
                (json!("SESSION_SECRET"), json!("project"), json!("demo"), 43),
                (
                    json!("DATABASE_URL"),
                    json!("environment"),
                    json!("demo/prod"),
                    DATABASE_URL.len()
                ),
            ]
        );
    }

    // TC: CRE-11 (the typed value as typed)
    #[tokio::test]
    async fn stores_typed_value_when_created_interactively() {
        let (server, _cli, _git, _outcome) = run_wizard().await;
        let typed: Vec<Value> = bodies(&server, "PUT", "/secrets")
            .iter()
            .filter(|body| body["key"] == "DATABASE_URL")
            .map(|body| body["value"].clone())
            .collect();
        assert_eq!(typed, vec![json!(DATABASE_URL)]);
    }

    // TC: CRE-11 (the catalogue, then the plan, then its steps)
    #[tokio::test]
    async fn reads_catalogue_before_plan_when_created_interactively() {
        let (server, _cli, _git, _outcome) = run_wizard().await;
        let mut expected = trail(&[("GET", "/git/templates")]);
        expected.extend(trail(RUN));
        assert_eq!(server.trail(), expected);
    }

    // TC: CRE-11 (a no to the plan creates nothing: no repository, no
    // project, no clone)
    #[tokio::test]
    async fn generates_nothing_when_plan_not_confirmed() {
        let server = server().await;
        let mut replies = wizard();
        replies[6] = Reply::No;
        let (cli, _git) = interactive(&server, replies);
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (outcome.code, server.trail(), cli.cwd.join("demo").exists()),
            (
                1,
                trail(&[("GET", "/git/templates"), ("POST", "/git/create-plan")]),
                false
            ),
            "{outcome:?}"
        );
    }

    // TC: CRE-11 (`--group` narrows: the catalogue of that group,
    // and no question about the group)
    #[tokio::test]
    async fn asks_templates_of_group_when_group_given() {
        let server = server().await;
        let replies = wizard().split_off(1);
        let (cli, _git) = interactive(&server, replies);
        let _outcome = cli.run(&["create", "--group", "web"]).await;
        let queries: Vec<Vec<(String, String)>> = server
            .seen_at("GET", "/git/templates")
            .iter()
            .map(Seen::query_pairs)
            .collect();
        let first = cli.asked().first().cloned().unwrap_or_default();
        assert_eq!(
            (queries, first.contains("acme-app")),
            (vec![vec![("group".to_owned(), "web".to_owned())]], true),
            "{:?}",
            cli.asked()
        );
    }

    // TC: CRE-11, TPL-06 (the catalogue is the server's plus `[local] templates` of the CLI config)
    #[tokio::test]
    async fn sends_local_templates_when_catalogue_asked_for_create() {
        let server = server().await;
        let (cli, _git) = interactive(&server, wizard());
        cli.write_config(&format!(
            "default_server = \"main\"\n\n[servers.main]\nurl = \"{}\"\ntoken = \"lfs_session\"\n\n[local]\ntemplates = [\"acme/extra\", \"git.example.com/team/site\"]\n",
            server.url
        ));
        let _outcome = cli.run(&["create"]).await;
        let repos: Vec<Option<String>> = server
            .seen_at("GET", "/git/templates")
            .iter()
            .map(|seen| {
                seen.query_pairs()
                    .into_iter()
                    .find(|(key, _)| key == "repos")
                    .map(|(_, value)| value)
            })
            .collect();
        assert_eq!(
            repos,
            vec![Some("acme/extra,git.example.com/team/site".to_owned())]
        );
    }

    // TC: CRE-11 (the clone in `./<name>` has `{{project}}` and `{{domain}}` replaced, committed and pushed)
    #[tokio::test]
    async fn pushes_substituted_readme_when_created_interactively() {
        let (_server, _cli, git, _outcome) = run_wizard().await;
        assert_eq!(
            git.remote_file(REPO, "main", "README.md"),
            "# demo at https://demo.apps.example.com\n"
        );
    }

    // TC: CRE-11 (the plan's CI workflow is committed with it)
    #[tokio::test]
    async fn pushes_ci_workflow_when_created_interactively() {
        let (_server, _cli, git, _outcome) = run_wizard().await;
        assert_eq!(git.remote_file(REPO, "main", SNIPPET_PATH), SNIPPET);
    }
}

/// TC: CRE-11. What a server's plan says (a template's hint, its
/// `[substitute]` paths, the commit message) reaches the terminal with its
/// control characters replaced, as the catalogue does (TPL-03): an older
/// server may pass a hostile template's text through.
mod hostile_plan {
    use super::*;

    // TC: CRE-11, TPL-03
    #[tokio::test]
    async fn shows_no_control_character_when_plan_holds_escapes() {
        let mut planned = plan();
        planned["steps"][2]["files"] = json!(["README.md", "x\u{1b}]0;owned\u{7}"]);
        planned["steps"][3]["secrets"][1]["hint"] = json!("h\u{1b}[2J");
        planned["steps"][5]["message"] = json!("chore: m\u{1b}[1A");
        let server = server_planning(vec![reply(200, &planned.to_string())]).await;
        let (cli, _git) = interactive(&server, wizard());
        let outcome = cli.run(&["create"]).await;
        let asked = cli.asked().join("\n");
        assert!(
            !outcome.err.contains(['\u{1b}', '\u{7}'])
                && !outcome.out.contains(['\u{1b}', '\u{7}'])
                && !asked.contains(['\u{1b}', '\u{7}'])
                && outcome.err.contains("x\u{fffd}]0;owned\u{fffd}")
                && outcome.err.contains("m\u{fffd}[1A")
                && asked.contains("h\u{fffd}[2J"),
            "{outcome:?} {:?}",
            cli.asked()
        );
    }
}

/// TC: CRE-07 (the CLI's side). A domain the server finds taken is asked
/// again on a terminal; in batch mode it is the server's `409`.
mod domain_taken {
    use super::*;

    /// The server's first plan answer: the default domain is taken.
    fn taken() -> crate::support::Answer {
        problem(409, "demo.apps.example.com is taken")
    }

    // TC: CRE-07 (interactively: another domain is offered and planned)
    #[tokio::test]
    async fn plans_another_domain_when_domain_taken_interactively() {
        let server = server_planning(vec![taken(), reply(200, &plan().to_string())]).await;
        let replies = vec![
            Reply::Pick(0),
            Reply::Pick(1),
            say("demo"),
            Reply::Pick(0),
            say(""),
            say("demo-2.apps.example.com"),
            say(DATABASE_URL),
            Reply::Yes,
        ];
        let (cli, _git) = interactive(&server, replies);
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            bodies(&server, "POST", "/git/create-plan"),
            vec![
                planned(),
                json!({"name": "demo", "template": TEMPLATE, "domain": "demo-2.apps.example.com", "public": false}),
            ],
            "{outcome:?}"
        );
    }

    // TC: CRE-07 (batch: the conflict ends the run, nothing is generated)
    #[tokio::test]
    async fn exits_1_with_conflict_when_domain_taken_in_batch() {
        let server = server_planning(vec![taken()]).await;
        let (cli, _git) = machine(&server);
        let outcome = cli.run(BATCH).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), server.trail()),
            (
                1,
                "error: demo.apps.example.com is taken\n",
                trail(&[("POST", "/git/create-plan")])
            ),
            "{outcome:?}"
        );
    }
}

/// TC: CRE-12. The flags give, without a question, the plan the
/// interactive path gives.
mod batch_create {
    use super::*;

    // TC: CRE-12
    #[tokio::test]
    async fn exits_0_when_flags_given() {
        let (_server, _cli, _git, outcome) = run_batch(&[]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
    }

    // TC: CRE-12 (the same plan request as the interactive path)
    #[tokio::test]
    async fn plans_same_as_interactive_when_flags_given() {
        let (interactively, _cli, _git, _outcome) = run_wizard().await;
        let (in_batch, _batch_cli, _batch_git, _batch_outcome) = run_batch(&[]).await;
        assert_eq!(
            (
                bodies(&interactively, "POST", "/git/create-plan"),
                bodies(&in_batch, "POST", "/git/create-plan")
            ),
            (vec![planned()], vec![planned()])
        );
    }

    // TC: CRE-12 (the same requests after the plan, and no catalogue read)
    #[tokio::test]
    async fn runs_plan_steps_in_order_when_flags_given() {
        let (server, _cli, _git, _outcome) = run_batch(&[]).await;
        assert_eq!(server.trail(), trail(RUN));
    }

    // TC: CRE-12 (the server's forge generates the repository of the same request)
    #[tokio::test]
    async fn generates_repository_of_plan_request_when_flags_given() {
        let (server, _cli, _git, _outcome) = run_batch(&[]).await;
        assert_eq!(bodies(&server, "POST", "/git/generate"), vec![planned()]);
    }

    // TC: CRE-12 (`--domain`, `--public`, `--owner`)
    #[tokio::test]
    async fn plans_domain_owner_and_public_when_flags_given() {
        let server = server().await;
        let (cli, _git) = machine(&server);
        let _outcome = cli
            .run(&[
                "create",
                "demo",
                "--template",
                TEMPLATE,
                "--domain",
                "demo.example.org",
                "--public",
                "--owner",
                "acme-org",
                "--secret",
                "DATABASE_URL=postgres://db/demo",
            ])
            .await;
        assert_eq!(
            bodies(&server, "POST", "/git/create-plan"),
            vec![json!({
                "name": "demo",
                "template": TEMPLATE,
                "domain": "demo.example.org",
                "public": true,
                "owner": "acme-org",
            })]
        );
    }

    // TC: CRE-12 (`--secret K=V` is the value `store_secrets` sends)
    #[tokio::test]
    async fn stores_secret_flag_value_when_flags_given() {
        let (server, _cli, _git, _outcome) = run_batch(&[]).await;
        let given: Vec<(Value, Value)> = bodies(&server, "PUT", "/secrets")
            .iter()
            .filter(|body| body["key"] == "DATABASE_URL")
            .map(|body| (body["value"].clone(), body["id"].clone()))
            .collect();
        assert_eq!(given, vec![(json!(DATABASE_URL), json!("demo/prod"))]);
    }

    // TC: CRE-12, CRE-04 (a required secret without `--secret` in batch
    // mode: 64 with the list, before the forge generates anything)
    #[tokio::test]
    async fn exits_64_listing_secret_when_required_secret_missing() {
        let server = server().await;
        let (cli, _git) = machine(&server);
        let outcome = cli.run(&["create", "demo", "--template", TEMPLATE]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), server.trail()),
            (
                64,
                "error: missing required arguments: --secret DATABASE_URL\n",
                trail(&[("POST", "/git/create-plan")])
            ),
            "{outcome:?}"
        );
    }

    // TC: CRE-12 (`--no-input` on a terminal is batch mode: the flags are enough, nothing is asked)
    #[tokio::test]
    async fn asks_nothing_when_no_input_and_flags_given() {
        let server = server().await;
        let (cli, _git) = interactive(&server, vec![]);
        let args: Vec<&str> = core::iter::once("--no-input")
            .chain(BATCH.iter().copied())
            .collect();
        let outcome = cli.run(&args).await;
        assert_eq!(
            (outcome.code, cli.asked()),
            (0, Vec::<String>::new()),
            "{outcome:?}"
        );
    }
}

/// TC: CRE-12. What the flags cannot give is refused before the forge
/// generates anything.
mod batch_refusals {
    use super::*;

    // TC: CRE-12 (a `--secret` the plan does not ask is a typo: 64, and
    // nothing is generated)
    #[tokio::test]
    async fn exits_64_when_secret_flag_unknown_to_plan() {
        let server = server().await;
        let (cli, _git) = machine(&server);
        let args: Vec<&str> = BATCH
            .iter()
            .copied()
            .chain(["--secret", "UNASKED_KEY=x"])
            .collect();
        let outcome = cli.run(&args).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), server.trail()),
            (
                64,
                "error: --secret UNASKED_KEY: the plan of demo asks no such secret\n",
                trail(&[("POST", "/git/create-plan")])
            ),
            "{outcome:?}"
        );
    }
}

/// TC: TPL-03. A catalogue without a template has nothing to pick.
mod empty_catalogue {
    use super::*;

    // TC: TPL-03 (64 with where templates come from; nothing planned)
    #[tokio::test]
    async fn exits_64_with_hint_when_catalogue_empty() {
        let server = Stub::start().await.route(
            "GET",
            "/git/templates",
            vec![reply(200, r#"{"groups":[],"warnings":[]}"#)],
        );
        let (cli, _git) = interactive(&server, vec![]);
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.contains("the catalogue has no template"),
                outcome.err.contains("--template owner/repo"),
                server.trail()
            ),
            (64, true, true, trail(&[("GET", "/git/templates")])),
            "{outcome:?}"
        );
    }
}

/// what was made, and when its URL answers.
mod ready_message {
    use super::*;

    // TC: CRE-12 (a compose project answers after the first `git push`)
    #[tokio::test]
    async fn says_url_answers_after_push_when_compose_project_created() {
        let (_server, _cli, _git, outcome) = run_batch(&[]).await;
        assert!(
            outcome
                .out
                .contains("it answers at https://demo.apps.example.com after the first `git push`"),
            "{outcome:?}"
        );
    }

    // TC: CRE-12 (a function answers after `lof deploy`)
    #[tokio::test]
    async fn says_url_answers_after_deploy_when_function_created() {
        let mut function = plan();
        for step in function["steps"].as_array_mut().into_iter().flatten() {
            if step["step"] == "add_environment" {
                step["kind"] = json!("function");
            }
        }
        let server = server_planning(vec![reply(200, &function.to_string())]).await;
        let (cli, _git) = machine(&server);
        let outcome = cli.run(BATCH).await;
        assert!(
            outcome.code == 0
                && outcome
                    .out
                    .contains("it answers at https://demo.apps.example.com after `lof deploy`")
                && !outcome.out.contains("git push`"),
            "{outcome:?}"
        );
    }
}

/// the server's forge generates the repository through the
/// person's connection there; the CLI clones it with its own git access.
mod clone_refused {
    use super::*;

    // TC: CRE-12 (a repository the CLI cannot clone is named as generated,
    // with how to give git access to it)
    #[tokio::test]
    async fn names_generated_repository_when_clone_fails() {
        let server = server().await;
        let git = GitHome::new();
        let cli = Cli::new().local(offline_git(&git));
        cli.log_in(&server.url);
        let outcome = cli.run(BATCH).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.contains(
                    "github.com/owner/demo was generated on the forge, but cannot be cloned"
                ),
                outcome.err.contains("lof git connect"),
                server.seen_at("POST", "/projects").len(),
                cli.cwd.join("demo").exists()
            ),
            (1, true, true, 0, false),
            "{outcome:?}"
        );
    }
}

/// TC: CRE-13. `--json` prints the plan and the result, and nothing else.
mod json_output {
    use super::*;

    /// Stdout of a batch `create --json`, as JSON.
    async fn printed() -> Value {
        let (_server, _cli, _git, outcome) = run_batch(&["--json"]).await;
        serde_json::from_str(&outcome.out).unwrap_or(Value::Null)
    }

    // TC: CRE-13 (the plan as the server answered it)
    #[tokio::test]
    async fn prints_plan_as_received_when_json() {
        assert_eq!(printed().await["plan"], plan());
    }

    // TC: CRE-13 (what was made, and the URL that answers after the first push)
    #[tokio::test]
    async fn prints_project_repository_and_url_when_json() {
        assert_eq!(
            printed().await["result"],
            json!({
                "project": "demo",
                "git_host": "github.com",
                "repo": REPO,
                "url": "https://demo.apps.example.com",
            })
        );
    }

    // TC: CRE-13 (stdout is one JSON document: the steps' lines are not on
    // it)
    #[tokio::test]
    async fn prints_one_json_document_when_json() {
        let (_server, _cli, _git, outcome) = run_batch(&["--json"]).await;
        assert!(
            serde_json::from_str::<Value>(&outcome.out).is_ok(),
            "{outcome:?}"
        );
    }
}

/// TC: CRE-14. `.liftoff` names the server in the new project, and
/// `.gitignore` lists it.
mod dotfile {
    use super::*;

    // TC: CRE-14
    #[tokio::test]
    async fn writes_dotfile_naming_server_when_created() {
        let (_server, cli, _git, _outcome) = run_batch(&[]).await;
        let dotfile: Option<toml::Table> =
            std::fs::read_to_string(cli.cwd.join("demo").join(".liftoff"))
                .ok()
                .and_then(|text| text.parse().ok());
        assert_eq!(
            dotfile
                .as_ref()
                .and_then(|table| table.get("server"))
                .and_then(toml::Value::as_str),
            Some(crate::support::SERVER)
        );
    }

    // TC: CRE-14 (the template's `.gitignore` keeps its lines)
    #[tokio::test]
    async fn appends_dotfile_to_gitignore_when_created() {
        let (_server, cli, _git, _outcome) = run_batch(&[]).await;
        assert_eq!(
            std::fs::read_to_string(cli.cwd.join("demo").join(".gitignore"))
                .ok()
                .as_deref(),
            Some("node_modules\n.liftoff\n")
        );
    }
}

/// TC: CRE-15. Without a terminal `create` needs `--template`.
mod batch_without_template {
    use super::*;

    // TC: CRE-15
    #[tokio::test]
    async fn exits_64_listing_template_when_batch_lacks_template() {
        let server = server().await;
        let (cli, _git) = machine(&server);
        let outcome = cli.run(&["create", "demo"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), server.trail()),
            (
                64,
                "error: missing required arguments: --template\n",
                vec![]
            ),
            "{outcome:?}"
        );
    }

    // TC: CRE-15 (`--no-input` on a terminal is the same)
    #[tokio::test]
    async fn exits_64_listing_template_when_no_input_lacks_template() {
        let server = server().await;
        let (cli, _git) = interactive(&server, vec![]);
        let outcome = cli.run(&["--no-input", "create", "demo"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), server.trail()),
            (
                64,
                "error: missing required arguments: --template\n",
                vec![]
            ),
            "{outcome:?}"
        );
    }
}

/// TC: CRE-16. Outside a repository with a remote, and without a name,
/// batch `create` has nothing to make a project of: 64 with a hint.
mod no_remote_no_name {
    use super::*;

    // TC: CRE-16
    #[tokio::test]
    async fn exits_64_with_hint_when_no_remote_and_no_name() {
        let server = server().await;
        let (cli, _git) = machine(&server);
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.contains("--template"),
                outcome.err.contains("remote"),
                server.trail()
            ),
            (64, true, true, vec![]),
            "{outcome:?}"
        );
    }
}

/// CRE-09 and CRE-14 through the CLI: in a
/// repository with a remote and no arguments, `create` reads `git remote`
/// and `liftoff.toml`; without a manifest it detects the stack, the
/// person confirms the proposal, and the plan commits it.
mod existing_repo {
    use super::*;

    const ORIGIN: &str = "https://github.com/owner/site.git";
    const SITE: &str = "owner/site";
    const SITE_HOST: &str = "site.apps.example.com";

    /// A compose file with one exposed service (DET-07).
    const COMPOSE_EXPOSED: &str =
        "services:\n  web:\n    image: ghcr.io/owner/site:latest\n    expose: [3000]\n";

    /// What the plan writes: the detected manifest as confirmed.
    const DETECTED: &str = "[service]\ncompose = \"compose.yaml\"\nexpose = \"web:3000\"\n";

    /// The detected manifest of a repository holding `docker-compose.yml`,
    /// with `--service web --port 3000`.
    const DOCKER_COMPOSE_MANIFEST: &str =
        "[service]\ncompose = \"docker-compose.yml\"\nexpose = \"web:3000\"\n";

    /// A committed manifest.
    const COMMITTED: &str = "[service]\ncompose = \"compose.yaml\"\nexpose = \"web:8080\"\n";

    const PROJECT_SITE: &str = r#"{"id":"01J000000000000000000000P4","name":"site","git_host":"github.com","repo":"owner/site","manifest":null,"created_by":"u1","created_at":"2026-10-20T10:00:00Z"}"#;
    const ENV_SITE: &str = r#"{"id":"01J000000000000000000000E4","project":"site","name":"prod","kind":"compose","branch":"main","pinned_by":"u1","state":"stopped","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"public","created_at":"2026-10-20T10:00:00Z"}"#;
    const DOMAIN_SITE: &str = r#"{"host":"site.apps.example.com","kind":"managed","project":"site","verified_at":"2026-10-20T10:00:00Z","record_lost_at":null,"proxied":false}"#;
    const BINDING_SITE: &str = r#"{"host":"site.apps.example.com","project":"site","environment":"prod","service":"web","container_port":3000,"host_port":20002}"#;
    const TOKEN_SITE: &str = r#"{"project":"site","id":"01J000000000000000000000T4","created_at":"2026-10-20T10:00:00Z"}"#;

    /// The minimal manifest of a repository whose files tell nothing,
    /// with `--service web --port 3000` (the documented compose file).
    const MINIMAL: &str = "[service]\ncompose = \"deploy/compose.yaml\"\nexpose = \"web:3000\"\n";

    /// `CreateService::plan_for_existing` of `manifest`: a detected one is
    /// written and committed (`write_manifest`, CRE-09), a committed one
    /// (`Committed`) is left as it is.
    fn site_plan(manifest: Manifest) -> Value {
        let (content, write) = match manifest {
            Manifest::Detected(content) => (content, true),
            Manifest::Committed(content) => (content, false),
        };
        let mut steps = Vec::new();
        if write {
            steps.push(json!({"side": "local", "step": "write_manifest", "content": content}));
        }
        steps.extend([
            json!({"side": "local", "step": "write_ci_snippet", "path": SNIPPET_PATH, "content": SNIPPET}),
            json!({"side": "local", "step": "commit", "message": "chore: set up liftoff"}),
            json!({"side": "local", "step": "push"}),
            json!({"side": "server", "step": "create_project", "project": {
                "name": "site", "git_host": "github.com", "repo": SITE, "manifest": content,
            }}),
            json!({"side": "server", "step": "add_environment", "name": "prod", "kind": "compose"}),
            json!({"side": "server", "step": "add_domain", "host": SITE_HOST, "label": "site"}),
            json!({"side": "server", "step": "bind", "env": "prod", "service": "web", "port": 3000, "host": SITE_HOST}),
            json!({"side": "server", "step": "issue_token"}),
            json!({"side": "server", "step": "sync_ci"}),
            json!({"side": "local", "step": "write_liftoff_file"}),
        ]);
        json!({
            "project": "site",
            "git_host": "github.com",
            "repo": SITE,
            "domain": SITE_HOST,
            "steps": steps,
        })
    }

    /// The manifest the CLI sends, as the server's plan treats it.
    #[derive(Clone, Copy)]
    enum Manifest {
        /// Detected: the plan writes it.
        Detected(&'static str),
        /// Committed: the plan leaves it.
        Committed(&'static str),
    }

    /// The server planning [`DETECTED`] as detected.
    async fn site_server() -> Stub {
        site_server_for(Manifest::Detected(DETECTED)).await
    }

    /// The server whose plan is [`site_plan`] of `manifest`.
    async fn site_server_for(manifest: Manifest) -> Stub {
        Stub::start()
            .await
            .route(
                "POST",
                "/git/create-plan",
                vec![reply(200, &site_plan(manifest).to_string())],
            )
            .route("POST", "/projects", vec![reply(201, PROJECT_SITE)])
            .route(
                "POST",
                "/projects/site/environments",
                vec![reply(201, ENV_SITE)],
            )
            .route("POST", "/domains", vec![reply(201, DOMAIN_SITE)])
            .route(
                "POST",
                "/projects/site/environments/prod/bindings",
                vec![reply(201, BINDING_SITE)],
            )
            .route(
                "POST",
                "/projects/site/token/rotate",
                vec![reply(201, TOKEN_SITE)],
            )
            .route("POST", "/secrets/sync", vec![reply(200, SYNCED)])
    }

    /// The work tree of `owner/site` holding `files`, pushed to the
    /// forge's bare repository, logged in to `server`; on a terminal
    /// answering `replies` when there are any.
    fn work_tree(
        server: &Stub,
        files: &[(&str, &str)],
        replies: Option<Vec<Reply>>,
    ) -> (Cli, GitHome) {
        let git = GitHome::new();
        let cli = Cli::new().in_repo(ORIGIN).local(offline_git(&git));
        let cli = match replies {
            Some(replies) => cli.terminal(replies),
            None => cli,
        };
        git.bare_remote(SITE);
        git.commit_tree(&cli.root, files);
        git.git(&cli.root, &["remote", "add", "origin", ORIGIN]);
        git.git(&cli.root, &["push", "--quiet", "origin", "main"]);
        cli.log_in(&server.url);
        (cli, git)
    }

    /// [`work_tree`] run from its subdirectory `sub`, in batch mode.
    fn work_tree_inside(server: &Stub, files: &[(&str, &str)], sub: &str) -> (Cli, GitHome) {
        let git = GitHome::new();
        let cli = Cli::new()
            .in_repo(ORIGIN)
            .local(offline_git(&git))
            .inside(sub);
        git.bare_remote(SITE);
        git.commit_tree(&cli.root, files);
        git.git(&cli.root, &["remote", "add", "origin", ORIGIN]);
        git.git(&cli.root, &["push", "--quiet", "origin", "main"]);
        cli.log_in(&server.url);
        (cli, git)
    }

    /// The `existing` of the one plan request `server` saw.
    fn existing_of(server: &Stub) -> Value {
        bodies(server, "POST", "/git/create-plan")
            .first()
            .map_or(Value::Null, |body| body["existing"].clone())
    }

    /// A file that tells detection nothing.
    const README_ONLY: &[(&str, &str)] = &[("README.md", "# site\n")];

    // TC: CLIX-02, CRE-09 (moved from `interactive.rs`; the original
    // `create`: what the files leave open, the service and port, is asked
    // on a terminal or given as flags, and both give the same plan)
    #[tokio::test]
    async fn plans_same_manifest_when_service_and_port_asked_or_flagged() {
        let in_batch = site_server_for(Manifest::Detected(MINIMAL)).await;
        let (batch, _batch_git) = work_tree(&in_batch, README_ONLY, None);
        let first = batch
            .run(&["create", "--service", "web", "--port", "3000"])
            .await;
        let asked = site_server_for(Manifest::Detected(MINIMAL)).await;
        let (terminal, _git) = work_tree(
            &asked,
            README_ONLY,
            Some(vec![say("web"), say("3000"), Reply::Yes, Reply::Yes]),
        );
        let second = terminal.run(&["create"]).await;
        assert_eq!(
            (
                first.code,
                second.code,
                bodies(&in_batch, "POST", "/git/create-plan"),
                terminal.asked().len()
            ),
            (0, 0, bodies(&asked, "POST", "/git/create-plan"), 4),
            "{first:?} {second:?} {:?}",
            terminal.asked()
        );
    }

    // TC: CLIX-02, CRE-09 (moved from `interactive.rs`: the repository of
    // `origin` and a minimal manifest, committed by the plan)
    #[tokio::test]
    async fn plans_repository_and_minimal_manifest_when_service_and_port_given() {
        let server = site_server_for(Manifest::Detected(MINIMAL)).await;
        let (cli, git) = work_tree(&server, README_ONLY, None);
        let outcome = cli
            .run(&["create", "--service", "web", "--port", "3000"])
            .await;
        let body = bodies(&server, "POST", "/git/create-plan")
            .first()
            .cloned()
            .unwrap_or(Value::Null);
        assert_eq!(
            (
                outcome.code,
                body["name"].clone(),
                body["existing"]["git_host"].clone(),
                body["existing"]["repo"].clone(),
                body["existing"]["manifest"].clone(),
                git.remote_file(SITE, "main", "liftoff.toml"),
            ),
            (
                0,
                json!("site"),
                json!("github.com"),
                json!(SITE),
                json!(MINIMAL),
                MINIMAL.to_owned(),
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIX-02 (moved from `interactive.rs`; `liftoff.toml` lives at the top of the repository, and names its compose file)
    #[tokio::test]
    async fn writes_manifest_at_root_when_run_in_subdirectory() {
        let server = site_server_for(Manifest::Detected(DOCKER_COMPOSE_MANIFEST)).await;
        let (cli, _git) = work_tree_inside(
            &server,
            &[("docker-compose.yml", "services: {}\n")],
            "src/app",
        );
        let outcome = cli
            .run(&["create", "--service", "web", "--port", "3000"])
            .await;
        assert_eq!(
            (
                outcome.code,
                existing_of(&server)["manifest"].clone(),
                cli.cwd_text("liftoff.toml"),
                cli.root_text("liftoff.toml"),
            ),
            (
                0,
                json!(DOCKER_COMPOSE_MANIFEST),
                None,
                Some(DOCKER_COMPOSE_MANIFEST.to_owned()),
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIX-02 (moved from `interactive.rs`: the manifest at the root
    // is sent as is from a subdirectory)
    #[tokio::test]
    async fn sends_root_manifest_when_run_in_subdirectory() {
        let manifest = "[service]\ncompose = \"compose.yaml\"\nexpose = \"api:8080\"\n";
        let server = site_server_for(Manifest::Committed(manifest)).await;
        let (cli, _git) = work_tree_inside(&server, &[("liftoff.toml", manifest)], "src");
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (
                outcome.code,
                existing_of(&server)["manifest"].clone(),
                existing_of(&server)["detected"].clone(),
                cli.cwd_text("liftoff.toml"),
            ),
            (0, json!(manifest), json!(false), None),
            "{outcome:?}"
        );
    }

    // TC: CRE-09 (no manifest → the detected one is the plan's `existing`, marked detected; the name is the repository's)
    #[tokio::test]
    async fn plans_detected_manifest_when_repo_has_none() {
        let server = site_server().await;
        let (cli, _git) = work_tree(&server, &[("compose.yaml", COMPOSE_EXPOSED)], None);
        let outcome = cli.run(&["create"]).await;
        let sent = bodies(&server, "POST", "/git/create-plan");
        let body = sent.first().cloned().unwrap_or(Value::Null);
        assert_eq!(
            (
                sent.len(),
                body["name"].clone(),
                body["existing"]["git_host"].clone(),
                body["existing"]["repo"].clone(),
                body["existing"]["detected"].clone(),
                body["existing"]["manifest"].clone(),
            ),
            (
                1,
                json!("site"),
                json!("github.com"),
                json!(SITE),
                json!(true),
                json!(DETECTED),
            ),
            "{outcome:?}"
        );
    }

    // TC: CRE-09 (the confirmed manifest is committed and pushed)
    #[tokio::test]
    async fn commits_detected_manifest_when_confirmed() {
        let server = site_server().await;
        let (cli, git) = work_tree(
            &server,
            &[("compose.yaml", COMPOSE_EXPOSED)],
            Some(vec![Reply::Yes, Reply::Yes]),
        );
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (
                existing_of(&server)["manifest"].clone(),
                git.remote_file(SITE, "main", "liftoff.toml")
            ),
            (json!(DETECTED), DETECTED.to_owned()),
            "{outcome:?}"
        );
    }

    // TC: CRE-09 (the proposal is shown for confirmation)
    #[tokio::test]
    async fn shows_detected_manifest_when_confirmation_asked() {
        let server = site_server().await;
        let (cli, _git) = work_tree(
            &server,
            &[("compose.yaml", COMPOSE_EXPOSED)],
            Some(vec![Reply::No]),
        );
        let _outcome = cli.run(&["create"]).await;
        let first = cli.asked().first().cloned().unwrap_or_default();
        assert!(first.contains("web:3000"), "{:?}", cli.asked());
    }

    // TC: CRE-09 (a declined proposal sends nothing and writes nothing,
    // after the one question)
    #[tokio::test]
    async fn sends_nothing_when_detected_manifest_declined() {
        let server = site_server().await;
        let (cli, _git) = work_tree(
            &server,
            &[("compose.yaml", COMPOSE_EXPOSED)],
            Some(vec![Reply::No]),
        );
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.as_str(),
                server.trail(),
                cli.root_text("liftoff.toml")
            ),
            (
                1,
                "error: not confirmed; nothing was changed\n",
                vec![],
                None
            ),
            "{outcome:?}"
        );
    }

    // TC: CRE-09 (a committed manifest is sent as is, not detected)
    #[tokio::test]
    async fn plans_committed_manifest_when_repo_has_one() {
        let server = site_server_for(Manifest::Committed(COMMITTED)).await;
        let (cli, _git) = work_tree(
            &server,
            &[
                ("compose.yaml", COMPOSE_EXPOSED),
                ("liftoff.toml", COMMITTED),
            ],
            None,
        );
        let _outcome = cli.run(&["create"]).await;
        assert_eq!(
            bodies(&server, "POST", "/git/create-plan"),
            vec![json!({
                "name": "site",
                "existing": {
                    "git_host": "github.com",
                    "repo": SITE,
                    "manifest": COMMITTED,
                    "detected": false,
                },
                "public": false,
            })]
        );
    }

    // TC: CRE-09 (a committed manifest is left as committed: the plan's
    // commit holds the CI workflow, the manifest stays)
    #[tokio::test]
    async fn keeps_committed_manifest_when_repo_has_one() {
        let server = site_server_for(Manifest::Committed(COMMITTED)).await;
        let (cli, git) = work_tree(
            &server,
            &[
                ("compose.yaml", COMPOSE_EXPOSED),
                ("liftoff.toml", COMMITTED),
            ],
            None,
        );
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (
                outcome.code,
                git.remote_file(SITE, "main", "liftoff.toml"),
                git.remote_file(SITE, "main", SNIPPET_PATH)
            ),
            (0, COMMITTED.to_owned(), SNIPPET.to_owned()),
            "{outcome:?}"
        );
    }

    // TC: CRE-12 (the flags of a template's repository do not apply to
    // an existing one: 64, nothing sent)
    #[tokio::test]
    async fn exits_64_when_template_flags_given_in_existing_repo() {
        let server = site_server().await;
        let (cli, _git) = work_tree(&server, &[("compose.yaml", COMPOSE_EXPOSED)], None);
        let outcome = cli
            .run(&["create", "--public", "--owner", "acme-org"])
            .await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), server.trail()),
            (
                64,
                "error: --public, --owner name the repository a template generates, and this repository exists already\n",
                vec![]
            ),
            "{outcome:?}"
        );
    }

    // TC: CRE-14 (in an existing repository `.liftoff` is written at the top
    // of its work tree and ignored)
    #[tokio::test]
    async fn writes_ignored_dotfile_at_root_when_existing_repo_created() {
        let server = site_server().await;
        let (cli, _git) = work_tree(&server, &[("compose.yaml", COMPOSE_EXPOSED)], None);
        let outcome = cli.run(&["create"]).await;
        let dotfile: Option<toml::Table> =
            cli.root_text(".liftoff").and_then(|text| text.parse().ok());
        assert_eq!(
            (
                dotfile
                    .as_ref()
                    .and_then(|table| table.get("server"))
                    .and_then(toml::Value::as_str),
                cli.root_text(".gitignore").as_deref()
            ),
            (Some(crate::support::SERVER), Some(".liftoff\n")),
            "{outcome:?}"
        );
    }
}
