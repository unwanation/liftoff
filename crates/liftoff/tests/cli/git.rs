//! `lof git connect github` (GIT-01, GIT-02) against the local stub of the API, in both modes.
//!
//! Without `--token` the CLI starts the forge's device flow through
//! `POST /git/connections/github.com/device` (which `git.connect` admits,
//! unlike the admin's `GET /modules`): when the `git.github` module has
//! an OAuth application it polls `…/device/poll` (the link and the code
//! on stderr, the poll every `interval`, raised on `slow_down`, until it
//! is no longer `202`); when the start answers `400` (no OAuth
//! application) it asks for a token, with a hint about the scopes it
//! needs, and sends it to `POST /git/connections`. The host of GitHub is
//! `github.com` unless `--host` says otherwise.

use std::time::Duration;

use crate::support::{Answer, Cli, Seen, Stub, json, say};

/// `POST /git/connections/github.com/device` of a module without an
/// OAuth application.
const NO_OAUTH: &str = r#"{"status":400,"title":"Bad Request","detail":"provider: the module git.github has no OAuth application (its config client_id): connect with a token instead, `lof git connect github --host github.com --token <token>`"}"#;

/// `POST /git/connections/github.com/device`.
const DEVICE: &str = r#"{"device_code":"dc-1","user_code":"WDJB-MJHT","verification_uri":"https://github.com/login/device","interval":5,"expires_in":900}"#;

/// `…/device/poll` before the person confirmed.
const PENDING: &str = r#"{"status":"pending"}"#;

/// The connection the device flow stores.
const OAUTH_CONNECTION: &str = r#"{"provider":"github","host":"github.com","account_login":"alice","kind":"oauth","expires_at":null}"#;

/// The connection a token stores.
const TOKEN_CONNECTION: &str = r#"{"provider":"github","host":"github.com","account_login":"alice","kind":"token","expires_at":null}"#;

const DEVICE_PATH: &str = "/git/connections/github.com/device";
const POLL_PATH: &str = "/git/connections/github.com/device/poll";

/// Whether the stub's `git.github` has an OAuth application.
const WITH_OAUTH: bool = true;
const WITHOUT_OAUTH: bool = false;

