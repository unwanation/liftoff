//! TC: CLIX-03. `--json` prints the API answer byte for byte (one final
//! newline when the body has none), whatever the command and the mode.

use rstest::rstest;

use crate::support::{Cli, Stub, json, say};

/// A pretty-printed body with keys out of order and a field the CLI's
/// types do not know: any typed re-serialisation would change its bytes.
const BODY: &str = "{\n  \"zz_future_field\": [1, 2],\n  \"login\": \"owner\",\n  \"id\": \"u1\",\n  \"email\": \"owner@example.com\",\n  \"role\": \"admin\",\n  \"source\": \"local\"\n}";

const LIST: &str = "[\n  {\"zz_future_field\": true,  \"key\": \"API_KEY\", \"scope\": \"environment\", \"version\": 2, \"ci\": false, \"updated_by\": \"u1\", \"updated_at\": \"2026-10-18T10:00:00Z\"}\n]";

mod json_flag {
    use super::*;

    // TC: CLIX-03
    #[rstest]
    #[case::whoami(&["whoami"], "GET", "/me", BODY)]
    #[case::status(&["status"], "GET", "/projects/liftoff/environments/prod", BODY)]
    #[case::env_ls(&["env", "ls"], "GET", "/projects/liftoff/environments", LIST)]
    #[case::deploys(&["deploys"], "GET", "/projects/liftoff/environments/prod/deploys", LIST)]
    #[case::domain_ls(&["domain", "ls"], "GET", "/domains", LIST)]
    #[case::secret_ls(&["secret", "ls"], "GET", "/secrets", LIST)]
    #[case::secret_history(&["secret", "history", "API_KEY"], "GET", "/secrets/API_KEY/versions", LIST)]
    #[case::admin_users(&["admin", "users"], "GET", "/admin/users", LIST)]
    #[case::token_snippet(&["token", "snippet"], "GET", "/projects/liftoff/snippet", BODY)]
    #[case::env_stop(&["env", "stop"], "POST", "/projects/liftoff/environments/prod/stop", BODY)]
    #[tokio::test]
    async fn prints_the_body_as_received(
        #[case] command: &[&str],
        #[case] method: &'static str,
        #[case] path: &str,
        #[case] body: &str,
    ) {
        let stub = Stub::start()
            .await
            .route(method, path, vec![json(200, body)]);
        let cli = Cli::logged_in(&stub.url);
        let args: Vec<&str> = ["--json", "--project", "liftoff"]
            .into_iter()
            .chain(command.iter().copied())
            .collect();
        let outcome = cli.run(&args).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{body}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIX-03 (a body that already ends in a newline is not doubled)
    #[tokio::test]
    async fn adds_no_second_newline_when_the_body_has_one() {
        let body = format!("{BODY}\n");
        let stub = Stub::start()
            .await
            .route("GET", "/me", vec![json(200, &body)]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--json", "whoami"]).await;
        assert_eq!((outcome.code, outcome.out), (0, body));
    }

    // TC: CLIX-03 (the flag goes anywhere on the line)
    #[tokio::test]
    async fn prints_the_body_when_the_flag_follows_the_command() {
        let stub = Stub::start()
            .await
            .route("GET", "/me", vec![json(200, BODY)]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["whoami", "--json"]).await;
        assert_eq!((outcome.code, outcome.out), (0, format!("{BODY}\n")));
    }

    // TC: CLIX-03 (an interactive run prints the same bytes)
    #[tokio::test]
    async fn prints_the_body_as_received_when_on_a_terminal() {
        let stub = Stub::start().await.route(
            "POST",
            "/projects/liftoff/environments",
            vec![json(201, BODY)],
        );
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("qa"), say("develop")]);
        let outcome = cli
            .run(&["--json", "--project", "liftoff", "env", "add"])
            .await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{BODY}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIX-03 (with `--json` a failure prints the problem object; stderr still says what happened)
    #[tokio::test]
    async fn prints_the_problem_object_when_the_api_fails() {
        // Pretty, out of order and with a field the CLI does not know: a
        // re-serialised problem would differ from it.
        let body =
            "{\n  \"zz\": 1,\n  \"detail\": \"environment qa exists\",\n  \"status\": 409\n}";
        let stub = Stub::start()
            .await
            .route("GET", "/me", vec![json(409, body)]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--json", "whoami"]).await;
        assert_eq!(
            (outcome.code, outcome.out, outcome.err),
            (
                1,
                format!("{body}\n"),
                "error: environment qa exists\n".to_owned()
            )
        );
    }
}
