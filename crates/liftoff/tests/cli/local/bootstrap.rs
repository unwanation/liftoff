//! `lof create` and `lof templates` without a server (5.4): CRE-11…13, CRE-15 and
//! CRE-16 in local mode, an existing repository's manifest detected,
//! committed and pushed (CRE-09; without a server `create` stops after step 3), and the local catalogue (TPL-03, TPL-06).
//!
//! The catalogue is the CLI config's `[local] templates`, read through
//! the local GitHub connection by the `git.github` module the CLI runs
//! (the fake GitHub of [`forge`](super::forge) serves `acme/app` and
//! `acme/hono-api`). The interactive path is the one of server mode
//! (`tests/cli/create.rs`): group, template, name, visibility, owner,
//! the secrets without `generate` (the template has none), and a yes to
//! the plan. CRE-14 has no local case: the plan stops after `Push`, and
//! `.liftoff` is written by `lof server connect` (LCL-03, LCL-04).

use serde_json::{Value, json};

use super::forge::{Seen, TEMPLATE_MANIFEST};
use super::repo::GitHome;
use super::{Machine, connected};
use crate::create::offline_git;
use crate::support::{Cli, Outcome, Reply, say};

/// The second template of the catalogue, in group `api`.
const HONO: &str = r#"[template]
group = "api"
name = "hono-api"
description = "Hono on Bun"

[service]
compose = "deploy/compose.yaml"
expose = "api:3000"
"#;

/// The compose file of the template.
const COMPOSE: &str = "services:\n  web:\n    image: ghcr.io/owner/demo:latest\n";

/// A connected machine whose config lists both templates as `[local]
/// templates` and whose forge holds what it generates as `repo`.
async fn catalogued(repo: &str) -> Machine {
    let machine = connected().await;
    machine.forge.add_template("acme/hono-api", HONO);
    machine
        .cli
        .write_config("[local]\ntemplates = [\"acme/app\", \"acme/hono-api\"]\n");
    machine.git.seed_remote(
        repo,
        &[
            ("liftoff.toml", TEMPLATE_MANIFEST),
            ("README.md", "# {{project}}\n"),
            ("deploy/compose.yaml", COMPOSE),
        ],
    );
    machine
}

/// The answers of the interactive path for `demo` from `acme-app`,
/// private under the connection's account.
fn wizard() -> Vec<Reply> {
    vec![
        Reply::Pick(0),
        Reply::Pick(0),
        say("demo"),
        Reply::Pick(0),
        say(""),
        Reply::Yes,
    ]
}

/// The bodies of the forge's `generate` of `acme/app`.
fn generated(machine: &Machine) -> Vec<Value> {
    machine
        .forge
        .seen_at("POST", "/repos/acme/app/generate")
        .iter()
        .map(Seen::json)
        .collect()
}

/// [`wizard`] (with `replies` instead when given) on a terminal, `create`
/// with `args`.
async fn run_interactive(replies: Vec<Reply>, args: &[&str], repo: &str) -> (Machine, Outcome) {
    let machine = catalogued(repo).await.terminal(replies);
    let outcome = machine.cli.run(args).await;
    (machine, outcome)
}

/// TC: CRE-11 without a server.
mod interactive_create {
    use super::*;

    // TC: CRE-11
    #[tokio::test]
    async fn exits_0_when_wizard_answered_without_server() {
        let (_machine, outcome) = run_interactive(wizard(), &["create"], "owner/demo").await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
    }

