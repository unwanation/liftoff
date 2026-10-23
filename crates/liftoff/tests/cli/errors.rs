//! TC: CLIX-06 (and v1 CLI-05). An API error prints `error: <detail>` and
//! exits 1; an unreachable server exits 69; wrong arguments exit 64.

use rstest::rstest;

use crate::support::{Cli, Stub, json, problem};

mod api_error {
    use super::*;

    // TC: CLIX-06
    #[tokio::test]
    async fn prints_detail_and_exits_1_when_problem_json() {
        let stub =
            Stub::start()
                .await
                .route("GET", "/me", vec![problem(409, "something conflicts")]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(outcome.code, 1, "{outcome:?}");
        assert_eq!(outcome.err, "error: something conflicts\n");
        assert!(outcome.out.is_empty(), "{outcome:?}");
    }

    // TC: CLIX-06 (a command of the phase, not only `whoami`)
    #[tokio::test]
    async fn prints_detail_and_exits_1_when_a_phase_command_is_refused() {
        let stub = Stub::start().await.route(
            "POST",
            "/projects/liftoff/environments",
            vec![problem(409, "environment qa exists")],
        );
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli
            .run(&[
                "--project",
                "liftoff",
                "env",
                "add",
                "qa",
                "--branch",
                "develop",
            ])
            .await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), outcome.out.as_str()),
            (1, "error: environment qa exists\n", "")
        );
    }

    // TC: CLIX-06 (`ModuleRequired` is an API error like any other)
    #[tokio::test]
    async fn prints_detail_and_exits_1_when_a_module_is_required() {
        let stub = Stub::start().await.route(
            "GET",
            "/secrets",
            vec![problem(424, "install the module tenants")],
        );
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--project", "liftoff", "secret", "ls"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (1, "error: install the module tenants\n")
        );
    }

    // TC: CLIX-06
    #[tokio::test]
    async fn prints_title_when_problem_has_no_detail() {
        let stub = Stub::start().await.route(
            "GET",
            "/me",
            vec![json(404, r#"{"title":"Not Found","status":404}"#)],
        );
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(outcome.code, 1, "{outcome:?}");
        assert_eq!(outcome.err, "error: Not Found\n");
    }

    // TC: CLIX-06
    #[tokio::test]
    async fn prints_status_when_body_is_not_a_problem() {
        let stub =
            Stub::start()
                .await
                .route("GET", "/me", vec![crate::support::text(502, "bad gateway")]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(outcome.code, 1, "{outcome:?}");
        assert!(outcome.err.starts_with("error: "), "{outcome:?}");
        assert!(outcome.err.contains("502"), "{outcome:?}");
    }
}

mod session {
    use super::*;

    // TC: CLIX-06 (the next-step hint is for the interactive mode; a script gets the detail alone)
    #[tokio::test]
    async fn prints_the_detail_alone_when_the_session_is_rejected_in_batch() {
        let stub =
            Stub::start()
                .await
                .route("GET", "/me", vec![problem(401, "unknown or revoked token")]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (1, "error: unknown or revoked token\n")
        );
    }

    // TC: CLIX-06 (interactive mode adds the next step)
    #[tokio::test]
    async fn points_to_login_when_the_session_is_rejected_on_a_terminal() {
        let stub =
            Stub::start()
                .await
                .route("GET", "/me", vec![problem(401, "unknown or revoked token")]);
        let cli = Cli::logged_in(&stub.url).terminal(vec![]);
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(outcome.code, 1, "{outcome:?}");
        assert!(
            outcome.err.starts_with("error: unknown or revoked token"),
            "{outcome:?}"
        );
        assert!(outcome.err.contains("liftoff login"), "{outcome:?}");
    }
}

mod network {
    use super::*;

    /// A URL on which nothing listens.
    fn closed_port() -> String {
        // Bind and drop: the port is closed afterwards.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        format!("http://127.0.0.1:{port}")
    }

    // TC: CLIX-06
    #[tokio::test]
    async fn exits_69_when_server_is_unreachable() {
        let cli = Cli::logged_in(&closed_port());
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(outcome.code, 69, "{outcome:?}");
        assert!(outcome.err.starts_with("error: "), "{outcome:?}");
    }

    // TC: CLIX-06 (resolving the project is a request like any other)
    #[tokio::test]
    async fn exits_69_when_the_server_is_unreachable_during_resolve() {
        let cli = Cli::logged_in(&closed_port()).in_repo("git@git.example.com:acme/shop.git");
        let outcome = cli.run(&["status"]).await;
        assert_eq!(outcome.code, 69, "{outcome:?}");
    }

    // TC: CLIX-06, REG-08 (a word that is no command is a usage error even when the server cannot be asked for module commands)
    #[tokio::test]
    async fn exits_64_when_command_is_unknown_and_server_is_unreachable() {
        let cli = Cli::logged_in(&closed_port());
        let outcome = cli.run(&["frobnicate"]).await;
        assert_eq!(
            (outcome.code, outcome.err.contains("frobnicate")),
            (64, true),
            "{outcome:?}"
        );
    }

    // TC: CLIX-06 (`login` reaches the server it is given)
    #[tokio::test]
    async fn exits_69_when_login_cannot_reach_the_server() {
        let cli = Cli::new();
        let outcome = cli
            .run(&["login", &closed_port(), "--token", "lft_setup"])
            .await;
        assert_eq!(outcome.code, 69, "{outcome:?}");
        assert!(!cli.config.exists(), "nothing is written");
    }
}

mod usage {
    use super::*;

    // TC: CLIX-06 (v1 CLI-05: wrong arguments are 64)
    #[rstest]
    #[case::unknown_flag(&["whoami", "--bogus"])]
    #[case::no_command(&[])]
    #[tokio::test]
    async fn exits_64_when_arguments_are_invalid(#[case] args: &[&str]) {
        let stub = Stub::start().await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(args).await;
        assert_eq!(outcome.code, 64, "{outcome:?}");
        assert!(!outcome.err.is_empty(), "the problem is explained");
        assert!(stub.seen().is_empty());
    }

    // TC: CLIX-06, REG-08 (a word that is no command of liftoff may be a module's; only `GET /modules` is asked before the run ends with 64)
    #[tokio::test]
    async fn exits_64_asking_only_modules_when_command_is_unknown() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", vec![json(200, "[]")]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["frobnicate"]).await;
        let seen: Vec<(String, String)> = stub
            .seen()
            .into_iter()
            .map(|seen| (seen.method, seen.path))
            .collect();
        assert_eq!(
            (outcome.code, outcome.err.contains("frobnicate"), seen),
            (
                64,
                true,
                vec![("GET".to_owned(), "/api/v1/modules".to_owned())]
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIX-06, REG-08 (a word that is no command is a usage error even without a session to ask for module commands)
    #[tokio::test]
    async fn exits_64_when_command_is_unknown_and_nobody_is_logged_in() {
        let cli = Cli::new();
        let outcome = cli.run(&["frobnicate"]).await;
        assert_eq!(
            (outcome.code, outcome.err.contains("frobnicate")),
            (64, true),
            "{outcome:?}"
        );
    }

    // TC: CLIX-06
    #[tokio::test]
    async fn exits_0_when_help_is_asked() {
        let cli = Cli::new();
        let outcome = cli.run(&["--help"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert!(outcome.out.contains("Usage"), "{outcome:?}");
    }
}
