//! TC: CLIX-02. On a terminal the same call asks for what is missing and
//! then does exactly what the batch call with every flag does: the same
//! requests, the same output.

use rstest::rstest;

use crate::fixtures::full_stub;
use crate::support::{Cli, Reply, Seen, Stub, json, say};

const SSH_REMOTE: &str = "git@git.example.com:acme/shop.git";

/// Pairs of a query or a multipart body.
type Pairs = Vec<(String, String)>;

/// What a request carries that a command decides: method, path, query and
/// body (JSON value, or the multipart fields).
fn request(seen: &Seen) -> (String, String, Pairs, serde_json::Value, Pairs) {
    (
        seen.method.clone(),
        seen.path.clone(),
        seen.query_pairs(),
        seen.json(),
        seen.fields.clone(),
    )
}

/// Runs `batch` without a terminal, then `asked` on a terminal answering
/// `replies`, both against `stub`, and returns what each sent and printed.
async fn both_modes(
    stub: &Stub,
    batch: &[&str],
    asked: &[&str],
    replies: Vec<Reply>,
) -> (Vec<Seen>, Vec<Seen>, String, String, Cli) {
    let batch_cli = Cli::logged_in(&stub.url);
    let first = batch_cli.run(batch).await;
    assert_eq!(first.code, 0, "batch: {first:?}");
    let sent_in_batch = stub.seen();
    let interactive_cli = Cli::logged_in(&stub.url).terminal(replies);
    let second = interactive_cli.run(asked).await;
    assert_eq!(second.code, 0, "interactive: {second:?}");
    let sent_interactively = stub.seen().split_off(sent_in_batch.len());
    (
        sent_in_batch,
        sent_interactively,
        first.out,
        second.out,
        interactive_cli,
    )
}

mod asks_then_runs {
    use super::*;