    // TC: CRE-11 (the groups of the local catalogue)
    #[tokio::test]
    async fn offers_groups_of_local_catalogue_first_when_created_without_server() {
        let (machine, _outcome) = run_interactive(wizard(), &["create"], "owner/demo").await;
        let first = machine.cli.asked().first().cloned().unwrap_or_default();
        assert!(
            first.ends_with(r#"["web", "api"]"#),
            "{:?}",
            machine.cli.asked()
        );
    }

    // TC: CRE-11 (the templates of the group with name and description)
    #[tokio::test]
    async fn shows_name_and_description_when_group_picked_without_server() {
        let (machine, _outcome) = run_interactive(wizard(), &["create"], "owner/demo").await;
        let second = machine.cli.asked().get(1).cloned().unwrap_or_default();
        assert!(
            second.contains("acme-app")
                && second.contains("SolidJS + acme-ui, Vite, Bun")
                && !second.contains("hono-api"),
            "{:?}",
            machine.cli.asked()
        );
    }

    // TC: CRE-11 (the picked template is generated under the typed name,
    // private under the account)
    #[tokio::test]
    async fn generates_picked_template_when_created_without_server() {
        let (machine, _outcome) = run_interactive(wizard(), &["create"], "owner/demo").await;
        assert_eq!(
            generated(&machine),
            vec![json!({"owner": "owner", "name": "demo", "private": true, "include_all": true})]
        );
    }

    // TC: CRE-11 (the visibility picked and the owner typed reach the forge)
    #[tokio::test]
    async fn generates_public_repository_of_typed_owner_when_answered_without_server() {
        let mut replies = wizard();
        replies[3] = Reply::Pick(1);
        replies[4] = say("acme-org");
        let (machine, outcome) = run_interactive(replies, &["create"], "acme-org/demo").await;
        assert_eq!(
            generated(&machine),
            vec![
                json!({"owner": "acme-org", "name": "demo", "private": false, "include_all": true})
            ],
            "{outcome:?}"
        );
    }

    // TC: CRE-11 (the name is the template's by default)
    #[tokio::test]
    async fn names_project_after_template_when_name_answer_empty_without_server() {
        let mut replies = wizard();
        replies[2] = say("");
        let (machine, _outcome) = run_interactive(replies, &["create"], "owner/acme-app").await;
        let names: Vec<Value> = generated(&machine)
            .iter()
            .map(|body| body["name"].clone())
            .collect();
        assert_eq!(names, vec![json!("acme-app")]);
    }

    // TC: CRE-11 (`generate` is silent: the template has no other secret,
    // nothing is asked without echo)
    #[tokio::test]
    async fn asks_no_secret_when_every_secret_generates_without_server() {
        let (machine, outcome) = run_interactive(wizard(), &["create"], "owner/demo").await;
        assert_eq!(
            (outcome.code, machine.cli.asked_hidden()),
            (0, Vec::<String>::new()),
            "{outcome:?}"
        );
    }

    // TC: CRE-11 (a no to the plan generates nothing)
    #[tokio::test]
    async fn generates_nothing_when_plan_not_confirmed_without_server() {
        let mut replies = wizard();
        replies[5] = Reply::No;
        let (machine, outcome) = run_interactive(replies, &["create"], "owner/demo").await;
        assert_eq!(
            (
                outcome.code,
                generated(&machine).len(),
                machine.cli.cwd.join("demo").exists()
            ),
            (1, 0, false),
            "{outcome:?}"
        );
    }

    // TC: CRE-11 (`--group` narrows the catalogue: no group question)
    #[tokio::test]
    async fn offers_templates_of_group_when_group_given_without_server() {
        let replies = wizard().split_off(1);
        let (machine, _outcome) =
            run_interactive(replies, &["create", "--group", "web"], "owner/demo").await;
        let first = machine.cli.asked().first().cloned().unwrap_or_default();
        assert!(
            first.contains("acme-app") && !first.contains("hono-api"),
            "{:?}",
            machine.cli.asked()
        );
    }
}

/// TC: CRE-12 without a server.
mod batch_create {
    use super::*;

    // TC: CRE-12 (`--public` and `--owner` reach the forge)
    #[tokio::test]
    async fn generates_public_repository_of_owner_when_flags_given_without_server() {
        let machine = catalogued("acme-org/demo").await;
        let outcome = machine
            .cli
            .run(&[
                "create",
                "demo",
                "--template",
                "acme/app",
                "--public",
                "--owner",
                "acme-org",
            ])
            .await;
        assert_eq!(
            generated(&machine),
            vec![
                json!({"owner": "acme-org", "name": "demo", "private": false, "include_all": true})
            ],
            "{outcome:?}"
        );
    }

