//! TC: CLIX-09. `doctor` lists its checks with their status and exits 1 when
//! any check fails. Checks: `config` (mode `0600`),
//! `server` (`/healthz`), `session` (`/me`), `ready` (`/readyz`: docker,
//! `SQLite`, Caddy admin), then the rows of `GET
//! /server`: `docker`, `sqlite` and `caddy` apart, and `dns`, the wildcard
//! of the base domain.

use std::os::unix::fs::PermissionsExt as _;

use crate::fixtures::ME;
use crate::support::{Cli, Stub, json, problem};

/// `(check, status)` of each line: the first two words.
fn statuses(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some((words.next()?.to_owned(), words.next()?.to_owned()))
        })
        .collect()
}

fn expect(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(check, status)| ((*check).to_owned(), (*status).to_owned()))
        .collect()
}

/// A `ServerView` whose rows have these statuses.
fn server_view(docker: &str, sqlite: &str, caddy: &str, dns: &str) -> String {
    format!(
        r#"{{"base_domain":"apps.test","public_ip":"203.0.113.10","checks":[
            {{"name":"docker","status":"{docker}"}},
            {{"name":"sqlite","status":"{sqlite}"}},
            {{"name":"caddy","status":"{caddy}"}},
            {{"name":"dns","status":"{dns}","detail":"lof-x.apps.test resolves to 198.51.100.7, not 203.0.113.10"}}]}}"#
    )
}

/// Every row of `GET /server` ok.
fn all_ok() -> String {
    server_view("ok", "ok", "ok", "ok")
}

async fn healthy() -> Stub {
    healthy_with(&all_ok()).await
}

