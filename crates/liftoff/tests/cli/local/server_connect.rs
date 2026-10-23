//! TC: LCL-04. `lof server connect <url>` in a project made in local mode
//! runs the server steps of its `create` plan on that server (the server builds and checks the plan, `POST /git/create-plan` with the repository as `existing`, and the CLI runs its server steps through their own endpoints), then writes `.liftoff`
//! naming the server and adds it to `.gitignore`.
//!
//! The server is the one of the config whose URL is `<url>`: the person
//! logged in to it first (`lof login`). The plan the stub answers mirrors
//! `CreateService::plan_for_existing` for the project local `create`
//! made; the "forge" is a bare repository the CLI's real `git` pushes to.

use std::path::Path;

use liftoff::local::{Local, RegistryKey};
use serde_json::json;

use super::repo::GitHome;
use crate::support::{Cli, SERVER, Stub, json};

/// The `origin` of the local project.
const ORIGIN: &str = "https://github.com/owner/demo.git";

/// `owner/name` of the repository on the forge.
const REPO: &str = "owner/demo";

/// Its committed `liftoff.toml` (what local `create` left from the
/// template: `{{project}}` substituted, `{{domain}}` left for the server).
const MANIFEST: &str = "[service]\ncompose = \"deploy/compose.yaml\"\nexpose = \"web:3000\"\n\n[secrets]\nSESSION_SECRET = { hint = \"32+ random bytes\", generate = \"random:32\" }\n\n[substitute]\nfiles = [\"README.md\"]\n";

/// Its committed `README.md`.
const README: &str = "# demo at https://{{domain}}\n";

const HOST: &str = "demo.apps.example.com";

/// Where the plan writes the CI workflow, and the workflow.
const SNIPPET_PATH: &str = ".github/workflows/deploy.yml";
const SNIPPET: &str = "name: deploy\non: push\n";

/// What local `create` generated for `SESSION_SECRET`.
const SESSION_SECRET: &str = "local-session-secret";

/// What the server answers to `POST /git/create-plan` for the existing
/// repository, as `CreateService::plan_for_existing` builds it for this
/// manifest (`finish_plan`: the secrets, the CI workflow, its commit and
/// push, the server steps with a managed domain, `.liftoff`), the
/// workflow written at `snippet_path`.
fn plan_writing(snippet_path: &str) -> String {
    json!({
        "project": "demo",
        "git_host": "github.com",
        "repo": REPO,
        "domain": HOST,
        "steps": [
            {"side": "local", "step": "ask_secrets", "secrets": [{
                "key": "SESSION_SECRET", "hint": "32+ random bytes", "generate": "random:32",
                "scope": "project", "ci": false,
            }]},
            {"side": "local", "step": "write_ci_snippet", "path": snippet_path, "content": SNIPPET},
            {"side": "local", "step": "commit", "message": "chore: set up liftoff"},
            {"side": "local", "step": "push"},
            {"side": "server", "step": "create_project", "project": {
                "name": "demo", "git_host": "github.com", "repo": REPO, "manifest": MANIFEST,
            }},
            {"side": "server", "step": "add_environment", "name": "prod", "kind": "compose"},
            {"side": "server", "step": "add_domain", "host": HOST, "label": "demo"},
            {"side": "server", "step": "bind", "env": "prod", "service": "web", "port": 3000, "host": HOST},
            {"side": "server", "step": "issue_token"},
            {"side": "server", "step": "store_secrets", "secrets": [{
                "key": "SESSION_SECRET", "scope": "project", "ci": false,
            }]},
            {"side": "server", "step": "sync_ci"},
            {"side": "local", "step": "write_liftoff_file"},
        ],
    })
    .to_string()
}