    // TC: CRE-12 (the flags give the plan the interactive path gives)
    #[tokio::test]
    async fn plans_same_as_interactive_when_flags_given_without_server() {
        let (interactively, asked) =
            run_interactive(wizard(), &["--json", "create"], "owner/demo").await;
        let batch = catalogued("owner/demo").await;
        let flagged = batch
            .cli
            .run(&[
                "--json",
                "create",
                "demo",
                "--template",
                "acme/app",
                "--private",
            ])
            .await;
        let plan = |outcome: &Outcome| {
            serde_json::from_str::<Value>(&outcome.out).unwrap_or(Value::Null)["plan"].clone()
        };
        assert!(
            plan(&asked) != Value::Null && plan(&asked) == plan(&flagged),
            "{asked:?} {flagged:?} {:?}",
            interactively.cli.asked()
        );
    }
}

/// TC: CRE-11, CRE-12 without a server: a secret without `generate` is
/// asked, or given with `--secret`, before the forge generates anything.
mod asked_secret {
    use super::*;

    /// A template whose `DATABASE_URL` has no `generate`.
    const WITH_DB: &str = r#"[template]
group = "web"
name = "with-db"
description = "With a database"

[service]
compose = "deploy/compose.yaml"
expose = "web:3000"

[secrets]
DATABASE_URL = { hint = "postgres://…", scope = "environment" }
"#;

    const DATABASE_URL: &str = "postgres://db/demo";

    /// A connected machine whose forge serves `acme/with-db` and holds
    /// what it generates for `demo`.
    async fn with_db() -> Machine {
        let machine = connected().await;
        machine.forge.add_template("acme/with-db", WITH_DB);
        machine.git.seed_remote(
            "owner/demo",
            &[("liftoff.toml", WITH_DB), ("deploy/compose.yaml", COMPOSE)],
        );
        machine
    }

    fn generated_with_db(machine: &Machine) -> usize {
        machine
            .forge
            .seen_at("POST", "/repos/acme/with-db/generate")
            .len()
    }

    /// The local value of `DATABASE_URL` of `demo`'s `prod`.
    async fn kept(machine: &Machine) -> Outcome {
        machine
            .cli
            .run(&["--project", "demo", "secret", "get", "DATABASE_URL"])
            .await
    }

    // TC: CRE-11 (asked without echo on a terminal, and kept in the local store)
    #[tokio::test]
    async fn asks_secret_without_echo_when_it_has_no_generate_without_server() {
        let machine = with_db()
            .await
            .terminal(vec![say(DATABASE_URL), Reply::Yes]);
        let outcome = machine
            .cli
            .run(&["create", "demo", "--template", "acme/with-db"])
            .await;
        let value = kept(&machine).await;
        assert_eq!(
            (
                outcome.code,
                machine
                    .cli
                    .asked_hidden()
                    .iter()
                    .filter(|question| question.contains("DATABASE_URL"))
                    .count(),
                value.out.trim_end()
            ),
            (0, 1, DATABASE_URL),
            "{outcome:?} {value:?}"
        );
    }

    // TC: CRE-12 (`--secret K=V` gives it in batch mode)
    #[tokio::test]
    async fn keeps_secret_flag_value_when_given_without_server() {
        let machine = with_db().await;
        let flag = format!("DATABASE_URL={DATABASE_URL}");
        let outcome = machine
            .cli
            .run(&[
                "create",
                "demo",
                "--template",
                "acme/with-db",
                "--secret",
                &flag,
            ])
            .await;
        let value = kept(&machine).await;
        assert_eq!(
            (outcome.code, value.out.trim_end()),
            (0, DATABASE_URL),
            "{outcome:?} {value:?}"
        );
    }