async fn healthy_with(server: &str) -> Stub {
    Stub::start()
        .await
        .route("GET", "/healthz", vec![json(200, r#"{"status":"ok"}"#)])
        .route("GET", "/readyz", vec![json(200, r#"{"status":"ready"}"#)])
        .route("GET", "/me", vec![json(200, ME)])
        .route("GET", "/server", vec![json(200, server)])
}

/// The rows of the first four checks, then `docker sqlite caddy dns`.
fn with_server(head: &[(&str, &str)], rows: [&str; 4]) -> Vec<(String, String)> {
    let mut all = expect(head);
    all.extend(expect(&[
        ("docker", rows[0]),
        ("sqlite", rows[1]),
        ("caddy", rows[2]),
        ("dns", rows[3]),
    ]));
    all
}

/// A logged-in CLI whose config file is `0600`, as `login` writes it.
fn private(url: &str) -> Cli {
    let cli = Cli::logged_in(url);
    std::fs::set_permissions(&cli.config, std::fs::Permissions::from_mode(0o600)).unwrap();
    cli
}

mod checks {
    use super::*;

    // TC: CLIX-09
    #[tokio::test]
    async fn lists_every_check_ok_and_exits_0_when_all_pass() {
        let stub = healthy().await;
        let cli = private(&stub.url);
        let outcome = cli.run(&["doctor"]).await;
        assert_eq!(
            (outcome.code, statuses(&outcome.out)),
            (
                0,
                with_server(
                    &[
                        ("config", "ok"),
                        ("server", "ok"),
                        ("session", "ok"),
                        ("ready", "ok")
                    ],
                    ["ok", "ok", "ok", "ok"]
                )
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIX-09
    #[tokio::test]
    async fn marks_ready_failed_and_exits_1_when_readyz_fails() {
        let stub = Stub::start()
            .await
            .route("GET", "/healthz", vec![json(200, r#"{"status":"ok"}"#)])
            .route("GET", "/readyz", vec![problem(503, "not ready")])
            .route("GET", "/me", vec![json(200, ME)])
            .route(
                "GET",
                "/server",
                vec![json(200, &server_view("ok", "ok", "fail", "ok"))],
            );
        let cli = private(&stub.url);
        let outcome = cli.run(&["doctor"]).await;
        assert_eq!(
            (outcome.code, statuses(&outcome.out)),
            (
                1,
                with_server(
                    &[
                        ("config", "ok"),
                        ("server", "ok"),
                        ("session", "ok"),
                        ("ready", "fail")
                    ],
                    ["ok", "ok", "fail", "ok"]
                )
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIX-09
    #[tokio::test]
    async fn marks_session_failed_and_exits_1_when_the_session_is_rejected() {
        let stub = Stub::start()
            .await
            .route("GET", "/healthz", vec![json(200, r#"{"status":"ok"}"#)])
            .route("GET", "/readyz", vec![json(200, r#"{"status":"ready"}"#)])
            .route("GET", "/me", vec![problem(401, "session expired")])
            .route("GET", "/server", vec![problem(401, "session expired")]);
        let cli = private(&stub.url);
        let outcome = cli.run(&["doctor"]).await;
        assert_eq!(
            (outcome.code, statuses(&outcome.out)),
            (
                1,
                with_server(
                    &[
                        ("config", "ok"),
                        ("server", "ok"),
                        ("session", "fail"),
                        ("ready", "ok")
                    ],
                    ["fail", "fail", "fail", "fail"]
                )
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIX-09 (an unreachable server is a red check, not exit 69)
    #[tokio::test]
    async fn lists_every_check_and_exits_1_when_the_server_is_unreachable() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let cli = private(&format!("http://127.0.0.1:{port}"));
        let outcome = cli.run(&["doctor"]).await;
        assert_eq!(
            (outcome.code, statuses(&outcome.out)),
            (
                1,
                with_server(
                    &[
                        ("config", "ok"),
                        ("server", "fail"),
                        ("session", "fail"),
                        ("ready", "fail")
                    ],
                    ["fail", "fail", "fail", "fail"]
                )
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIX-09 (doctor warns when the config is readable by others; a warning is not red)
    #[tokio::test]
    async fn warns_and_exits_0_when_the_config_is_wider_than_0600() {
        let stub = healthy().await;
        let cli = Cli::logged_in(&stub.url);
        std::fs::set_permissions(&cli.config, std::fs::Permissions::from_mode(0o644)).unwrap();
        let outcome = cli.run(&["doctor"]).await;
        assert_eq!(
            (outcome.code, statuses(&outcome.out)),
            (
                0,
                with_server(
                    &[
                        ("config", "warn"),
                        ("server", "ok"),
                        ("session", "ok"),
                        ("ready", "ok")
                    ],
                    ["ok", "ok", "ok", "ok"]
                )
            ),
            "{outcome:?}"
        );
    }
}

mod server_rows {
    use super::*;

    /// `(status, detail)` of the `dns` line.
    fn dns_line(out: &str) -> Option<String> {
        out.lines()
            .find(|line| line.starts_with("dns"))
            .map(str::to_owned)
    }

    // TC: CLIX-09 (docker apart from SQLite and Caddy admin)
    #[tokio::test]
    async fn marks_only_docker_failed_and_exits_1_when_docker_is_down() {
        let stub = healthy_with(&server_view("fail", "ok", "ok", "ok")).await;
        let cli = private(&stub.url);
        let outcome = cli.run(&["doctor"]).await;
        assert_eq!(
            (outcome.code, statuses(&outcome.out)),
            (
                1,
                with_server(
                    &[
                        ("config", "ok"),
                        ("server", "ok"),
                        ("session", "ok"),
                        ("ready", "ok")
                    ],
                    ["fail", "ok", "ok", "ok"]
                )
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIX-09 (a wildcard that resolves elsewhere warns with the
    // server's detail; a warning is not red)
    #[tokio::test]
    async fn warns_with_the_detail_and_exits_0_when_the_wildcard_resolves_elsewhere() {
        let stub = healthy_with(&server_view("ok", "ok", "ok", "warn")).await;
        let cli = private(&stub.url);
        let outcome = cli.run(&["doctor"]).await;
        let line = dns_line(&outcome.out).unwrap_or_default();
        assert_eq!(
            (
                outcome.code,
                line.contains("warn"),
                line.contains("198.51.100.7")
            ),
            (0, true, true),
            "{outcome:?}"
        );
    }

    // TC: CLIX-09 (no wildcard: managed domains cannot resolve)
    #[tokio::test]
    async fn exits_1_when_the_wildcard_is_missing() {
        let stub = healthy_with(&server_view("ok", "ok", "ok", "fail")).await;
        let cli = private(&stub.url);
        let outcome = cli.run(&["doctor"]).await;
        assert_eq!(
            (
                outcome.code,
                dns_line(&outcome.out).is_some_and(|l| l.contains("fail"))
            ),
            (1, true),
            "{outcome:?}"
        );
    }

    // TC: CLIX-09 (a server older than `GET /server` is a warning that
    // names the upgrade, not a red check)
    #[tokio::test]
    async fn warns_on_every_server_row_and_exits_0_when_the_server_predates_them() {
        let stub = Stub::start()
            .await
            .route("GET", "/healthz", vec![json(200, r#"{"status":"ok"}"#)])
            .route("GET", "/readyz", vec![json(200, r#"{"status":"ready"}"#)])
            .route("GET", "/me", vec![json(200, ME)]);
        let cli = private(&stub.url);
        let outcome = cli.run(&["doctor"]).await;
        assert_eq!(
            (
                outcome.code,
                statuses(&outcome.out),
                outcome.out.contains("upgrade")
            ),
            (
                0,
                with_server(
                    &[
                        ("config", "ok"),
                        ("server", "ok"),
                        ("session", "ok"),
                        ("ready", "ok")
                    ],
                    ["warn", "warn", "warn", "warn"]
                ),
                true
            ),
            "{outcome:?}"
        );
    }
}