/// A stub whose `git.github` has an OAuth application when `oauth`, its
/// polls answering `polls`, with `POST /git/connections`; `GET /modules`
/// is refused, as for anyone but the admin.
async fn stub_polling(oauth: bool, polls: Vec<Answer>) -> Stub {
    let start = if oauth {
        json(200, DEVICE)
    } else {
        json(400, NO_OAUTH)
    };
    Stub::start()
        .await
        .route(
            "GET",
            "/modules",
            vec![json(403, r#"{"status":403,"title":"Forbidden"}"#)],
        )
        .route("POST", DEVICE_PATH, vec![start])
        .route("POST", POLL_PATH, polls)
        .route(
            "POST",
            "/git/connections",
            vec![json(201, TOKEN_CONNECTION)],
        )
}

/// [`stub_polling`] with one pending poll, then the connection.
async fn stub(oauth: bool) -> Stub {
    stub_polling(oauth, vec![json(202, PENDING), json(201, OAUTH_CONNECTION)]).await
}

mod device_flow {
    use super::*;

    // TC: GIT-01 (with `client_id`, the device flow)
    #[tokio::test]
    async fn prints_connection_when_module_has_oauth_and_person_confirms() {
        let stub = stub(WITH_OAUTH).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["git", "connect", "github"]).await;
        assert_eq!(
            (outcome.code, outcome.out.as_str()),
            (0, "connected github.com as alice\n"),
            "{outcome:?}"
        );
    }

    // TC: GIT-01 (the link and the code go to stderr: the CLI owns stdout)
    #[tokio::test]
    async fn tells_link_and_code_on_stderr_when_device_flow_starts() {
        let stub = stub(WITH_OAUTH).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["git", "connect", "github"]).await;
        assert!(
            outcome.err.contains("https://github.com/login/device")
                && outcome.err.contains("WDJB-MJHT")
                && !outcome.out.contains("WDJB-MJHT"),
            "{outcome:?}"
        );
    }

    // TC: GIT-01 (start, poll until it is no longer `202`; no token sent)
    #[tokio::test]
    async fn starts_then_polls_until_created_when_module_has_oauth() {
        let stub = stub(WITH_OAUTH).await;
        let cli = Cli::logged_in(&stub.url);
        let _outcome = cli.run(&["git", "connect", "github"]).await;
        let started = stub.seen_at("POST", DEVICE_PATH);
        let polls = stub.seen_at("POST", POLL_PATH);
        assert_eq!(
            (
                started.iter().map(Seen::json).collect::<Vec<_>>(),
                polls.iter().map(Seen::json).collect::<Vec<_>>(),
                stub.seen_at("POST", "/git/connections").len(),
            ),
            (
                vec![serde_json::json!({"provider": "github"})],
                vec![
                    serde_json::json!({"provider": "github", "device_code": "dc-1"}),
                    serde_json::json!({"provider": "github", "device_code": "dc-1"}),
                ],
                0,
            )
        );
    }

    // TC: GIT-01 (the device flow is for every user who may `git.connect`, not only for the admin who may list the modules)
    #[tokio::test]
    async fn connects_without_listing_modules_when_caller_may_not_list_them() {
        let stub = stub(WITH_OAUTH).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["git", "connect", "github"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.out.as_str(),
                stub.seen_at("GET", "/modules").len()
            ),
            (0, "connected github.com as alice\n", 0),
            "{outcome:?}"
        );
    }

    // TC: GIT-01 (polls every `interval` of the start)
    #[tokio::test]
    async fn pauses_interval_before_each_poll_when_pending() {
        let stub = stub(WITH_OAUTH).await;
        let cli = Cli::logged_in(&stub.url);
        let _outcome = cli.run(&["git", "connect", "github"]).await;
        assert_eq!(cli.slept(), vec![Duration::from_secs(5); 2]);
    }

    // TC: GIT-01 (RFC 8628 3.5 `slow_down`: the interval the forge names
    // holds for every later poll)
    #[tokio::test]
    async fn pauses_forge_interval_after_slow_down_when_forge_names_one() {
        let stub = stub_polling(
            WITH_OAUTH,
            vec![
                json(202, r#"{"status":"slow_down","interval":12}"#),
                json(202, PENDING),
                json(201, OAUTH_CONNECTION),
            ],
        )
        .await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["git", "connect", "github"]).await;
        assert_eq!(
            (outcome.code, cli.slept()),
            (
                0,
                vec![
                    Duration::from_secs(5),
                    Duration::from_secs(12),
                    Duration::from_secs(12),
                ]
            ),
            "{outcome:?}"
        );
    }

    // TC: GIT-01 (RFC 8628 3.5: without a new interval, 5 s more on each
    // `slow_down`)
    #[tokio::test]
    async fn pauses_five_seconds_more_after_each_slow_down_when_forge_names_none() {
        let slow_down = json(202, r#"{"status":"slow_down"}"#);
        let stub = stub_polling(
            WITH_OAUTH,
            vec![slow_down.clone(), slow_down, json(201, OAUTH_CONNECTION)],
        )
        .await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["git", "connect", "github"]).await;
        assert_eq!(
            (outcome.code, cli.slept()),
            (
                0,
                vec![
                    Duration::from_secs(5),
                    Duration::from_secs(10),
                    Duration::from_secs(15),
                ]
            ),
            "{outcome:?}"
        );
    }

    // TC: GIT-01 (a refusal other than "no OAuth application" is the
    // answer: no token is asked for a forge the caller may not connect)
    #[tokio::test]
    async fn fails_without_asking_token_when_device_start_is_forbidden() {
        let stub = Stub::start().await.route(
            "POST",
            DEVICE_PATH,
            vec![json(
                403,
                r#"{"status":403,"title":"Forbidden","detail":"bob may not git.connect"}"#,
            )],
        );
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("ghp_pasted")]);
        let outcome = cli.run(&["git", "connect", "github"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.contains("bob may not git.connect"),
                cli.asked_hidden().len(),
                stub.seen_at("POST", "/git/connections").len(),
            ),
            (1, true, 0, 0),
            "{outcome:?}"
        );
    }

    // TC: GIT-01 (a token given is sent as is, even when the module could run the device flow)
    #[tokio::test]
    async fn posts_token_without_device_flow_when_token_given() {
        let stub = stub(WITH_OAUTH).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli
            .run(&["git", "connect", "github", "--token", "ghp_given"])
            .await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            (
                stub.seen_at("POST", "/git/connections")
                    .iter()
                    .map(Seen::json)
                    .collect::<Vec<_>>(),
                stub.seen_at("POST", DEVICE_PATH).len(),
            ),
            (
                vec![
                    serde_json::json!({"provider": "github", "host": "github.com", "token": "ghp_given"})
                ],
                0,
            )
        );
    }
}

mod token {
    use super::*;

    // TC: GIT-01 (without `client_id` the CLI asks for
    // a token; the host of GitHub is not asked)
    #[tokio::test]
    async fn asks_only_for_token_and_posts_it_when_module_has_no_oauth() {
        let stub = stub(WITHOUT_OAUTH).await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("ghp_pasted")]);
        let outcome = cli.run(&["git", "connect", "github"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            (
                cli.asked().len(),
                cli.asked_hidden().len(),
                stub.seen_at("POST", "/git/connections")
                    .iter()
                    .map(Seen::json)
                    .collect::<Vec<_>>(),
                stub.seen_at("POST", DEVICE_PATH).len(),
            ),
            (
                1,
                1,
                vec![
                    serde_json::json!({"provider": "github", "host": "github.com", "token": "ghp_pasted"})
                ],
                1,
            ),
            "asked {:?}",
            cli.asked()
        );
    }

    // TC: GIT-01 (the hint names the scopes the token needs, GIT-02)
    #[tokio::test]
    async fn hints_needed_scopes_when_module_has_no_oauth() {
        let stub = stub(WITHOUT_OAUTH).await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("ghp_pasted")]);
        let outcome = cli.run(&["git", "connect", "github"]).await;
        assert!(
            outcome.err.contains("repo") && outcome.err.contains("read:packages"),
            "{outcome:?}"
        );
    }

    // TC: GIT-01 (in batch mode a missing token is exit 64 with the flag listed, before anything is stored; the device start that answered `400` is all that was sent)
    #[tokio::test]
    async fn exits_64_listing_token_when_module_has_no_oauth_without_tty() {
        let stub = stub(WITHOUT_OAUTH).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["git", "connect", "github"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.contains("--token"),
                outcome.err.contains("--host"),
                stub.seen_at("POST", "/git/connections").len(),
                stub.seen_at("POST", DEVICE_PATH).len(),
            ),
            (64, true, false, 0, 1),
            "{outcome:?}"
        );
    }
}