    // TC: CRE-12, CRE-04 (missing in batch mode: 64 with the list, and the
    // forge generates nothing)
    #[tokio::test]
    async fn exits_64_before_generating_when_secret_missing_without_server() {
        let machine = with_db().await;
        let outcome = machine
            .cli
            .run(&["create", "demo", "--template", "acme/with-db"])
            .await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.as_str(),
                generated_with_db(&machine)
            ),
            (
                64,
                "error: missing required arguments: --secret DATABASE_URL\n",
                0
            ),
            "{outcome:?}"
        );
    }
}

/// TC: CRE-09 without a server (steps 1–3 are `git remote` and `liftoff.toml`, a detected manifest confirmed and committed; without a server `create` stops there and says the project is ready for `lof server connect`).
mod existing_repo {
    use super::*;

    const ORIGIN: &str = "https://github.com/owner/site.git";
    const SITE: &str = "owner/site";

    /// A compose file with one exposed service (DET-07).
    const COMPOSE_EXPOSED: &str =
        "services:\n  web:\n    image: ghcr.io/owner/site:latest\n    expose: [3000]\n";

    /// What detection proposes for it.
    const DETECTED: &str = "[service]\ncompose = \"compose.yaml\"\nexpose = \"web:3000\"\n";

    /// A committed manifest.
    const COMMITTED: &str = "[service]\ncompose = \"compose.yaml\"\nexpose = \"web:8080\"\n";

    /// The work tree of `owner/site` holding `files`, pushed to the
    /// forge's bare repository, with no server in the config; on a
    /// terminal answering `replies` when there are any.
    fn work_tree(files: &[(&str, &str)], replies: Option<Vec<Reply>>) -> (Cli, GitHome) {
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
        (cli, git)
    }

    /// The files the last commit of the forge's `main` changed.
    fn last_commit_files(git: &GitHome) -> Vec<String> {
        git.git(
            &git.remote(SITE),
            &["show", "--name-only", "--format=", "main"],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }

    // TC: CRE-09 (the detected manifest is committed and pushed)
    #[tokio::test]
    async fn pushes_detected_manifest_when_created_without_server() {
        let (cli, git) = work_tree(&[("compose.yaml", COMPOSE_EXPOSED)], None);
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (outcome.code, git.remote_file(SITE, "main", "liftoff.toml")),
            (0, DETECTED.to_owned()),
            "{outcome:?}"
        );
    }

