//! Resources: the logs of deploys and environments (`MCP-05`).

use rmcp::model::ReadResourceRequestParams;
use serde_json::{Value, json};

use crate::support::{Stub, TOKEN, connect, events, text};

mod templates {
    use super::*;

    // TC: MCP-05 (both documented resources)
    #[tokio::test]
    async fn both_logs_listed_when_started() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let mut uris: Vec<String> = client
            .list_all_resource_templates()
            .await
            .unwrap()
            .into_iter()
            .map(|template| template.uri_template)
            .collect();
        uris.sort_unstable();
        assert_eq!(
            uris,
            vec![
                "liftoff://deploys/{id}/log".to_owned(),
                "liftoff://projects/{p}/environments/{e}/log".to_owned(),
            ]
        );
    }
}

mod deploy_log {
    use super::*;

    const ID: &str = "01JA0000000000000000000042";

    async fn read(stub: &Stub, uri: &str) -> Value {
        let client = connect(stub).await;
        let result = client
            .read_resource(ReadResourceRequestParams::new(uri.to_owned()))
            .await
            .unwrap();
        serde_json::to_value(result).unwrap()
    }

    // TC: MCP-05
    #[tokio::test]
    async fn answers_log_text_when_deploy_read() {
        let log = "pulling registry.example.com/shop:abc123\nstarted web\n";
        let stub = Stub::start()
            .await
            .route("GET", &format!("/deploys/{ID}/log"), text(log));
        let uri = format!("liftoff://deploys/{ID}/log");
        let result = read(&stub, &uri).await;
        assert_eq!(
            result["contents"],
            json!([{"uri": uri, "mimeType": "text/plain", "text": log}])
        );
    }

    // TC: MCP-05 (read with the session of the CLI config)
    #[tokio::test]
    async fn sends_session_when_deploy_read() {
        let stub = Stub::start()
            .await
            .route("GET", &format!("/deploys/{ID}/log"), text("ok\n"));
        read(&stub, &format!("liftoff://deploys/{ID}/log")).await;
        let seen: Vec<(String, String, Option<String>)> = stub
            .calls()
            .into_iter()
            .map(|seen| (seen.method, seen.path, seen.authorization))
            .collect();
        assert_eq!(
            seen,
            vec![(
                "GET".to_owned(),
                format!("/api/v1/deploys/{ID}/log"),
                Some(format!("Bearer {TOKEN}"))
            )]
        );
    }
}

mod environment_log {
    use super::*;

    async fn read(stub: &Stub, uri: &str) -> Value {
        let client = connect(stub).await;
        let result = client
            .read_resource(ReadResourceRequestParams::new(uri.to_owned()))
            .await
            .unwrap();
        serde_json::to_value(result).unwrap()
    }

    // TC: MCP-05 (the environment's container log: the lines of the
    // event stream, read once without following)
    #[tokio::test]
    async fn answers_log_lines_when_environment_read() {
        let stub = Stub::start().await.route(
            "GET",
            "/projects/shop/environments/prod/logs",
            events(": keep-alive\n\ndata: web | listening on :8080\n\ndata: web | GET / 200\n\n"),
        );
        let uri = "liftoff://projects/shop/environments/prod/log";
        let result = read(&stub, uri).await;
        let seen: Vec<(String, Vec<String>, Option<String>)> = stub
            .calls()
            .into_iter()
            .map(|seen| {
                let pairs = seen.query_pairs();
                (seen.path, pairs, seen.authorization)
            })
            .collect();
        assert_eq!(
            (result["contents"].clone(), seen),
            (
                json!([{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": "web | listening on :8080\nweb | GET / 200\n"
                }]),
                vec![(
                    "/api/v1/projects/shop/environments/prod/logs".to_owned(),
                    vec!["follow=false".to_owned(), "tail=500".to_owned()],
                    Some(format!("Bearer {TOKEN}"))
                )]
            )
        );
    }
}
