//! `lof artifact publish|update|open|ls|versions|share|unshare|rollback|rm`
//! against the recording stub of
//! the API (`CLIA-01…05`).
//!
//! Files are read relative to the working directory: a bare path inside
//! it is named by its path from there, one outside by its base name,
//! `name=path` by `name`. `publish` and `update` send
//! `multipart/form-data` (`html`, a `files` field per file whose
//! `filename` is its name in the artifact, `title`). Batch mode unless a
//! test gives the run a terminal; a browser is never opened (the host is
//! the fixture's fake).

use rstest::rstest;

use crate::support::{Cli, HostRun, Outcome, Stub, json, problem, say};

/// The artifact of every answer.
const ID: &str = "aB3dE6gH9jK2";

/// Its address.
const URL: &str = "https://west.test/a/aB3dE6gH9jK2";

/// The one-time link of the answers.
const LINK: &str =
    "https://west.test/api/v1/auth/links/otl_0123456789abcdefghijABCDEFGHIJ0123456789";

/// `201` of `POST /artifacts`: version 1, private, with its login link.
const PUBLISHED: &str = r#"{"id":"aB3dE6gH9jK2","url":"https://west.test/a/aB3dE6gH9jK2","title":"Report","shared":false,"expires_at":"2026-10-13T05:00:00Z","version":1,"created_at":"2026-10-13T00:00:00Z","updated_at":"2026-10-13T00:00:00Z","login_url":"https://west.test/api/v1/auth/links/otl_0123456789abcdefghijABCDEFGHIJ0123456789"}"#;

/// `200` of `PUT /artifacts/{id}`: version 2, no login link.
const UPDATED: &str = r#"{"id":"aB3dE6gH9jK2","url":"https://west.test/a/aB3dE6gH9jK2","title":"Second","shared":false,"expires_at":"2026-10-13T06:00:00Z","version":2,"created_at":"2026-10-13T00:00:00Z","updated_at":"2026-10-13T01:00:00Z"}"#;

/// `200` of `POST /artifacts/{id}/share`.
const SHARED: &str = r#"{"id":"aB3dE6gH9jK2","url":"https://west.test/a/aB3dE6gH9jK2","title":"Report","shared":true,"expires_at":null,"version":1,"created_at":"2026-10-13T00:00:00Z","updated_at":"2026-10-13T00:00:00Z"}"#;

/// `200` of `POST /artifacts/{id}/rollback {to: 1}`: version 3.
const ROLLED_BACK: &str = r#"{"id":"aB3dE6gH9jK2","url":"https://west.test/a/aB3dE6gH9jK2","title":"Report","shared":false,"expires_at":"2026-10-13T07:00:00Z","version":3,"created_at":"2026-10-13T00:00:00Z","updated_at":"2026-10-13T02:00:00Z"}"#;

/// `200` of `GET /artifacts`: one private, one shared.
const LISTED: &str = r#"[{"id":"aB3dE6gH9jK2","url":"https://west.test/a/aB3dE6gH9jK2","title":"Report","shared":false,"expires_at":"2026-10-13T05:00:00Z","version":2,"created_at":"2026-10-13T00:00:00Z","updated_at":"2026-10-13T01:00:00Z"},{"id":"Zz9Yy8Xx7Ww6","url":"https://west.test/a/Zz9Yy8Xx7Ww6","title":"Slides","shared":true,"expires_at":null,"version":1,"created_at":"2026-10-12T00:00:00Z","updated_at":"2026-10-12T00:00:00Z"}]"#;

/// `200` of `GET /artifacts/{id}/versions`, oldest first.
const VERSIONS: &str = r#"[{"n":1,"title":"Report","size":28,"created_at":"2026-10-13T00:00:00Z"},{"n":2,"title":"Second","size":15,"created_at":"2026-10-13T01:00:00Z"}]"#;

/// `201` of `POST /auth/links`.
const ISSUED: &str = r#"{"url":"https://west.test/api/v1/auth/links/otl_0123456789abcdefghijABCDEFGHIJ0123456789","expires_at":"2026-10-13T00:10:00Z"}"#;

/// The page every run publishes.
const PAGE: &str = "<h1>Report</h1>";

/// A stylesheet.
const CSS: &[u8] = b"h1{color:red}";