    // TC: CRE-09 (the commit holds `liftoff.toml` alone: an untracked file
    // of the work tree stays out of it)
    #[tokio::test]
    async fn commits_only_manifest_when_created_without_server() {
        let (cli, git) = work_tree(&[("compose.yaml", COMPOSE_EXPOSED)], None);
        cli.write_root("notes.txt", "draft\n");
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            last_commit_files(&git),
            vec!["liftoff.toml".to_owned()],
            "{outcome:?}"
        );
    }

    // TC: CRE-09, LCL-03 (it stops and says the project is ready for `lof server connect`; no `.liftoff`, which names a server)
    #[tokio::test]
    async fn prints_server_connect_as_next_step_when_existing_repo_created_without_server() {
        let (cli, _git) = work_tree(&[("compose.yaml", COMPOSE_EXPOSED)], None);
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.out.contains("lof server connect"),
                cli.root_text(".liftoff")
            ),
            (0, true, None),
            "{outcome:?}"
        );
    }

    // TC: CRE-09 (the proposal is shown and confirmed on a terminal)
    #[tokio::test]
    async fn shows_detected_manifest_when_confirmation_asked_without_server() {
        let (cli, _git) = work_tree(&[("compose.yaml", COMPOSE_EXPOSED)], Some(vec![Reply::Yes]));
        let outcome = cli.run(&["create"]).await;
        let first = cli.asked().first().cloned().unwrap_or_default();
        assert!(
            outcome.code == 0 && first.contains("web:3000"),
            "{outcome:?} {:?}",
            cli.asked()
        );
    }

    // TC: CRE-09 (a declined proposal writes and pushes nothing)
    #[tokio::test]
    async fn commits_nothing_when_detected_manifest_declined_without_server() {
        let (cli, git) = work_tree(&[("compose.yaml", COMPOSE_EXPOSED)], Some(vec![Reply::No]));
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (
                outcome.code,
                cli.root_text("liftoff.toml"),
                last_commit_files(&git)
            ),
            (1, None, vec!["compose.yaml".to_owned()]),
            "{outcome:?}"
        );
    }

    // TC: CRE-09 (a committed manifest is ready as it is: nothing is
    // committed)
    #[tokio::test]
    async fn keeps_committed_manifest_when_created_without_server() {
        let (cli, git) = work_tree(
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
                outcome.out.contains("lof server connect"),
                git.remote_file(SITE, "main", "liftoff.toml"),
                last_commit_files(&git).len()
            ),
            (0, true, COMMITTED.to_owned(), 2),
            "{outcome:?}"
        );
    }

    // TC: CRE-13 (`--json`: no plan without a server, and what the
    // project is)
    #[tokio::test]
    async fn prints_repository_when_json_without_server() {
        let (cli, _git) = work_tree(&[("compose.yaml", COMPOSE_EXPOSED)], None);
        let outcome = cli.run(&["--json", "create"]).await;
        let printed: Value = serde_json::from_str(&outcome.out).unwrap_or(Value::Null);
        assert_eq!(
            printed,
            json!({
                "plan": null,
                "result": {"project": "site", "git_host": "github.com", "repo": SITE},
            }),
            "{outcome:?}"
        );
    }

    // TC: CRE-12 (the flags of a template's repository: 64, nothing
    // written)
    #[tokio::test]
    async fn exits_64_when_template_flags_given_in_existing_repo_without_server() {
        let (cli, git) = work_tree(&[("compose.yaml", COMPOSE_EXPOSED)], None);
        let outcome = cli.run(&["create", "--private"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.as_str(),
                cli.root_text("liftoff.toml"),
                last_commit_files(&git)
            ),
            (
                64,
                "error: --private name the repository a template generates, and this repository exists already\n",
                None,
                vec!["compose.yaml".to_owned()]
            ),
            "{outcome:?}"
        );
    }

    // TC: CRE-12 (`--secret`, `--domain`, `--tenant` belong to the server's
    // plan: 64 naming them, nothing written or pushed)
    #[tokio::test]
    async fn exits_64_when_server_flags_given_in_existing_repo_without_server() {
        let (cli, git) = work_tree(&[("compose.yaml", COMPOSE_EXPOSED)], None);
        let outcome = cli
            .run(&[
                "create",
                "--domain",
                "site.example.com",
                "--tenant",
                "acme",
                "--secret",
                "A=b",
            ])
            .await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.as_str(),
                cli.root_text("liftoff.toml"),
                last_commit_files(&git)
            ),
            (
                64,
                "error: --secret, --domain, --tenant apply to the project on a server; without one, `lof create` here only commits liftoff.toml (`lof server connect` plans the rest)\n",
                None,
                vec!["compose.yaml".to_owned()]
            ),
            "{outcome:?}"
        );
    }
}

/// TC: CRE-13 without a server: `--json` prints the plan and the result.
mod json_output {
    use super::*;

    async fn printed() -> Value {
        let machine = catalogued("owner/demo").await;
        let outcome = machine
            .cli
            .run(&["--json", "create", "demo", "--template", "acme/app"])
            .await;
        serde_json::from_str(&outcome.out).unwrap_or(Value::Null)
    }