const PROJECT: &str = r#"{"id":"01J000000000000000000000P3","name":"demo","git_host":"github.com","repo":"owner/demo","manifest":null,"created_by":"u1","created_at":"2026-10-20T10:00:00Z"}"#;
const ENV: &str = r#"{"id":"01J000000000000000000000E3","project":"demo","name":"prod","kind":"compose","branch":"main","pinned_by":"u1","state":"stopped","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"public","created_at":"2026-10-20T10:00:00Z"}"#;
const DOMAIN: &str = r#"{"host":"demo.apps.example.com","kind":"managed","project":"demo","verified_at":"2026-10-20T10:00:00Z","record_lost_at":null,"proxied":false}"#;
const BINDING: &str = r#"{"host":"demo.apps.example.com","project":"demo","environment":"prod","service":"web","container_port":3000,"host_port":20001}"#;
const TOKEN: &str =
    r#"{"project":"demo","id":"01J000000000000000000000T3","created_at":"2026-10-20T10:00:00Z"}"#;
const SECRET: &str = r#"{"key":"SESSION_SECRET","scope":"project","version":1,"ci":false,"updated_by":"u1","updated_at":"2026-10-20T10:00:00Z"}"#;
const SYNCED: &str = r#"{"synced":["LIFTOFF_TOKEN"],"unchanged":[],"failed":[]}"#;

/// The server, answering the plan of [`plan_writing`] `snippet_path`.
async fn server_writing(snippet_path: &str) -> Stub {
    server_answering(&plan_writing(snippet_path)).await
}

/// The server, answering `plan` to `POST /git/create-plan`.
async fn server_answering(plan: &str) -> Stub {
    Stub::start()
        .await
        .route("POST", "/git/create-plan", vec![json(200, plan)])
        .route("POST", "/projects", vec![json(201, PROJECT)])
        .route("POST", "/projects/demo/environments", vec![json(201, ENV)])
        .route("POST", "/domains", vec![json(201, DOMAIN)])
        .route(
            "POST",
            "/projects/demo/environments/prod/bindings",
            vec![json(201, BINDING)],
        )
        .route(
            "POST",
            "/projects/demo/token/rotate",
            vec![json(201, TOKEN)],
        )
        .route("PUT", "/secrets", vec![json(200, SECRET)])
        .route("POST", "/secrets/sync", vec![json(200, SYNCED)])
}

async fn server() -> Stub {
    server_writing(SNIPPET_PATH).await
}

/// Makes `cli.root` the work tree of `owner/demo`: `liftoff.toml` and
/// `README.md` committed on `main` and pushed to the bare repository the
/// "forge" holds.
fn work_tree(cli: &Cli, git: &GitHome) {
    git.bare_remote(REPO);
    git.commit_tree(
        &cli.root,
        &[("liftoff.toml", MANIFEST), ("README.md", README)],
    );
    git.git(&cli.root, &["remote", "add", "origin", ORIGIN]);
    git.git(&cli.root, &["push", "--quiet", "origin", "main"]);
}

/// The local project as local `create` left it (its work tree, and
/// `SESSION_SECRET` in the local store), then logged in to `server`;
/// the git home lives as long as the CLI.
async fn project(server: &Stub) -> (Cli, GitHome) {
    let git = GitHome::new();
    let local = git.env().into_iter().fold(
        Local::new("http://127.0.0.1:9/index.json", RegistryKey::new("")),
        |local, (key, value)| local.with_git_env(key, value),
    );
    let cli = Cli::new().in_repo(ORIGIN).local(local);
    work_tree(&cli, &git);
    let set = cli
        .run(&[
            "--project",
            "demo",
            "secret",
            "set",
            "--scope",
            "project",
            &format!("SESSION_SECRET={SESSION_SECRET}"),
        ])
        .await;
    assert_eq!(set.code, 0, "secret set: {set:?}");
    cli.log_in(&server.url);
    (cli, git)
}

/// The JSON bodies `server` saw at `method path`.
fn bodies(server: &Stub, method: &str, path: &str) -> Vec<serde_json::Value> {
    server
        .seen_at(method, path)
        .iter()
        .map(crate::support::Seen::json)
        .collect()
}

/// `(method, path)` of a trail, as literals.
fn trail(steps: &[(&str, &str)]) -> Vec<(String, String)> {
    steps
        .iter()
        .map(|(method, path)| ((*method).to_owned(), (*path).to_owned()))
        .collect()
}

mod connect_project {
    use super::*;

    // TC: LCL-04
    #[tokio::test]
    async fn exits_0_when_server_steps_ran() {
        let server = server().await;
        let (cli, _git) = project(&server).await;
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
    }