/// The 8-byte PNG signature: a file that is no UTF-8.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The program that opens a URL in the browser of this machine.
const BROWSER: &str = if cfg!(target_os = "macos") {
    "open"
} else {
    "xdg-open"
};

/// A stub that answers `method /api/v1<path>` with `status` and `body`.
async fn stub(method: &'static str, path: &str, status: u16, body: &str) -> Stub {
    Stub::start()
        .await
        .route(method, path, vec![json(status, body)])
}

/// `page.html`, `style.css` and `img/logo.png` in the working directory
/// of `cli`.
fn site(cli: &Cli) {
    std::fs::write(cli.cwd.join("page.html"), PAGE).unwrap();
    std::fs::write(cli.cwd.join("style.css"), CSS).unwrap();
    std::fs::create_dir_all(cli.cwd.join("img")).unwrap();
    std::fs::write(cli.cwd.join("img").join("logo.png"), PNG).unwrap();
}

/// Runs `args` with the site of [`site`] against `stub`.
async fn run_in_site(stub: &Stub, args: &[&str]) -> (Outcome, Cli) {
    let cli = Cli::logged_in(&stub.url);
    site(&cli);
    let outcome = cli.run(args).await;
    (outcome, cli)
}

/// The first request to `method path` the stub saw.
fn first(stub: &Stub, method: &str, path: &str) -> crate::support::Seen {
    stub.seen_at(method, path)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no {method} {path}: {:?}", stub.trail()))
}

mod publish {
    use super::*;