    // TC: CRE-13 (the plan the core built, from its first step)
    #[tokio::test]
    async fn prints_plan_when_json_without_server() {
        let printed = printed().await;
        assert_eq!(
            (
                printed["plan"]["project"].clone(),
                printed["plan"]["git_host"].clone(),
                printed["plan"]["steps"][0]["step"].clone()
            ),
            (json!("demo"), json!("github.com"), json!("generate_repo"))
        );
    }

    // TC: CRE-13 (what was made: the repository, no URL yet)
    #[tokio::test]
    async fn prints_generated_repository_when_json_without_server() {
        let printed = printed().await;
        assert_eq!(
            (
                printed["result"]["project"].clone(),
                printed["result"]["git_host"].clone(),
                printed["result"]["repo"].clone(),
                printed["result"].get("url").cloned()
            ),
            (
                json!("demo"),
                json!("github.com"),
                json!("owner/demo"),
                None
            )
        );
    }
}

/// TC: CRE-15 without a server.
mod batch_without_template {
    use super::*;

    // TC: CRE-15
    #[tokio::test]
    async fn exits_64_listing_template_when_batch_lacks_template_without_server() {
        let machine = catalogued("owner/demo").await;
        let outcome = machine.cli.run(&["create", "demo"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.as_str(),
                generated(&machine).len()
            ),
            (64, "error: missing required arguments: --template\n", 0),
            "{outcome:?}"
        );
    }
}

/// TC: CRE-16 without a server.
mod no_remote_no_name {
    use super::*;

    // TC: CRE-16
    #[tokio::test]
    async fn exits_64_with_hint_when_no_remote_and_no_name_without_server() {
        let machine = catalogued("owner/demo").await;
        let outcome = machine.cli.run(&["create"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.contains("--template"),
                outcome.err.contains("remote"),
                generated(&machine).len()
            ),
            (64, true, true, 0),
            "{outcome:?}"
        );
    }
}

/// `lof templates` without a server (`templates` works locally).
mod templates {
    use super::*;

    // TC: TPL-03, TPL-06 (the local catalogue by group, with names and
    // descriptions)
    #[tokio::test]
    async fn lists_local_catalogue_by_group_when_templates_run_without_server() {
        let machine = catalogued("owner/demo").await;
        let outcome = machine.cli.run(&["templates"]).await;
        let positions: Option<Vec<usize>> = [
            "web",
            "acme-app",
            "SolidJS + acme-ui, Vite, Bun",
            "api",
            "hono-api",
            "Hono on Bun",
        ]
        .iter()
        .map(|needle| outcome.out.find(needle))
        .collect();
        assert!(
            outcome.code == 0 && positions.is_some_and(|at| at.is_sorted()),
            "{outcome:?}"
        );
    }

    // TC: TPL-03 (`--group` keeps that group only)
    #[tokio::test]
    async fn lists_only_group_when_group_given_without_server() {
        let machine = catalogued("owner/demo").await;
        let outcome = machine.cli.run(&["templates", "--group", "api"]).await;
        assert!(
            outcome.code == 0
                && outcome.out.contains("hono-api")
                && !outcome.out.contains("acme-app"),
            "{outcome:?}"
        );
    }

    // TC: TPL-03 (`--json`: the catalogue as the server would answer it)
    #[tokio::test]
    async fn prints_catalogue_view_when_json_without_server() {
        let machine = catalogued("owner/demo").await;
        let outcome = machine.cli.run(&["--json", "templates"]).await;
        let printed: Value = serde_json::from_str(&outcome.out).unwrap_or(Value::Null);
        assert_eq!(
            printed,
            json!({
                "groups": [
                    {"group": "web", "templates": [{
                        "host": "github.com", "repo": "acme/app", "name": "acme-app",
                        "description": "SolidJS + acme-ui, Vite, Bun", "kind": "service",
                    }]},
                    {"group": "api", "templates": [{
                        "host": "github.com", "repo": "acme/hono-api", "name": "hono-api",
                        "description": "Hono on Bun", "kind": "service",
                    }]},
                ],
                "warnings": [],
            }),
            "{outcome:?}"
        );
    }
}