/// `lof git connect gitlab`: the same paths as
/// GitHub's, the host `gitlab.com` unless `--host` names a self-hosted
/// GitLab, and a token hint naming the scope `api`.
mod gitlab {
    use super::*;

    const GITLAB_DEVICE_PATH: &str = "/git/connections/gitlab.com/device";
    const GITLAB_POLL_PATH: &str = "/git/connections/gitlab.com/device/poll";

    /// `POST /git/connections/gitlab.com/device` of a module without an
    /// OAuth application.
    const GITLAB_NO_OAUTH: &str = r#"{"status":400,"title":"Bad Request","detail":"provider: the module git.gitlab has no OAuth application (its config client_id): connect with a token instead, `lof git connect gitlab --host gitlab.com --token <token>`"}"#;

    /// `POST /git/connections/gitlab.com/device`.
    const GITLAB_DEVICE: &str = r#"{"device_code":"dc-1","user_code":"0A44L90H","verification_uri":"https://gitlab.com/oauth/device","interval":5,"expires_in":300}"#;

    const GITLAB_OAUTH_CONNECTION: &str = r#"{"provider":"gitlab","host":"gitlab.com","account_login":"alice","kind":"oauth","expires_at":null}"#;

    const GITLAB_TOKEN_CONNECTION: &str = r#"{"provider":"gitlab","host":"gitlab.com","account_login":"alice","kind":"token","expires_at":null}"#;

    /// A stub whose `git.gitlab` has an OAuth application when `oauth`.
    async fn gitlab_stub(oauth: bool) -> Stub {
        let start = if oauth {
            json(200, GITLAB_DEVICE)
        } else {
            json(400, GITLAB_NO_OAUTH)
        };
        Stub::start()
            .await
            .route("POST", GITLAB_DEVICE_PATH, vec![start])
            .route(
                "POST",
                GITLAB_POLL_PATH,
                vec![json(202, PENDING), json(201, GITLAB_OAUTH_CONNECTION)],
            )
            .route(
                "POST",
                "/git/connections",
                vec![json(201, GITLAB_TOKEN_CONNECTION)],
            )
    }

    // TC: GIT-14 (`host` defaults to `gitlab.com`; with
    // `client_id`, the device flow there)
    #[tokio::test]
    async fn connects_gitlab_com_by_device_flow_when_no_host_given() {
        let stub = gitlab_stub(WITH_OAUTH).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["git", "connect", "gitlab"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.out.as_str(),
                stub.seen_at("POST", GITLAB_DEVICE_PATH)
                    .iter()
                    .map(Seen::json)
                    .collect::<Vec<_>>(),
            ),
            (
                0,
                "connected gitlab.com as alice\n",
                vec![serde_json::json!({"provider": "gitlab"})],
            ),
            "{outcome:?}"
        );
    }

    // TC: GIT-14 (without `client_id` the CLI asks for a token of
    // `gitlab.com` only, not for the host)
    #[tokio::test]
    async fn asks_only_for_token_of_gitlab_com_when_module_has_no_oauth() {
        let stub = gitlab_stub(WITHOUT_OAUTH).await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("glpat-pasted")]);
        let outcome = cli.run(&["git", "connect", "gitlab"]).await;
        assert_eq!(
            (
                outcome.code,
                cli.asked().len(),
                stub.seen_at("POST", "/git/connections")
                    .iter()
                    .map(Seen::json)
                    .collect::<Vec<_>>(),
            ),
            (
                0,
                1,
                vec![
                    serde_json::json!({"provider": "gitlab", "host": "gitlab.com", "token": "glpat-pasted"})
                ],
            ),
            "{outcome:?} asked {:?}",
            cli.asked()
        );
    }

    // TC: GIT-14 (the hint names the scope a GitLab token needs: `api`)
    #[tokio::test]
    async fn hints_api_scope_when_module_has_no_oauth() {
        let stub = gitlab_stub(WITHOUT_OAUTH).await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("glpat-pasted")]);
        let outcome = cli.run(&["git", "connect", "gitlab"]).await;
        assert!(
            outcome
                .err
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|word| word == "api"),
            "{outcome:?}"
        );
    }
}
