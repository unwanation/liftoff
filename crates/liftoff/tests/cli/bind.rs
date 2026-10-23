//! `lof bind`: `<service>:<port> <host>` for a compose environment,
//! `<host>` alone for a function environment (a function binding).

use serde_json::json;

use crate::support::{Cli, Stub, json};

const FUNCTION_BINDING: &str = r#"{"host":"api.apps.example.com","project":"liftoff","environment":"api","service":"function","container_port":80,"host_port":20002}"#;

const API_BINDINGS: &str = "/projects/liftoff/environments/api/bindings";

mod function {
    use super::*;

    async fn bind_host() -> (crate::support::Outcome, Stub) {
        let stub =
            Stub::start()
                .await
                .route("POST", API_BINDINGS, vec![json(201, FUNCTION_BINDING)]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli
            .run(&[
                "--project",
                "liftoff",
                "--env",
                "api",
                "--no-input",
                "bind",
                "api.apps.example.com",
            ])
            .await;
        (outcome, stub)
    }

    // TC: REST-05 (the CLI side: a host alone binds a function environment)
    #[tokio::test]
    async fn sends_only_the_host_when_given_a_host_alone() {
        let (outcome, stub) = bind_host().await;
        let sent: Vec<serde_json::Value> = stub
            .seen_at("POST", API_BINDINGS)
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        assert_eq!(
            (outcome.code, sent),
            (0, vec![json!({"host": "api.apps.example.com"})]),
            "{:?}",
            outcome.err
        );
    }

    // TC: REST-05
    #[tokio::test]
    async fn prints_the_function_route_when_bound() {
        let (outcome, _stub) = bind_host().await;
        assert_eq!(
            outcome.out, "bound api.apps.example.com to the function of liftoff/api\n",
            "{:?}",
            outcome.err
        );
    }
}