    // TC: CLIA-01 (the page goes as the multipart field `html`)
    #[tokio::test]
    async fn sends_page_as_html_field_when_publishing() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let (outcome, _cli) = run_in_site(&stub, &["artifact", "publish", "page.html"]).await;
        let sent = first(&stub, "POST", "/artifacts");
        assert_eq!(
            (outcome.code, sent.part("html")),
            (0, Some(PAGE.as_bytes().to_vec())),
            "{outcome:?}"
        );
    }

    // TC: CLIA-01 (with the session of the config)
    #[tokio::test]
    async fn sends_session_when_publishing() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let (_outcome, _cli) = run_in_site(&stub, &["artifact", "publish", "page.html"]).await;
        assert_eq!(
            first(&stub, "POST", "/artifacts").authorization.as_deref(),
            Some("Bearer lfs_session")
        );
    }

    // TC: CLIA-01 (`--files a,b`: one `files` field per file, named by
    // its path from the working directory)
    #[tokio::test]
    async fn sends_each_file_under_its_path_when_files_given() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let (outcome, _cli) = run_in_site(
            &stub,
            &[
                "artifact",
                "publish",
                "page.html",
                "--files",
                "style.css,img/logo.png",
            ],
        )
        .await;
        assert_eq!(
            (
                outcome.code,
                first(&stub, "POST", "/artifacts").uploads("files")
            ),
            (
                0,
                vec![
                    ("style.css".to_owned(), CSS.to_vec()),
                    ("img/logo.png".to_owned(), PNG.to_vec()),
                ]
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIA-01 (`--files` repeated adds up; `./` is no part of the
    // name)
    #[tokio::test]
    async fn sends_files_of_every_flag_when_repeated() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let (outcome, _cli) = run_in_site(
            &stub,
            &[
                "artifact",
                "publish",
                "page.html",
                "--files",
                "./style.css",
                "--files",
                "img/logo.png",
            ],
        )
        .await;
        let names: Vec<String> = first(&stub, "POST", "/artifacts")
            .uploads("files")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            (outcome.code, names),
            (0, vec!["style.css".to_owned(), "img/logo.png".to_owned()]),
            "{outcome:?}"
        );
    }

    // TC: CLIA-01 (a file outside the working directory is named by its
    // base name)
    #[tokio::test]
    async fn names_file_by_base_name_when_outside_working_directory() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let cli = Cli::logged_in(&stub.url).inside("site");
        site(&cli);
        std::fs::write(cli.root.join("logo.png"), PNG).unwrap();
        let outcome = cli
            .run(&["artifact", "publish", "page.html", "--files", "../logo.png"])
            .await;
        assert_eq!(
            (
                outcome.code,
                first(&stub, "POST", "/artifacts").uploads("files")
            ),
            (0, vec![("logo.png".to_owned(), PNG.to_vec())]),
            "{outcome:?}"
        );
    }

    // TC: CLIA-01 (`name=path` names the file in the artifact)
    #[tokio::test]
    async fn names_file_as_given_when_name_equals_path() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let cli = Cli::logged_in(&stub.url).inside("site");
        site(&cli);
        std::fs::write(cli.root.join("logo.png"), PNG).unwrap();
        let outcome = cli
            .run(&[
                "artifact",
                "publish",
                "page.html",
                "--files",
                "brand/logo.png=../logo.png",
            ])
            .await;
        assert_eq!(
            (
                outcome.code,
                first(&stub, "POST", "/artifacts").uploads("files")
            ),
            (0, vec![("brand/logo.png".to_owned(), PNG.to_vec())]),
            "{outcome:?}"
        );
    }

    // TC: CLIA-01 (`--title T` is the field `title`)
    #[tokio::test]
    async fn sends_title_when_given() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let (_outcome, _cli) = run_in_site(
            &stub,
            &["artifact", "publish", "page.html", "--title", "Q3 report"],
        )
        .await;
        assert_eq!(
            first(&stub, "POST", "/artifacts").param("title"),
            Some("Q3 report".to_owned())
        );
    }

    // TC: CLIA-01 (without `--title` the server takes the page's)
    #[tokio::test]
    async fn sends_no_title_when_left_out() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let (_outcome, _cli) = run_in_site(&stub, &["artifact", "publish", "page.html"]).await;
        assert_eq!(first(&stub, "POST", "/artifacts").part("title"), None);
    }

    // TC: CLIA-01 (stdout: the address, then the login link of the
    // private artifact)
    #[tokio::test]
    async fn prints_url_then_login_link_when_private() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let (outcome, _cli) = run_in_site(&stub, &["artifact", "publish", "page.html"]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{URL}\n{LINK}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-01 (the same on a terminal)
    #[tokio::test]
    async fn prints_url_then_login_link_when_interactive() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![]);
        site(&cli);
        let outcome = cli.run(&["artifact", "publish", "page.html"]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{URL}\n{LINK}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-01 (`--json`: the answer as received)
    #[tokio::test]
    async fn prints_body_as_received_when_json() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let (outcome, _cli) =
            run_in_site(&stub, &["--json", "artifact", "publish", "page.html"]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{PUBLISHED}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-01 (a file that cannot be read sends nothing; the error
    // names it)
    #[tokio::test]
    async fn sends_nothing_when_file_missing() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let (outcome, _cli) = run_in_site(
            &stub,
            &["artifact", "publish", "page.html", "--files", "nope.css"],
        )
        .await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.starts_with("error: ") && outcome.err.contains("nope.css"),
                stub.trail()
            ),
            (1, true, Vec::new()),
            "{outcome:?}"
        );
    }

    // TC: CLIA-01 (CLIX-01: batch mode lists what is missing)
    #[tokio::test]
    async fn exits_64_when_html_missing_in_batch() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let (outcome, cli) = run_in_site(&stub, &["artifact", "publish"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.as_str(),
                stub.trail(),
                cli.asked()
            ),
            (
                64,
                "error: missing required arguments: <html>\n",
                Vec::new(),
                Vec::<String>::new()
            )
        );
    }

    // TC: CLIA-01 (a terminal asks for the page instead)
    #[tokio::test]
    async fn asks_for_html_file_when_interactive() {
        let stub = stub("POST", "/artifacts", 201, PUBLISHED).await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("page.html")]);
        site(&cli);
        let outcome = cli.run(&["artifact", "publish"]).await;
        assert_eq!(
            (
                outcome.code,
                cli.asked().len(),
                first(&stub, "POST", "/artifacts").part("html")
            ),
            (0, 1, Some(PAGE.as_bytes().to_vec())),
            "{outcome:?}"
        );
    }

    // TC: CLIA-01 (CLIX-06: a refusal is `error: <detail>`, exit 1)
    #[tokio::test]
    async fn prints_detail_when_server_refuses() {
        let stub = Stub::start().await.route(
            "POST",
            "/artifacts",
            vec![problem(
                400,
                "files.../x.css: must not contain a `..` segment",
            )],
        );
        let (outcome, _cli) = run_in_site(&stub, &["artifact", "publish", "page.html"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), outcome.out.as_str()),
            (
                1,
                "error: files.../x.css: must not contain a `..` segment\n",
                ""
            )
        );
    }

    // TC: CLIA-01 (with `--json` the problem goes to stdout as received)
    #[tokio::test]
    async fn prints_problem_as_received_when_json_and_refused() {
        let stub = Stub::start().await.route(
            "POST",
            "/artifacts",
            vec![problem(403, "carol may not artifact.publish")],
        );
        let (outcome, _cli) =
            run_in_site(&stub, &["--json", "artifact", "publish", "page.html"]).await;
        let printed: serde_json::Value = serde_json::from_str(&outcome.out).unwrap_or_default();
        assert_eq!(
            (outcome.code, printed["detail"].as_str()),
            (1, Some("carol may not artifact.publish")),
            "{outcome:?}"
        );
    }
}