    // TC: LCL-04 (the plan first, then its server steps in order)
    #[tokio::test]
    async fn runs_server_steps_of_plan_in_order_when_project_connects() {
        let server = server().await;
        let (cli, _git) = project(&server).await;
        let _outcome = cli.run(&["server", "connect", &server.url]).await;
        assert_eq!(
            server.trail(),
            trail(&[
                ("POST", "/git/create-plan"),
                ("POST", "/projects"),
                ("POST", "/projects/demo/environments"),
                ("POST", "/domains"),
                ("POST", "/projects/demo/environments/prod/bindings"),
                ("POST", "/projects/demo/token/rotate"),
                ("PUT", "/secrets"),
                ("POST", "/secrets/sync"),
            ])
        );
    }

    // TC: LCL-04 (the server builds the plan of the existing
    // repository from `git remote` and the committed `liftoff.toml`)
    #[tokio::test]
    async fn asks_plan_for_existing_repository_when_project_connects() {
        let server = server().await;
        let (cli, _git) = project(&server).await;
        let _outcome = cli.run(&["server", "connect", &server.url]).await;
        let asked: Vec<serde_json::Value> = server
            .seen_at("POST", "/git/create-plan")
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        assert_eq!(
            asked,
            vec![json!({
                "name": "demo",
                "existing": {
                    "git_host": "github.com",
                    "repo": "owner/demo",
                    "manifest": MANIFEST,
                    "detected": false,
                },
                "public": false,
            })]
        );
    }

    // TC: LCL-04 (the `CreateProject` step's body as the plan has it)
    #[tokio::test]
    async fn creates_project_of_plan_when_project_connects() {
        let server = server().await;
        let (cli, _git) = project(&server).await;
        let _outcome = cli.run(&["server", "connect", &server.url]).await;
        let created: Vec<serde_json::Value> = server
            .seen_at("POST", "/projects")
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        assert_eq!(
            created,
            vec![json!({
                "name": "demo",
                "git_host": "github.com",
                "repo": "owner/demo",
                "manifest": MANIFEST,
            })]
        );
    }

    // TC: LCL-04 (`.liftoff` names the server)
    #[tokio::test]
    async fn writes_dotfile_naming_server_when_project_connects() {
        let server = server().await;
        let (cli, _git) = project(&server).await;
        let _outcome = cli.run(&["server", "connect", &server.url]).await;
        let dotfile: Option<toml::Table> =
            cli.root_text(".liftoff").and_then(|text| text.parse().ok());
        assert_eq!(
            dotfile
                .as_ref()
                .and_then(|table| table.get("server"))
                .and_then(toml::Value::as_str),
            Some(SERVER)
        );
    }

    // TC: LCL-04 (`.liftoff` is never committed)
    #[tokio::test]
    async fn ignores_dotfile_in_git_when_project_connects() {
        let server = server().await;
        let (cli, _git) = project(&server).await;
        let _outcome = cli.run(&["server", "connect", &server.url]).await;
        let ignored = cli
            .root_text(".gitignore")
            .is_some_and(|text| text.lines().any(|line| line.trim() == ".liftoff"));
        assert!(ignored, "{:?}", cli.root_text(".gitignore"));
    }