    // TC: CLIX-02
    #[tokio::test]
    async fn asks_name_and_branch_then_creates_the_environment() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("qa"), say("develop")]);
        let outcome = cli.run(&["--project", "liftoff", "env", "add"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(cli.asked().len(), 2, "{:?}", cli.asked());
        let sent = stub.seen_at("POST", "/projects/liftoff/environments");
        let body = sent.first().map(Seen::json).unwrap_or_default();
        assert_eq!(
            (body["name"].as_str(), body["branch"].as_str()),
            (Some("qa"), Some("develop"))
        );
    }

    // TC: CLIX-02 (only what is missing is asked)
    #[tokio::test]
    async fn asks_only_the_branch_when_the_name_is_given() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("develop")]);
        let outcome = cli.run(&["--project", "liftoff", "env", "add", "qa"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(cli.asked().len(), 1, "{:?}", cli.asked());
    }

    // TC: CLIX-02 (a refused confirmation sends nothing)
    #[tokio::test]
    async fn sends_nothing_when_the_removal_is_not_confirmed() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![Reply::No]);
        let outcome = cli.run(&["--project", "liftoff", "env", "rm", "qa"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (1, "error: not confirmed; nothing was changed\n")
        );
        assert_eq!(cli.asked().len(), 1, "the removal is confirmed first");
        assert!(
            stub.seen_at("DELETE", "/projects/liftoff/environments/qa")
                .is_empty(),
            "{:?}",
            stub.trail()
        );
    }

    // TC: CLIX-02 (Esc at a question ends the run without a request)
    #[tokio::test]
    async fn sends_nothing_when_the_user_cancels_a_question() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![]);
        let outcome = cli.run(&["--project", "liftoff", "env", "add"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (1, "error: cancelled\n")
        );
        assert_eq!(cli.asked().len(), 1, "the first question was asked");
        assert!(stub.seen().is_empty(), "{:?}", stub.trail());
    }

    // TC: CLIX-02 (an empty answer is a missing argument)
    #[tokio::test]
    async fn exits_64_when_an_answer_is_empty() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("  ")]);
        let outcome = cli.run(&["--project", "liftoff", "env", "add"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (64, "error: <name> is required\n")
        );
        assert!(stub.seen().is_empty(), "{:?}", stub.trail());
    }

    // TC: CLIX-02 (a typed answer that does not parse is a wrong argument)
    #[tokio::test]
    async fn exits_64_when_the_port_answer_is_not_a_number() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url)
            .in_repo(SSH_REMOTE)
            .terminal(vec![say("web"), say("http")]);
        let outcome = cli.run(&["create"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (64, "error: --port: not a valid value: \"http\"\n")
        );
        assert!(stub.seen().is_empty(), "{:?}", stub.trail());
        assert_eq!(cli.cwd_text("liftoff.toml"), None);
    }

    // TC: CLIX-02 (a choice is picked from a list)
    #[tokio::test]
    async fn picks_the_provider_when_git_connect_has_none() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![
            Reply::Pick(0),
            say("git.example.com"),
            say("fjt_token"),
        ]);
        let outcome = cli.run(&["git", "connect"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            cli.asked().first().map(String::as_str),
            Some(r#"Forge ["forgejo", "github", "gitlab"]"#)
        );
        let sent = stub.seen_at("POST", "/git/connections");
        assert_eq!(
            sent.first().and_then(|seen| seen.param("provider")),
            Some("forgejo".to_owned())
        );
    }

    // TC: CLIX-02 (a choice is picked from a list)
    #[tokio::test]
    async fn picks_the_default_server_when_server_use_has_no_name() {
        let cli = two_servers().terminal(vec![Reply::Pick(0)]);
        let outcome = cli.run(&["server", "use"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            cli.asked(),
            vec![r#"Default server ["staging", "west"]"#.to_owned()]
        );
        assert_eq!(
            cli.config_toml()
                .get("default_server")
                .and_then(toml::Value::as_str),
            Some("staging")
        );
    }
}

/// A config with `west` (the default) and `staging`; nothing is asked of
/// them.
fn two_servers() -> Cli {
    let cli = Cli::new();
    cli.write_config(
        "default_server = \"west\"\n\n[servers.west]\nurl = \"http://127.0.0.1:9\"\ntoken = \"lfs_primary\"\n\n[servers.staging]\nurl = \"http://127.0.0.1:9\"\ntoken = \"lfs_staging\"\n",
    );
    cli
}

mod same_result_in_both_modes {
    use super::*;

    // TC: CLIX-02 (one command, one result in both modes)
    #[rstest]
    #[case::env_add(
        &["--project", "liftoff", "env", "add", "qa", "--branch", "develop"],
        &["--project", "liftoff", "env", "add"],
        vec![say("qa"), say("develop")],
    )]
    #[case::env_rm(
        &["--project", "liftoff", "env", "rm", "qa", "--yes"],
        &["--project", "liftoff", "env", "rm", "qa"],
        vec![Reply::Yes],
    )]
    #[case::bind(
        &["--project", "liftoff", "bind", "web:3000", "shop.apps.example.com"],
        &["--project", "liftoff", "bind"],
        vec![say("web:3000"), say("shop.apps.example.com")],
    )]
    #[case::domain_add(
        &["--project", "liftoff", "domain", "add", "shop"],
        &["--project", "liftoff", "domain", "add"],
        vec![say("shop")],
    )]
    #[case::secret_set(
        &["--project", "liftoff", "secret", "set", "API_KEY=s3cr3t"],
        &["--project", "liftoff", "secret", "set"],
        vec![say("API_KEY"), say("s3cr3t")],
    )]
    #[case::secret_rollback(
        &["--project", "liftoff", "secret", "rollback", "API_KEY", "--to", "1"],
        &["--project", "liftoff", "secret", "rollback", "API_KEY"],
        vec![say("1")],
    )]
    #[case::git_connect(
        &["git", "connect", "forgejo", "--host", "git.example.com", "--token", "fjt_token"],
        &["git", "connect", "forgejo"],
        vec![say("git.example.com"), say("fjt_token")],
    )]
    #[case::git_connect_picked(
        &["git", "connect", "forgejo", "--host", "git.example.com", "--token", "fjt_token"],
        &["git", "connect"],
        vec![Reply::Pick(0), say("git.example.com"), say("fjt_token")],
    )]
    #[case::git_disconnect(
        &["git", "disconnect", "git.example.com"],
        &["git", "disconnect"],
        vec![say("git.example.com")],
    )]
    #[case::deploys_show(
        &["deploys", "show", "01J000000000000000000000D2"],
        &["deploys", "show"],
        vec![say("01J000000000000000000000D2")],
    )]
    #[case::deploys_wait(
        &["deploys", "wait", "01J000000000000000000000D2"],
        &["deploys", "wait"],
        vec![say("01J000000000000000000000D2")],
    )]
    #[case::admin_sessions_revoke(
        &["admin", "sessions", "revoke", "bob"],
        &["admin", "sessions", "revoke"],
        vec![say("bob")],
    )]
    #[tokio::test]
    async fn sends_the_same_requests_and_prints_the_same_when_asked(
        #[case] batch: &[&str],
        #[case] asked: &[&str],
        #[case] replies: Vec<Reply>,
    ) {
        let stub = full_stub().await;
        let (in_batch, interactively, batch_out, interactive_out, cli) =
            both_modes(&stub, batch, asked, replies).await;
        assert!(!in_batch.is_empty(), "the command calls the API");
        assert!(!cli.asked().is_empty(), "the terminal run asked");
        assert_eq!(
            interactively.iter().map(request).collect::<Vec<_>>(),
            in_batch.iter().map(request).collect::<Vec<_>>()
        );
        assert_eq!(interactive_out, batch_out);
    }

    // TC: CLIX-02 (a picked server is the named one)
    #[tokio::test]
    async fn makes_the_same_server_the_default_when_it_is_picked() {
        let batch = two_servers();
        let first = batch.run(&["server", "use", "staging"]).await;
        let asked = two_servers().terminal(vec![Reply::Pick(0)]);
        let second = asked.run(&["server", "use"]).await;
        assert_eq!((second.code, &second.out), (first.code, &first.out));
        assert_eq!(asked.config_text(), batch.config_text());
    }

    // TC: CLIX-02 (login asks the URL and the token)
    #[tokio::test]
    async fn logs_in_the_same_way_when_url_and_token_are_asked() {
        let stub = full_stub().await;
        let (in_batch, interactively, _, _, cli) = both_modes(
            &stub,
            &["login", &stub.url, "--token", "lft_setup"],
            &["login"],
            vec![say(&stub.url), say("lft_setup")],
        )
        .await;
        // Without `--token` the CLI first tries the `oidc` module's device
        // flow; the server has none, so it asks the token.
        let without_probe = |seen: &&Seen| seen.path != "/api/v1/m/oidc/device";
        assert_eq!(
            interactively
                .iter()
                .filter(without_probe)
                .map(request)
                .collect::<Vec<_>>(),
            in_batch.iter().map(request).collect::<Vec<_>>()
        );
        assert_eq!(cli.asked().len(), 2, "{:?}", cli.asked());
    }

    // TC: CLIX-02 (a command with nothing to ask runs the same and asks
    // nothing on a terminal)
    #[rstest]
    #[case::whoami(&["whoami"], "GET", "/me")]
    #[case::logout(&["logout"], "DELETE", "/auth/session")]
    #[case::deploys(&["deploys"], "GET", "/projects/liftoff/environments/prod/deploys")]
    #[case::redeploy(&["redeploy"], "POST", "/projects/liftoff/environments/prod/redeploy")]
    #[case::deploy(&["deploy", "--tag", "v1"], "POST", "/deploys")]
    #[case::logs(&["logs"], "GET", "/projects/liftoff/environments/prod/logs")]
    #[case::env_ls(&["env", "ls"], "GET", "/projects/liftoff/environments")]
    #[case::env_stop(&["env", "stop"], "POST", "/projects/liftoff/environments/prod/stop")]
    #[case::env_start(&["env", "start"], "POST", "/projects/liftoff/environments/prod/start")]
    #[case::domain_ls(&["domain", "ls"], "GET", "/domains")]
    #[case::domain_rm(&["domain", "rm", "shop.apps.example.com"], "DELETE", "/domains/shop.apps.example.com")]
    #[case::unbind(&["unbind", "shop.apps.example.com"], "DELETE", "/bindings/shop.apps.example.com")]
    #[case::secret_ls(&["secret", "ls"], "GET", "/secrets")]
    #[case::secret_get(&["secret", "get", "API_KEY"], "GET", "/secrets/API_KEY")]
    #[case::secret_rm(&["secret", "rm", "API_KEY"], "DELETE", "/secrets/API_KEY")]
    #[case::secret_pull(&["secret", "pull"], "GET", "/secrets/resolve")]
    #[case::secret_history(&["secret", "history", "API_KEY"], "GET", "/secrets/API_KEY/versions")]
    #[case::token_rotate(&["token", "rotate"], "POST", "/projects/liftoff/token/rotate")]
    #[case::token_snippet(&["token", "snippet"], "GET", "/projects/liftoff/snippet")]
    #[case::admin_token(&["admin", "token"], "POST", "/admin/token")]
    #[case::admin_users(&["admin", "users"], "GET", "/admin/users")]
    #[case::admin_reconcile(&["admin", "reconcile"], "POST", "/admin/reconcile")]
    #[case::git_ls(&["git", "ls"], "GET", "/git/connections")]
    #[case::deploys_show(&["deploys", "show", "01J000000000000000000000D2"], "GET", "/deploys/01J000000000000000000000D2")]
    #[case::deploys_wait(&["deploys", "wait", "01J000000000000000000000D2"], "GET", "/deploys/01J000000000000000000000D2")]
    #[case::admin_env_set(
        &["admin", "env", "set", "liftoff/prod", "--cpus", "1.5", "--memory", "512"],
        "PATCH",
        "/projects/liftoff/environments/prod"
    )]
    #[tokio::test]
    async fn sends_one_request_and_asks_nothing_in_both_modes(
        #[case] command: &[&str],
        #[case] method: &str,
        #[case] path: &str,
    ) {
        let stub = full_stub().await;
        let args: Vec<&str> = ["--project", "liftoff"]
            .into_iter()
            .chain(command.iter().copied())
            .collect();
        let (in_batch, interactively, batch_out, interactive_out, cli) =
            both_modes(&stub, &args, &args, vec![]).await;
        let trail: Vec<(String, String)> = in_batch
            .iter()
            .map(|seen| (seen.method.clone(), seen.path.clone()))
            .collect();
        assert_eq!(
            trail,
            vec![(method.to_owned(), format!("/api/v1{path}"))],
            "batch"
        );
        assert_eq!(
            interactively.iter().map(request).collect::<Vec<_>>(),
            in_batch.iter().map(request).collect::<Vec<_>>()
        );
        assert_eq!(interactive_out, batch_out);
        assert!(cli.asked().is_empty(), "{:?}", cli.asked());
    }

    // TC: CLIX-02 (`status` reads the environment, then the project's tunnels)
    #[tokio::test]
    async fn sends_env_and_list_requests_and_asks_nothing_when_status() {
        let stub = full_stub().await;
        let args = ["--project", "liftoff", "status"];
        let (in_batch, interactively, batch_out, interactive_out, cli) =
            both_modes(&stub, &args, &args, vec![]).await;
        let trail: Vec<(String, String)> = in_batch
            .iter()
            .map(|seen| (seen.method.clone(), seen.path.clone()))
            .collect();
        assert_eq!(
            trail,
            vec![
                (
                    "GET".to_owned(),
                    "/api/v1/projects/liftoff/environments/prod".to_owned()
                ),
                (
                    "GET".to_owned(),
                    "/api/v1/projects/liftoff/environments".to_owned()
                )
            ],
            "batch"
        );
        assert_eq!(
            interactively.iter().map(request).collect::<Vec<_>>(),
            in_batch.iter().map(request).collect::<Vec<_>>()
        );
        assert_eq!(interactive_out, batch_out);
        assert!(cli.asked().is_empty(), "{:?}", cli.asked());
    }
}

