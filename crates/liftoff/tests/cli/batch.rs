//! TC: CLIX-01. Without a terminal (or with `--no-input`) a command that
//! lacks a required argument asks nothing, sends nothing and exits 64 with
//! the list of what is missing.

use liftoff::Mode;
use rstest::rstest;

use crate::fixtures::full_stub;
use crate::support::Cli;

const SSH_REMOTE: &str = "git@git.example.com:acme/shop.git";

mod missing_arguments {
    use super::*;

    // TC: CLIX-01
    #[rstest]
    #[case::env_add(&["env", "add"], "<name>, --branch")]
    #[case::env_add_without_branch(&["env", "add", "qa"], "--branch")]
    #[case::env_rm_without_yes(&["env", "rm", "qa"], "--yes")]
    #[case::bind(&["bind"], "<service>:<port>, <host>")]
    #[case::bind_without_host(&["bind", "web:3000"], "<host>")]
    #[case::unbind(&["unbind"], "<host>")]
    #[case::domain_add(&["domain", "add"], "<label|host>")]
    #[case::domain_rm(&["domain", "rm"], "<host>")]
    #[case::domain_set(&["domain", "set"], "<host>, --proxied")]
    #[case::domain_set_without_flag(&["domain", "set", "shop.apps.example.com"], "--proxied")]
    #[case::secret_set(&["secret", "set"], "K=V")]
    #[case::secret_get(&["secret", "get"], "K")]
    #[case::secret_rm(&["secret", "rm"], "K")]
    #[case::secret_history(&["secret", "history"], "K")]
    #[case::secret_rollback(&["secret", "rollback", "API_KEY"], "--to")]
    #[case::git_connect(&["git", "connect", "forgejo"], "--host, --token")]
    #[case::admin_sessions_revoke(&["admin", "sessions", "revoke"], "<login>")]
    #[case::admin_env_set(&["admin", "env", "set"], "<project/env>")]
    #[case::server_use(&["server", "use"], "<name>")]
    #[tokio::test]
    async fn exits_64_with_the_list_when_an_argument_is_missing(
        #[case] command: &[&str],
        #[case] missing: &str,
    ) {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url);
        let args: Vec<&str> = ["--project", "liftoff"]
            .into_iter()
            .chain(command.iter().copied())
            .collect();
        let outcome = cli.run(&args).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), outcome.out.as_str()),
            (
                64,
                format!("error: missing required arguments: {missing}\n").as_str(),
                ""
            ),
            "{outcome:?}"
        );
        assert!(stub.seen().is_empty(), "{:?}", stub.trail());
        assert!(cli.asked().is_empty(), "{:?}", cli.asked());
    }

    // TC: CLIX-01 (`login` needs no config to say what is missing). With
    // a URL and no token the CLI first asks the server for the `oidc`
    // module's device flow, so that case runs against a stub:
    // `login::device_flow::asks_for_token_when_server_has_no_oidc_module`.
    #[rstest]
    #[case::nothing(&["login"], "<url>, --token")]
    #[tokio::test]
    async fn exits_64_with_the_list_when_login_lacks_arguments(
        #[case] args: &[&str],
        #[case] missing: &str,
    ) {
        let cli = Cli::new();
        let outcome = cli.run(args).await;
        assert_eq!(
            (outcome.code, outcome.err),
            (
                64,
                format!("error: missing required arguments: {missing}\n")
            ),
        );
        assert!(!cli.config.exists(), "nothing is written");
    }

    // TC: CLIX-01 (the minimal `create` asks service and port)
    #[tokio::test]
    async fn exits_64_with_the_list_when_create_lacks_service_and_port() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url).in_repo(SSH_REMOTE);
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (64, "error: missing required arguments: --service, --port\n"),
            "{outcome:?}"
        );
        assert!(stub.seen().is_empty(), "{:?}", stub.trail());
        assert_eq!(cli.cwd_text("liftoff.toml"), None);
    }

    // TC: CLIX-01 (outside a repository the project must be named)
    #[tokio::test]
    async fn exits_64_naming_project_when_no_remote_and_no_flag() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["status"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (64, "error: missing required arguments: --project\n"),
            "{outcome:?}"
        );
        assert!(stub.seen().is_empty(), "{:?}", stub.trail());
    }
}

mod no_input_flag {
    use super::*;

    // TC: CLIX-01 (`--no-input` is batch mode even on a terminal)
    #[tokio::test]
    async fn asks_nothing_and_exits_64_when_no_input_on_a_terminal() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![]);
        let outcome = cli
            .run(&["--no-input", "--project", "liftoff", "env", "add"])
            .await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (64, "error: missing required arguments: <name>, --branch\n"),
            "{outcome:?}"
        );
        assert!(cli.asked().is_empty(), "{:?}", cli.asked());
        assert!(stub.seen().is_empty(), "{:?}", stub.trail());
    }
}

mod mode_detect {
    use super::*;

    // TC: CLIX-01
    #[rstest]
    #[case::no_terminal(false, false)]
    #[case::no_terminal_and_no_input(false, true)]
    #[case::terminal_and_no_input(true, true)]
    fn is_batch_when_no_terminal_or_no_input(#[case] tty: bool, #[case] no_input: bool) {
        assert_eq!(Mode::detect(tty, no_input), Mode::Batch);
    }

    // TC: CLIX-02
    #[test]
    fn is_interactive_when_terminal_and_no_flag() {
        assert_eq!(Mode::detect(true, false), Mode::Interactive);
    }
}