    // TC: LCL-04 (a URL the config has no session for: log in first;
    // nothing is sent anywhere)
    #[tokio::test]
    async fn points_to_login_when_url_is_not_in_config() {
        let server = server().await;
        let (cli, _git) = project(&server).await;
        let outcome = cli
            .run(&["server", "connect", "https://new.example.com"])
            .await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.contains("login"),
                server.seen().len()
            ),
            (1, true, 0),
            "{outcome:?}"
        );
    }

    // TC: LCL-04 (a managed domain is added by its label)
    #[tokio::test]
    async fn adds_managed_domain_by_label_when_project_connects() {
        let server = server().await;
        let (cli, _git) = project(&server).await;
        let _outcome = cli.run(&["server", "connect", &server.url]).await;
        assert_eq!(
            bodies(&server, "POST", "/domains"),
            vec![json!({"project": "demo", "name": "demo", "external": false})]
        );
    }

    // TC: LCL-04 (`prod` on the default branch)
    #[tokio::test]
    async fn adds_prod_on_default_branch_when_project_connects() {
        let server = server().await;
        let (cli, _git) = project(&server).await;
        let _outcome = cli.run(&["server", "connect", &server.url]).await;
        assert_eq!(
            bodies(&server, "POST", "/projects/demo/environments"),
            vec![json!({"name": "prod", "branch": "main", "kind": "compose"})]
        );
    }

    // TC: LCL-04 (the value local `create` generated goes to the server
    // in the plan's scope, not a new one)
    #[tokio::test]
    async fn stores_local_secret_value_when_project_connects() {
        let server = server().await;
        let (cli, _git) = project(&server).await;
        let _outcome = cli.run(&["server", "connect", &server.url]).await;
        assert_eq!(
            bodies(&server, "PUT", "/secrets"),
            vec![json!({
                "scope": "project",
                "id": "demo",
                "key": "SESSION_SECRET",
                "value": SESSION_SECRET,
                "ci": false,
            })]
        );
    }

    // TC: LCL-04 (the CI workflow is committed and pushed before the server steps)
    #[tokio::test]
    async fn pushes_ci_workflow_to_forge_when_project_connects() {
        let server = server().await;
        let (cli, git) = project(&server).await;
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        assert_eq!(
            git.remote_file(REPO, "main", SNIPPET_PATH),
            SNIPPET,
            "{outcome:?}"
        );
    }

    // TC: LCL-04 (`{{domain}}` of `[substitute]` becomes the domain the server gives the project)
    #[tokio::test]
    async fn pushes_domain_into_substituted_files_when_project_connects() {
        let server = server().await;
        let (cli, git) = project(&server).await;
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        assert_eq!(
            git.remote_file(REPO, "main", "README.md"),
            format!("# demo at https://{HOST}\n"),
            "{outcome:?}"
        );
    }
}

/// TC: LCL-04. The push of `server connect` reaches a private repository:
/// it carries the local connection's credential, as local `create`'s
/// clone and push do.
mod connect_with_forge_credential {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    use super::super::{Machine, TOKEN as FORGE_TOKEN, connected};
    use super::*;

    /// A `pre-push` hook of the work tree that writes to `out` the
    /// `http.extraHeader` the push's `git` sends to `https://github.com/`
    /// (however it was configured).
    fn record_push_header(root: &Path, out: &Path) {
        let hook = root.join(".git").join("hooks").join("pre-push");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\ngit config --get-urlmatch http.extraHeader https://github.com/owner/demo.git > '{}'\n",
                out.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
    }

    // TC: LCL-04
    #[tokio::test]
    async fn pushes_with_local_connection_header_when_project_connects() {
        let server = server().await;
        let Machine {
            cli,
            git,
            forge: _forge,
            _registry,
        } = connected().await;
        let cli = cli.in_repo(ORIGIN);
        work_tree(&cli, &git);
        let out = cli.config_dir().join("..").join("push-header");
        record_push_header(&cli.root, &out);
        cli.log_in(&server.url);
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        let basic = STANDARD.encode(format!("owner:{FORGE_TOKEN}"));
        assert_eq!(
            std::fs::read_to_string(&out).ok(),
            Some(format!("Authorization: Basic {basic}\n")),
            "{outcome:?}"
        );
    }
}

/// TC: LCL-04. A plan's `write_ci_snippet` writes inside the work tree
/// only: a path that leaves it, or goes through a symlink, is refused
/// before anything reaches the server.
mod confined_snippet {
    use super::*;

    /// `server connect` with a plan writing the workflow at `path`: its
    /// exit code and the requests after the plan.
    async fn connect_writing(path: &str, prepare: impl FnOnce(&Cli)) -> (u8, usize) {
        let server = server_writing(path).await;
        let (cli, _git) = project(&server).await;
        prepare(&cli);
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        (outcome.code, server.seen().len().saturating_sub(1))
    }

    // TC: LCL-04
    #[tokio::test]
    async fn refuses_path_leaving_work_tree() {
        let escape = tempfile::tempdir().unwrap();
        let target = escape.path().join("deploy.yml");
        let relative = format!("../../../../../../../..{}", target.display());
        let outcome = connect_writing(&relative, |_| {}).await;
        assert_eq!((outcome, target.exists()), ((1, 0), false));
    }