mod requests {
    //! What the batch form of each command with input sends (command →
    //! request).

    use super::*;

    async fn sent(args: &[&str], method: &str, path: &str) -> Seen {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(args).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        stub.seen_at(method, path)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no {method} {path}: {:?}", stub.trail()))
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn env_add_posts_name_and_branch() {
        let seen = sent(
            &[
                "--project",
                "liftoff",
                "env",
                "add",
                "qa",
                "--branch",
                "develop",
            ],
            "POST",
            "/projects/liftoff/environments",
        )
        .await;
        assert_eq!(
            (seen.param("name"), seen.param("branch")),
            (Some("qa".to_owned()), Some("develop".to_owned()))
        );
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn env_rm_confirms_in_the_query() {
        let seen = sent(
            &["--project", "liftoff", "env", "rm", "qa", "--yes"],
            "DELETE",
            "/projects/liftoff/environments/qa",
        )
        .await;
        assert_eq!(seen.query_pairs(), pairs(&[("confirm", "true")]));
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn bind_posts_host_service_and_port() {
        let seen = sent(
            &[
                "--project",
                "liftoff",
                "bind",
                "web:3000",
                "shop.apps.example.com",
            ],
            "POST",
            "/projects/liftoff/environments/prod/bindings",
        )
        .await;
        assert_eq!(
            seen.json(),
            serde_json::json!({"host": "shop.apps.example.com", "service": "web", "port": 3000})
        );
    }

    // TC: CLIX-02 (`--env` picks the environment)
    #[tokio::test]
    async fn bind_targets_the_named_environment_when_env_flag() {
        let stub = full_stub().await.route(
            "POST",
            "/projects/liftoff/environments/qa/bindings",
            vec![json(201, crate::fixtures::BINDING)],
        );
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli
            .run(&[
                "--project",
                "liftoff",
                "--env",
                "qa",
                "bind",
                "web:3000",
                "shop.apps.example.com",
            ])
            .await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            stub.seen_at("POST", "/projects/liftoff/environments/qa/bindings")
                .len(),
            1,
            "{:?}",
            stub.trail()
        );
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn domain_add_posts_a_managed_label_of_the_project() {
        let seen = sent(
            &["--project", "liftoff", "domain", "add", "shop"],
            "POST",
            "/domains",
        )
        .await;
        assert_eq!(
            seen.json(),
            serde_json::json!({"project": "liftoff", "name": "shop", "external": false})
        );
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn domain_add_posts_external_when_external_flag() {
        let seen = sent(
            &[
                "--project",
                "liftoff",
                "domain",
                "add",
                "shop.example.org",
                "--external",
            ],
            "POST",
            "/domains",
        )
        .await;
        assert_eq!(
            seen.json(),
            serde_json::json!({"project": "liftoff", "name": "shop.example.org", "external": true})
        );
    }

    // TC: CLIX-02, DNS-01 (`domain add <label> --proxied`)
    #[tokio::test]
    async fn domain_add_posts_proxied_when_proxied_flag() {
        let seen = sent(
            &["--project", "liftoff", "domain", "add", "shop", "--proxied"],
            "POST",
            "/domains",
        )
        .await;
        assert_eq!(
            seen.json(),
            serde_json::json!({"project": "liftoff", "name": "shop", "external": false, "proxied": true})
        );
    }

    // TC: CLIX-02, DNS-05 (`domain set <host> --proxied=<b>`)
    #[tokio::test]
    async fn domain_set_patches_proxied_flag() {
        let seen = sent(
            &["domain", "set", "shop.apps.example.com", "--proxied=false"],
            "PATCH",
            "/domains/shop.apps.example.com",
        )
        .await;
        assert_eq!(seen.json(), serde_json::json!({"proxied": false}));
    }

    // TC: CLIX-02, DNS-05 (a bare `--proxied` is `true`)
    #[tokio::test]
    async fn domain_set_patches_true_when_bare_proxied_flag() {
        let seen = sent(
            &["domain", "set", "shop.apps.example.com", "--proxied"],
            "PATCH",
            "/domains/shop.apps.example.com",
        )
        .await;
        assert_eq!(seen.json(), serde_json::json!({"proxied": true}));
    }

    // TC: CLIX-02 (`set` writes to the default environment)
    #[tokio::test]
    async fn secret_set_puts_into_the_environment_scope_by_default() {
        let seen = sent(
            &["--project", "liftoff", "secret", "set", "API_KEY=s3cr3t"],
            "PUT",
            "/secrets",
        )
        .await;
        assert_eq!(
            seen.json(),
            serde_json::json!({
                "scope": "environment",
                "id": "liftoff/prod",
                "key": "API_KEY",
                "value": "s3cr3t",
                "ci": false
            })
        );
    }

    // TC: CLIX-02 (the scope can be raised to the project)
    #[tokio::test]
    async fn secret_set_puts_into_the_project_scope_when_asked() {
        let seen = sent(
            &[
                "--project",
                "liftoff",
                "secret",
                "set",
                "API_KEY=s3cr3t",
                "--scope",
                "project",
                "--ci",
            ],
            "PUT",
            "/secrets",
        )
        .await;
        assert_eq!(
            seen.json(),
            serde_json::json!({
                "scope": "project",
                "id": "liftoff",
                "key": "API_KEY",
                "value": "s3cr3t",
                "ci": true
            })
        );
    }

    // TC: CLIX-02, TEN-11 (`--scope tenant` in the project
    // directory is the tenant of its project, named by the project)
    #[tokio::test]
    async fn secret_set_puts_into_the_tenant_of_the_project_when_asked() {
        let seen = sent(
            &[
                "--project",
                "liftoff",
                "secret",
                "set",
                "API_KEY=s3cr3t",
                "--scope",
                "tenant",
            ],
            "PUT",
            "/secrets",
        )
        .await;
        assert_eq!(
            seen.json(),
            serde_json::json!({
                "scope": "tenant",
                "id": "liftoff",
                "key": "API_KEY",
                "value": "s3cr3t",
                "ci": false
            })
        );
    }

    // TC: CLIX-02, TEN-11
    #[tokio::test]
    async fn secret_ls_names_the_tenant_of_the_project_when_asked() {
        let seen = sent(
            &["--project", "liftoff", "secret", "ls", "--scope", "tenant"],
            "GET",
            "/secrets",
        )
        .await;
        assert_eq!(
            seen.query_pairs(),
            pairs(&[("scope", "tenant"), ("id", "liftoff")])
        );
    }

    // TC: CLIX-02 (a value may contain `=`)
    #[tokio::test]
    async fn secret_set_splits_at_the_first_equals_sign() {
        let seen = sent(
            &["--project", "liftoff", "secret", "set", "API_KEY=a=b"],
            "PUT",
            "/secrets",
        )
        .await;
        assert_eq!(seen.param("value"), Some("a=b".to_owned()));
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn secret_ls_names_the_environment_scope() {
        let seen = sent(&["--project", "liftoff", "secret", "ls"], "GET", "/secrets").await;
        assert_eq!(
            seen.query_pairs(),
            pairs(&[("scope", "environment"), ("id", "liftoff/prod")])
        );
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn secret_rollback_posts_the_version_in_the_scope() {
        let seen = sent(
            &[
                "--project",
                "liftoff",
                "secret",
                "rollback",
                "API_KEY",
                "--to",
                "1",
            ],
            "POST",
            "/secrets/API_KEY/rollback",
        )
        .await;
        assert_eq!(
            (seen.query_pairs(), seen.json()),
            (
                pairs(&[("scope", "environment"), ("id", "liftoff/prod")]),
                serde_json::json!({"to": 1})
            )
        );
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn admin_sessions_revoke_names_the_login() {
        let seen = sent(
            &["admin", "sessions", "revoke", "bob"],
            "DELETE",
            "/admin/sessions",
        )
        .await;
        assert_eq!(seen.query_pairs(), pairs(&[("login", "bob")]));
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn admin_env_set_patches_cpus_and_memory() {
        let seen = sent(
            &[
                "admin",
                "env",
                "set",
                "liftoff/prod",
                "--cpus",
                "1.5",
                "--memory",
                "512",
            ],
            "PATCH",
            "/projects/liftoff/environments/prod",
        )
        .await;
        assert_eq!(
            seen.json(),
            serde_json::json!({"cpus": 1.5, "memory_mb": 512})
        );
    }

    // TC: CLIX-02 (only what is given is changed)
    #[tokio::test]
    async fn admin_env_set_patches_only_the_given_field() {
        let seen = sent(
            &["admin", "env", "set", "liftoff/prod", "--cpus", "1.5"],
            "PATCH",
            "/projects/liftoff/environments/prod",
        )
        .await;
        assert_eq!(seen.json(), serde_json::json!({"cpus": 1.5}));
    }

    // TC: CLIX-02 (a snapshot unless `-f`)
    #[tokio::test]
    async fn logs_asks_not_to_follow_when_no_follow_flag() {
        let seen = sent(
            &["--project", "liftoff", "logs"],
            "GET",
            "/projects/liftoff/environments/prod/logs",
        )
        .await;
        assert_eq!(seen.query_pairs(), pairs(&[("follow", "false")]));
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn logs_follows_one_service_when_f_flag() {
        let seen = sent(
            &["--project", "liftoff", "logs", "web", "-f", "--tail", "10"],
            "GET",
            "/projects/liftoff/environments/prod/logs",
        )
        .await;
        assert_eq!(
            seen.query_pairs(),
            pairs(&[("follow", "true"), ("service", "web"), ("tail", "10")])
        );
    }

    // TC: CLIX-02 (looks at the queued deploy)
    #[tokio::test]
    async fn deploy_waits_for_the_queued_deploy_when_wait_flag() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli
            .run(&["--project", "liftoff", "deploy", "--tag", "v1", "--wait"])
            .await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            stub.trail(),
            vec![
                ("POST".to_owned(), "/deploys".to_owned()),
                (
                    "GET".to_owned(),
                    "/deploys/01J000000000000000000000D2".to_owned()
                ),
            ]
        );
        assert!(
            outcome.out.ends_with("deploy #3 succeeded\n"),
            "{outcome:?}"
        );
    }

    // TC: CLIX-02 (the file goes along)
    #[tokio::test]
    async fn deploy_sends_the_compose_file_when_compose_flag() {
        let stub = full_stub().await;
        let cli = Cli::logged_in(&stub.url);
        std::fs::write(cli.cwd.join("prod.yaml"), "services: {}\n").unwrap();
        let outcome = cli
            .run(&[
                "--project",
                "liftoff",
                "deploy",
                "--tag",
                "v1",
                "--compose",
                "prod.yaml",
            ])
            .await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let sent = stub.seen_at("POST", "/deploys");
        assert_eq!(
            sent.first().and_then(|seen| seen.param("compose")),
            Some("services: {}\n".to_owned())
        );
    }

    // TC: CLIX-02 (Forgejo by token only)
    #[tokio::test]
    async fn git_connect_forgejo_posts_provider_host_and_token() {
        let seen = sent(
            &[
                "git",
                "connect",
                "forgejo",
                "--host",
                "git.example.com",
                "--token",
                "fjt_token",
            ],
            "POST",
            "/git/connections",
        )
        .await;
        assert_eq!(
            seen.json(),
            serde_json::json!({"provider": "forgejo", "host": "git.example.com", "token": "fjt_token"})
        );
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn deploy_names_project_environment_and_tag() {
        let seen = sent(
            &["--project", "liftoff", "deploy", "--tag", "v1"],
            "POST",
            "/deploys",
        )
        .await;
        assert_eq!(
            (
                seen.param("project"),
                seen.param("environment"),
                seen.param("tag")
            ),
            (
                Some("liftoff".to_owned()),
                Some("prod".to_owned()),
                Some("v1".to_owned())
            )
        );
    }

    // TC: CLIX-02
    #[tokio::test]
    async fn redeploy_posts_the_target_number_when_to_flag() {
        let seen = sent(
            &["--project", "liftoff", "redeploy", "--to", "2"],
            "POST",
            "/projects/liftoff/environments/prod/redeploy",
        )
        .await;
        assert_eq!(seen.param("to"), Some("2".to_owned()));
    }
}
