//! `lof mcp` as Claude Code runs it (`claude mcp add --transport stdio lof
//! -- lof mcp`): the binary itself, its stdin and stdout piped, with the
//! config of a logged-in CLI pointing at the stub (`MCP-03`).

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::support::{Cli, SESSION, Stub, json as answer};

/// The project the stub lists.
fn project() -> Value {
    json!({
        "id": "01JA0000000000000000000001",
        "name": "shop",
        "git_host": "git.example.com",
        "repo": "alice/shop",
        "manifest": null,
        "created_by": "01JA0000000000000000000000",
        "created_at": "2026-10-20T10:00:00Z"
    })
}

/// `lof mcp` started with the config of `cli`.
fn spawn(cli: &Cli) -> Child {
    Command::new(env!("CARGO_BIN_EXE_liftoff"))
        .arg("mcp")
        .env("LIFTOFF_CONFIG", &cli.config)
        .current_dir(&cli.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("lof mcp starts")
}

/// One JSON-RPC session over the child's pipes; every line `lof mcp`
/// writes is kept.
struct Session {
    input: ChildStdin,
    output: tokio::io::Lines<BufReader<ChildStdout>>,
    lines: Vec<String>,
}

impl Session {
    async fn send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.input.write_all(line.as_bytes()).await.unwrap();
        self.input.flush().await.unwrap();
    }

    /// The answer to the request `id`.
    async fn answer(&mut self, id: u64) -> Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(20), self.output.next_line())
                .await
                .expect("lof mcp answers within 20 s")
                .unwrap()
                .expect("lof mcp keeps stdout open");
            self.lines.push(line.clone());
            let message: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if message["id"] == id {
                return message;
            }
        }
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.answer(id).await
    }
}

// TC: MCP-03 (an MCP client of `lof mcp` sees the
// tools, and `liftoff_projects_list` answers with the session of the
// config)
#[tokio::test]
async fn projects_listed_with_config_session_when_client_calls_lof_mcp() {
    let stub = Stub::start().await.route(
        "GET",
        "/projects",
        vec![answer(200, &json!([project()]).to_string())],
    );
    let cli = Cli::logged_in(&stub.url);
    let mut child = spawn(&cli);
    let mut session = Session {
        input: child.stdin.take().unwrap(),
        output: BufReader::new(child.stdout.take().unwrap()).lines(),
        lines: Vec::new(),
    };
    let initialized = session
        .request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "cli-test", "version": "0"}
            }),
        )
        .await;
    session
        .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    let listed = session.request(2, "tools/list", json!({})).await;
    let called = session
        .request(
            3,
            "tools/call",
            json!({"name": "liftoff_projects_list", "arguments": {}}),
        )
        .await;
    drop(session.input);
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("lof mcp stops when its client hangs up")
        .unwrap();
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    let projects: Option<Value> = called["result"]["content"][0]["text"]
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok());
    let seen: Vec<Option<String>> = stub
        .seen_at("GET", "/projects")
        .into_iter()
        .map(|seen| seen.authorization)
        .collect();
    assert_eq!(
        (
            initialized["result"]["serverInfo"]["name"].clone(),
            names.contains(&"liftoff_projects_list"),
            called["result"]["isError"].as_bool().unwrap_or(false),
            projects,
            seen,
        ),
        (
            json!("liftoff"),
            true,
            false,
            Some(json!([project()])),
            vec![Some(format!("Bearer {SESSION}"))],
        ),
        "{called}"
    );
    // stdout carries JSON-RPC and nothing else (the CLI owns it).
    let stray: Vec<&String> = session
        .lines
        .iter()
        .filter(|line| serde_json::from_str::<Value>(line).is_err())
        .collect();
    assert_eq!((status.success(), stray), (true, Vec::<&String>::new()));
}