    // TC: LCL-04
    #[tokio::test]
    async fn refuses_absolute_path() {
        let escape = tempfile::tempdir().unwrap();
        let target = escape.path().join("deploy.yml");
        let outcome = connect_writing(&target.display().to_string(), |_| {}).await;
        assert_eq!((outcome, target.exists()), ((1, 0), false));
    }

    // TC: LCL-04 (a symlinked directory of the work tree is not followed)
    #[tokio::test]
    async fn refuses_path_through_symlinked_directory() {
        let escape = tempfile::tempdir().unwrap();
        let outside = escape.path().to_path_buf();
        let outcome = connect_writing(SNIPPET_PATH, |cli| {
            std::os::unix::fs::symlink(&outside, cli.root.join(".github")).unwrap();
        })
        .await;
        assert_eq!(
            (outcome, outside.join("workflows").exists()),
            ((1, 0), false)
        );
    }
}

/// The paths of `rev` in the bare repository of [`REPO`].
fn remote_paths(git: &GitHome, rev: &str) -> Vec<String> {
    git.git(&git.remote(REPO), &["ls-tree", "-r", "--name-only", rev])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// TC: LCL-04. `server connect` runs in a work tree the person already
/// has: its commit holds what the plan wrote (the CI workflow, the manifest, the substituted files) and nothing else, and it lands on
/// the default branch `prod` deploys.
mod own_work_tree {
    use super::*;

    // TC: LCL-04 (an untracked file is not committed nor pushed)
    #[tokio::test]
    async fn leaves_untracked_file_out_of_pushed_commit() {
        let server = server().await;
        let (cli, git) = project(&server).await;
        cli.write_root("secret.txt", "API_KEY=do-not-push\n");
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        assert_eq!(
            (
                outcome.code,
                remote_paths(&git, "main").contains(&"secret.txt".to_owned())
            ),
            (0, false),
            "{outcome:?}"
        );
    }

    // TC: LCL-04 (the plan's files are what the commit holds)
    #[tokio::test]
    async fn pushes_only_files_plan_wrote_when_work_tree_has_others() {
        let server = server().await;
        let (cli, git) = project(&server).await;
        cli.write_root("todo.md", "later\n");
        let _outcome = cli.run(&["server", "connect", &server.url]).await;
        let changed = git.git(
            &git.remote(REPO),
            &["diff", "--name-only", "main~1", "main"],
        );
        let mut changed: Vec<&str> = changed.lines().collect();
        changed.sort_unstable();
        assert_eq!(changed, vec![SNIPPET_PATH, "README.md"]);
    }

    // TC: LCL-04 (uncommitted edits of tracked files: refused before
    // anything after the plan reaches the server or the forge)
    #[tokio::test]
    async fn refuses_connect_when_tracked_file_has_uncommitted_edits() {
        let server = server().await;
        let (cli, git) = project(&server).await;
        cli.write_root("liftoff.toml", &format!("{MANIFEST}# half done\n"));
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        assert_eq!(
            (
                outcome.code,
                server.seen().len(),
                remote_paths(&git, "main").contains(&SNIPPET_PATH.to_owned())
            ),
            (1, 1, false),
            "{outcome:?}"
        );
    }

    // TC: LCL-04 (`prod` deploys the default branch, so the workflow goes there; on another branch the connect is refused before anything after the plan is sent)
    #[tokio::test]
    async fn refuses_connect_when_work_tree_is_on_other_branch() {
        let server = server().await;
        let (cli, git) = project(&server).await;
        git.git(&cli.root, &["checkout", "--quiet", "-b", "feature"]);
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        let feature = git.git(&git.remote(REPO), &["branch", "--list", "feature"]);
        assert_eq!(
            (outcome.code, server.seen().len(), feature.trim().is_empty()),
            (1, 1, true),
            "{outcome:?}"
        );
    }

    // TC: LCL-04 (the refusal names the branch to switch to)
    #[tokio::test]
    async fn names_default_branch_when_work_tree_is_on_other_branch() {
        let server = server().await;
        let (cli, git) = project(&server).await;
        git.git(&cli.root, &["checkout", "--quiet", "-b", "feature"]);
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        assert!(outcome.err.contains("main"), "{outcome:?}");
    }
}

/// TC: LCL-04. What `server connect` writes besides the plan's files
/// (`.liftoff`, `.gitignore`) stays in the project directory: a committed
/// symlink of either name is not written through.
mod confined_dotfiles {
    use super::*;

    // TC: LCL-04
    #[rstest::rstest]
    #[case::dotfile(".liftoff")]
    #[case::gitignore(".gitignore")]
    #[tokio::test]
    async fn leaves_symlink_target_alone_when_name_is_a_symlink(#[case] name: &str) {
        let server = server().await;
        let (cli, git) = project(&server).await;
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim");
        std::fs::write(&target, "# untouched\n").unwrap();
        std::os::unix::fs::symlink(&target, cli.root.join(name)).unwrap();
        git.git(&cli.root, &["add", "--", name]);
        git.git(&cli.root, &["commit", "--quiet", "--message", "link"]);
        git.git(&cli.root, &["push", "--quiet", "origin", "main"]);
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        assert_eq!(
            (outcome.code, std::fs::read_to_string(&target).ok()),
            (1, Some("# untouched\n".to_owned())),
            "{outcome:?}"
        );
    }
}

/// TC: LCL-04. Plan 2.6: a tenant secret that already exists is shared
/// with the tenant's other projects, so `store_secrets` keeps it as
/// `CreateService::store_secrets` does.
mod tenant_secret {
    use super::*;

    /// [`plan_writing`] whose plan also stores `SHARED_KEY` in the tenant.
    fn plan_with_tenant_secret() -> String {
        let mut plan: serde_json::Value =
            serde_json::from_str(&plan_writing(SNIPPET_PATH)).unwrap();
        let steps = plan["steps"].as_array_mut().unwrap();
        steps[0]["secrets"].as_array_mut().unwrap().push(json!({
            "key": "SHARED_KEY", "hint": null, "generate": "random:16",
            "scope": "tenant", "ci": false,
        }));
        for step in steps.iter_mut() {
            if step["step"] == "store_secrets" {
                step["secrets"].as_array_mut().unwrap().push(json!({
                    "key": "SHARED_KEY", "scope": "tenant", "ci": false,
                }));
            }
        }
        plan.to_string()
    }

    /// The server of [`plan_with_tenant_secret`], whose tenant of `demo`
    /// already holds `SHARED_KEY`.
    async fn server_with_tenant_key() -> Stub {
        let listed = r#"[{"key":"SHARED_KEY","scope":"tenant","version":4,"ci":false,"updated_by":"u2","updated_at":"2026-10-01T10:00:00Z"}]"#;
        server_answering(&plan_with_tenant_secret()).await.route(
            "GET",
            "/secrets",
            vec![json(200, listed)],
        )
    }

    // TC: LCL-04
    #[tokio::test]
    async fn keeps_existing_tenant_secret_when_project_connects() {
        let server = server_with_tenant_key().await;
        let (cli, _git) = project(&server).await;
        let outcome = cli.run(&["server", "connect", &server.url]).await;
        let put: Vec<serde_json::Value> = bodies(&server, "PUT", "/secrets")
            .iter()
            .filter_map(|body| body.get("key").cloned())
            .collect();
        assert_eq!(
            (outcome.code, put),
            (0, vec![json!("SESSION_SECRET")]),
            "{outcome:?}"
        );
    }

    // TC: LCL-04 (it asks the tenant scope of the project)
    #[tokio::test]
    async fn asks_tenant_scope_of_project_when_plan_stores_tenant_secret() {
        let server = server_with_tenant_key().await;
        let (cli, _git) = project(&server).await;
        let _outcome = cli.run(&["server", "connect", &server.url]).await;
        let asked: Vec<Vec<(String, String)>> = server
            .seen_at("GET", "/secrets")
            .iter()
            .map(crate::support::Seen::query_pairs)
            .collect();
        assert_eq!(
            asked,
            vec![vec![
                ("scope".to_owned(), "tenant".to_owned()),
                ("id".to_owned(), "demo".to_owned()),
            ]]
        );
    }
}