mod update {
    use super::*;

    const PATH: &str = "/artifacts/aB3dE6gH9jK2";

    // TC: CLIA-02 (`PUT /artifacts/{id}` with the page and its files)
    #[tokio::test]
    async fn puts_page_and_files_to_artifact_when_updating() {
        let stub = stub("PUT", PATH, 200, UPDATED).await;
        let (outcome, _cli) = run_in_site(
            &stub,
            &[
                "artifact",
                "update",
                ID,
                "page.html",
                "--files",
                "style.css",
            ],
        )
        .await;
        let sent = first(&stub, "PUT", PATH);
        assert_eq!(
            (outcome.code, sent.part("html"), sent.uploads("files")),
            (
                0,
                Some(PAGE.as_bytes().to_vec()),
                vec![("style.css".to_owned(), CSS.to_vec())]
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIA-02 (stdout: the address; an update carries no login link)
    #[tokio::test]
    async fn prints_url_when_updated() {
        let stub = stub("PUT", PATH, 200, UPDATED).await;
        let (outcome, _cli) = run_in_site(&stub, &["artifact", "update", ID, "page.html"]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{URL}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-02 (`--json`)
    #[tokio::test]
    async fn prints_body_as_received_when_json() {
        let stub = stub("PUT", PATH, 200, UPDATED).await;
        let (outcome, _cli) =
            run_in_site(&stub, &["--json", "artifact", "update", ID, "page.html"]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{UPDATED}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-02 (CLIX-01)
    #[tokio::test]
    async fn exits_64_when_id_and_html_missing_in_batch() {
        let stub = stub("PUT", PATH, 200, UPDATED).await;
        let (outcome, _cli) = run_in_site(&stub, &["artifact", "update"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), stub.trail()),
            (
                64,
                "error: missing required arguments: <id>, <html>\n",
                Vec::new()
            )
        );
    }
}

mod open {
    use super::*;

    const LINKS: &str = "/auth/links";

    // TC: CLIA-03 (a fresh one-time link for the target `artifact:<id>`)
    #[tokio::test]
    async fn asks_for_link_to_artifact_when_opening() {
        let stub = stub("POST", LINKS, 201, ISSUED).await;
        let cli = Cli::logged_in(&stub.url);
        let _outcome = cli.run(&["artifact", "open", ID]).await;
        assert_eq!(
            first(&stub, "POST", LINKS).json(),
            serde_json::json!({"target": "artifact:aB3dE6gH9jK2"})
        );
    }

    // TC: CLIA-03 (stdout: the link)
    #[tokio::test]
    async fn prints_link_when_opening() {
        let stub = stub("POST", LINKS, 201, ISSUED).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "open", ID]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{LINK}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-03 (no browser without `--browser`)
    #[tokio::test]
    async fn runs_no_browser_when_flag_absent() {
        let stub = stub("POST", LINKS, 201, ISSUED).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "open", ID]).await;
        assert_eq!(
            (outcome.code, cli.host_runs()),
            (0, Vec::new()),
            "{outcome:?}"
        );
    }

    // TC: CLIA-03 (`--browser` hands the link to the browser of this
    // machine: an argument vector, no shell)
    #[tokio::test]
    async fn hands_link_to_browser_when_flag_given() {
        let stub = stub("POST", LINKS, 201, ISSUED).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "open", ID, "--browser"]).await;
        assert_eq!(
            (outcome.code, cli.host_runs()),
            (
                0,
                vec![HostRun {
                    program: BROWSER.to_owned(),
                    args: vec![LINK.to_owned()],
                    stdin: None,
                }]
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIA-03 (a browser that cannot start leaves the printed link:
    // the run succeeds)
    #[tokio::test]
    async fn prints_link_and_succeeds_when_browser_fails() {
        let stub = stub("POST", LINKS, 201, ISSUED).await;
        let cli = Cli::logged_in(&stub.url).failing_host(BROWSER);
        let outcome = cli.run(&["artifact", "open", ID, "--browser"]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{LINK}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-03 (`--json`)
    #[tokio::test]
    async fn prints_body_as_received_when_json() {
        let stub = stub("POST", LINKS, 201, ISSUED).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--json", "artifact", "open", ID]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{ISSUED}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-03 (CLIX-06: another user's artifact)
    #[tokio::test]
    async fn prints_detail_when_refused() {
        let stub = Stub::start().await.route(
            "POST",
            LINKS,
            vec![problem(403, "artifact aB3dE6gH9jK2 is another user's")],
        );
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "open", ID]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (1, "error: artifact aB3dE6gH9jK2 is another user's\n")
        );
    }
}

mod ls {
    use super::*;

    // TC: CLIA-04 (a table: id, title, visibility, current version and
    // expiry)
    #[tokio::test]
    async fn prints_table_when_listing() {
        let stub = stub("GET", "/artifacts", 200, LISTED).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "ls"]).await;
        assert_eq!(
            (outcome.code, outcome.out.as_str()),
            (
                0,
                "ID            TITLE   VISIBILITY  VERSION  EXPIRES\n\
                 aB3dE6gH9jK2  Report  private     2        2026-10-13T05:00:00Z\n\
                 Zz9Yy8Xx7Ww6  Slides  shared      1        -\n"
            ),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-04 (`--json`)
    #[tokio::test]
    async fn prints_body_as_received_when_json() {
        let stub = stub("GET", "/artifacts", 200, LISTED).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--json", "artifact", "ls"]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{LISTED}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-04 (`--all` asks for every user's)
    #[rstest]
    #[case::own(&["artifact", "ls"], vec![])]
    #[case::all(&["artifact", "ls", "--all"], vec![("all".to_owned(), "true".to_owned())])]
    #[tokio::test]
    async fn queries_all_only_when_flag_given(
        #[case] args: &[&str],
        #[case] expected: Vec<(String, String)>,
    ) {
        let stub = stub("GET", "/artifacts", 200, LISTED).await;
        let cli = Cli::logged_in(&stub.url);
        let _outcome = cli.run(args).await;
        assert_eq!(first(&stub, "GET", "/artifacts").query_pairs(), expected);
    }
}

mod versions {
    use super::*;

    const PATH: &str = "/artifacts/aB3dE6gH9jK2/versions";

    // TC: CLIA-04 (`versions` as a table, oldest first as received)
    #[tokio::test]
    async fn prints_table_when_listing_versions() {
        let stub = stub("GET", PATH, 200, VERSIONS).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "versions", ID]).await;
        assert_eq!(
            (outcome.code, outcome.out.as_str()),
            (
                0,
                "VERSION  TITLE   SIZE  CREATED\n\
                 1        Report  28    2026-10-13T00:00:00Z\n\
                 2        Second  15    2026-10-13T01:00:00Z\n"
            ),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-04 (`versions --json`)
    #[tokio::test]
    async fn prints_body_as_received_when_json() {
        let stub = stub("GET", PATH, 200, VERSIONS).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--json", "artifact", "versions", ID]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{VERSIONS}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-04 (CLIX-01)
    #[tokio::test]
    async fn exits_64_when_id_missing_in_batch() {
        let stub = stub("GET", PATH, 200, VERSIONS).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "versions"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), stub.trail()),
            (64, "error: missing required arguments: <id>\n", Vec::new())
        );
    }

    // TC: CLIA-04 (an unknown artifact: CLIX-06)
    #[tokio::test]
    async fn prints_detail_when_artifact_unknown() {
        let stub = Stub::start().await.route(
            "GET",
            PATH,
            vec![problem(404, "artifact aB3dE6gH9jK2 not found")],
        );
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "versions", ID]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (1, "error: artifact aB3dE6gH9jK2 not found\n")
        );
    }
}

mod rollback {
    use super::*;

    const PATH: &str = "/artifacts/aB3dE6gH9jK2/rollback";

    // TC: CLIA-04 (`{to: N}`)
    #[tokio::test]
    async fn posts_version_to_copy_when_rolling_back() {
        let stub = stub("POST", PATH, 200, ROLLED_BACK).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "rollback", ID, "--to", "1"]).await;
        assert_eq!(
            (outcome.code, first(&stub, "POST", PATH).json()),
            (0, serde_json::json!({"to": 1})),
            "{outcome:?}"
        );
    }

    // TC: CLIA-04 (`--json`)
    #[tokio::test]
    async fn prints_body_as_received_when_json() {
        let stub = stub("POST", PATH, 200, ROLLED_BACK).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli
            .run(&["--json", "artifact", "rollback", ID, "--to", "1"])
            .await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{ROLLED_BACK}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-04 (CLIX-01)
    #[tokio::test]
    async fn exits_64_when_to_missing_in_batch() {
        let stub = stub("POST", PATH, 200, ROLLED_BACK).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "rollback", ID]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), stub.trail()),
            (64, "error: missing required arguments: --to\n", Vec::new())
        );
    }
}

