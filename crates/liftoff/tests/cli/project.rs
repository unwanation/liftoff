//! TC: CLIX-04. The project comes from `git remote get-url origin` (ssh
//! and https forms), resolved by the server; `.liftoff` overrides the
//! environment; `--project`/`--env` beat both.

use std::path::Path;
use std::process::Command;

use liftoff::{GitCommand, GitRemote as _};
use rstest::rstest;

use crate::fixtures::{ENV, full_stub};
use crate::support::{Cli, Stub, json, problem};

const RESOLVE: &str = "/projects/-/resolve";

fn trail(stub: &Stub) -> Vec<(String, String)> {
    stub.trail()
}

fn step(method: &str, path: &str) -> (String, String) {
    (method.to_owned(), path.to_owned())
}

/// The resolved project is `web-app`: its routes answer too.
async fn stub() -> Stub {
    full_stub()
        .await
        .route(
            "GET",
            "/projects/web-app/environments/prod",
            vec![json(200, ENV)],
        )
        .route(
            "GET",
            "/projects/web-app/environments/qa",
            vec![json(200, ENV)],
        )
        .route(
            "GET",
            "/projects/web-app/environments/stage",
            vec![json(200, ENV)],
        )
        // `status` lists the project's tunnels too.
        .route(
            "GET",
            "/projects/web-app/environments",
            vec![json(200, &format!("[{ENV}]"))],
        )
}

mod remote_forms {
    use super::*;

    // TC: CLIX-04
    #[rstest]
    #[case::scp_like_ssh("git@git.example.com:acme/shop.git")]
    #[case::scp_like_ssh_without_suffix("git@git.example.com:acme/shop")]
    #[case::ssh_url_with_port("ssh://git@git.example.com:2222/acme/shop.git")]
    #[case::https("https://git.example.com/acme/shop.git")]
    #[case::https_without_suffix("https://git.example.com/acme/shop")]
    #[case::https_with_user("https://owner@git.example.com/acme/shop.git")]
    #[case::https_trailing_slash("https://git.example.com/acme/shop/")]
    #[tokio::test]
    async fn resolves_host_and_owner_repo_when_origin_is(#[case] origin: &str) {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).in_repo(origin);
        let outcome = cli.run(&["status"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let sent = stub.seen_at("POST", RESOLVE);
        assert_eq!(
            sent.first().map(crate::support::Seen::json),
            Some(serde_json::json!({"git_host": "git.example.com", "repo": "acme/shop"}))
        );
    }

    // TC: CLIX-04 (an origin that names no forge repository)
    #[tokio::test]
    async fn exits_64_naming_project_when_origin_is_a_local_path() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).in_repo("/srv/git/liftoff.git");
        let outcome = cli.run(&["status"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (64, "error: missing required arguments: --project\n"),
            "{outcome:?}"
        );
        assert!(stub.seen().is_empty(), "{:?}", stub.trail());
    }
}

mod resolution {
    use super::*;

    const ORIGIN: &str = "git@git.example.com:acme/shop.git";

    // TC: CLIX-04 (the name comes from the server, not from the repo)
    #[tokio::test]
    async fn uses_the_resolved_name_and_prod_when_no_overrides() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).in_repo(ORIGIN);
        let outcome = cli.run(&["status"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            trail(&stub),
            vec![
                step("POST", RESOLVE),
                step("GET", "/projects/web-app/environments/prod"),
                step("GET", "/projects/web-app/environments")
            ]
        );
    }

    // TC: CLIX-04
    #[tokio::test]
    async fn uses_the_dotfile_environment_when_dotfile_names_one() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).in_repo(ORIGIN);
        cli.dotfile("env = \"qa\"\n");
        let outcome = cli.run(&["status"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            trail(&stub),
            vec![
                step("POST", RESOLVE),
                step("GET", "/projects/web-app/environments/qa"),
                step("GET", "/projects/web-app/environments")
            ]
        );
    }

    // TC: CLIX-04 (a flag beats the file)
    #[tokio::test]
    async fn uses_the_env_flag_when_both_flag_and_dotfile() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).in_repo(ORIGIN);
        cli.dotfile("env = \"qa\"\n");
        let outcome = cli.run(&["--env", "stage", "status"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            trail(&stub),
            vec![
                step("POST", RESOLVE),
                step("GET", "/projects/web-app/environments/stage"),
                step("GET", "/projects/web-app/environments")
            ]
        );
    }

    // TC: CLIX-04 (`--project` needs no resolve)
    #[tokio::test]
    async fn skips_resolve_when_project_flag() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).in_repo(ORIGIN);
        let outcome = cli.run(&["--project", "liftoff", "status"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            trail(&stub),
            vec![
                step("GET", "/projects/liftoff/environments/prod"),
                step("GET", "/projects/liftoff/environments")
            ]
        );
    }

    // TC: CLIX-04 (secrets use the resolved project too)
    #[tokio::test]
    async fn pulls_secrets_of_the_resolved_project_and_dotfile_env() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).in_repo(ORIGIN);
        cli.dotfile("env = \"qa\"\n");
        let outcome = cli.run(&["secret", "pull"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let sent = stub.seen_at("GET", "/secrets/resolve");
        assert_eq!(
            sent.first().map(crate::support::Seen::query_pairs),
            Some(vec![
                ("project".to_owned(), "web-app".to_owned()),
                ("env".to_owned(), "qa".to_owned())
            ])
        );
    }

    // TC: CLIX-04 (an unknown repository is the server's answer)
    #[tokio::test]
    async fn prints_the_detail_and_exits_1_when_the_repository_is_unknown() {
        let stub = Stub::start().await.route(
            "POST",
            RESOLVE,
            vec![problem(404, "no project for github.com/acme/shop")],
        );
        let cli = Cli::logged_in(&stub.url).in_repo(ORIGIN);
        let outcome = cli.run(&["status"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (1, "error: no project for github.com/acme/shop\n")
        );
    }

    // TC: CLIX-04 (a `.liftoff` with unknown keys is a config error)
    #[tokio::test]
    async fn exits_78_when_the_dotfile_has_an_unknown_key() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).in_repo(ORIGIN);
        cli.dotfile("environment = \"qa\"\n");
        let outcome = cli.run(&["status"]).await;
        assert_eq!(outcome.code, 78, "{outcome:?}");
        assert!(outcome.err.starts_with("error: "), "{outcome:?}");
        assert!(stub.seen().is_empty(), "{:?}", stub.trail());
    }
}

mod git_command {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    // TC: CLIX-04 (the real `git remote get-url origin`)
    #[test]
    fn reads_origin_when_the_directory_is_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        git(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                "git@git.example.com:acme/shop.git",
            ],
        );
        assert_eq!(
            GitCommand.origin(dir.path()).as_deref(),
            Some("git@git.example.com:acme/shop.git")
        );
    }

    // TC: CLIX-04
    #[test]
    fn reads_origin_when_the_directory_is_inside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        git(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/shop.git",
            ],
        );
        let nested = dir.path().join("deploy");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            GitCommand.origin(&nested).as_deref(),
            Some("https://github.com/acme/shop.git")
        );
    }

    // TC: CLIX-04
    #[test]
    fn is_none_when_the_repository_has_no_origin() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        assert_eq!(GitCommand.origin(dir.path()), None);
    }
}