mod visibility {
    use super::*;

    // TC: CLIA-04 (`share <id>` and `unshare <id>`: the routes of spec
    // 8.2, the answer as received under `--json`)
    #[rstest]
    #[case::share("share", "/artifacts/aB3dE6gH9jK2/share")]
    #[case::unshare("unshare", "/artifacts/aB3dE6gH9jK2/unshare")]
    #[tokio::test]
    async fn posts_to_route_and_prints_body_when_json(
        #[case] word: &str,
        #[case] path: &'static str,
    ) {
        let stub = stub("POST", path, 200, SHARED).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--json", "artifact", word, ID]).await;
        assert_eq!(
            (outcome.code, outcome.out, stub.trail()),
            (
                0,
                format!("{SHARED}\n"),
                vec![("POST".to_owned(), path.to_owned())]
            ),
            "{:?}",
            outcome.err
        );
    }
}

mod rm {
    use super::*;

    // TC: CLIA-05 (`rm <id>`: the whole artifact)
    #[tokio::test]
    async fn deletes_artifact_when_no_version_given() {
        let stub = stub(
            "DELETE",
            "/artifacts/aB3dE6gH9jK2",
            200,
            r#"{"deleted":"aB3dE6gH9jK2"}"#,
        )
        .await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "rm", ID]).await;
        assert_eq!(
            (outcome.code, outcome.out.as_str(), stub.trail()),
            (
                0,
                "removed aB3dE6gH9jK2\n",
                vec![("DELETE".to_owned(), "/artifacts/aB3dE6gH9jK2".to_owned())]
            ),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-05 (`rm <id> --version N`: that version alone)
    #[tokio::test]
    async fn deletes_only_version_when_version_given() {
        let path = "/artifacts/aB3dE6gH9jK2/versions/1";
        let stub = stub("DELETE", path, 200, r#"{"deleted":"aB3dE6gH9jK2/v/1"}"#).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "rm", ID, "--version", "1"]).await;
        assert_eq!(
            (outcome.code, outcome.out.as_str(), stub.trail()),
            (
                0,
                "removed version 1 of aB3dE6gH9jK2\n",
                vec![("DELETE".to_owned(), path.to_owned())]
            ),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-05 (ART-10: the current version is refused, CLIX-06)
    #[tokio::test]
    async fn prints_detail_when_version_is_current() {
        let stub = Stub::start().await.route(
            "DELETE",
            "/artifacts/aB3dE6gH9jK2/versions/2",
            vec![problem(
                409,
                "version 2 is the current version of artifact aB3dE6gH9jK2; roll back or update first",
            )],
        );
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "rm", ID, "--version", "2"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (
                1,
                "error: version 2 is the current version of artifact aB3dE6gH9jK2; roll back or update first\n"
            )
        );
    }

    // TC: CLIA-05 (`--json`)
    #[tokio::test]
    async fn prints_body_as_received_when_json() {
        let body = r#"{"deleted":"aB3dE6gH9jK2"}"#;
        let stub = stub("DELETE", "/artifacts/aB3dE6gH9jK2", 200, body).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--json", "artifact", "rm", ID]).await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{body}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIA-05 (CLIX-01)
    #[tokio::test]
    async fn exits_64_when_id_missing_in_batch() {
        let stub = Stub::start().await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["artifact", "rm"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), stub.trail()),
            (64, "error: missing required arguments: <id>\n", Vec::new())
        );
    }
}
