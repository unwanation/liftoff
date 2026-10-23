//! `lof module …` and the commands of modules (REG-03…08) against the local stub of the API, in
//! both modes. The command tree of a module is built from `GET /modules`
//! (`liftoff-api::ModuleView`): `lof <module> <command> [--<arg> V]...`
//! runs `POST /api/v1/m/<module>/<command>` with the arguments as a JSON
//! object and prints the answer as received.

use crate::support::{Cli, Stub, json, problem, say, text};

/// `GET /modules` with `probe` installed: its command `hello` takes the
/// required `--who`, asked with `Who to greet`.
const MODULES: &str = r#"[{"name":"probe","version":"0.1.0","enabled":true,"error":null,"installed_at":"2026-10-19T10:00:00Z","commands":[{"name":"hello","description":"Says hello","args":[{"name":"who","type":"string","required":true,"hint":"Who to greet"}]}],"routes":[{"method":"POST","path":"/hello","public":false}],"tools":[],"config_schema":null}]"#;

/// `GET /modules` with a module `counter` whose `count` takes an integer
/// `--n` and a boolean `--loud`.
const COUNTER: &str = r#"[{"name":"counter","version":"0.1.0","enabled":true,"error":null,"installed_at":"2026-10-19T10:00:00Z","commands":[{"name":"count","description":"Counts","args":[{"name":"n","type":"integer","required":true,"hint":"How far"},{"name":"loud","type":"boolean","required":false,"hint":null}]}],"routes":[{"method":"POST","path":"/count","public":false}],"tools":[],"config_schema":null}]"#;

/// One installed module as `POST /modules` answers it.
const PROBE: &str = r#"{"name":"probe","version":"0.1.0","enabled":true,"error":null,"installed_at":"2026-10-19T10:00:00Z","commands":[{"name":"hello","description":"Says hello","args":[{"name":"who","type":"string","required":true,"hint":"Who to greet"}]}],"routes":[{"method":"POST","path":"/hello","public":false}],"tools":[],"config_schema":null}"#;

/// `GET /modules/available`: `probe` 0.1.0 with a required `greeting`
/// (asked with `How to greet`) and a secret `token`.
const AVAILABLE: &str = r#"[{"name":"probe","releases":[{"version":"0.1.0","wit_version":"0.1.0","liftoffd_min":"2.0.0","requires":[],"config_schema":{"type":"object","properties":{"greeting":{"type":"string","description":"How to greet"},"token":{"type":"string","format":"secret","description":"A token to keep secret"}},"required":["greeting"],"additionalProperties":false}}]}]"#;

/// A stub with `probe` installed, its `hello` answering `hi, world`.
async fn stub() -> Stub {
    Stub::start()
        .await
        .route("GET", "/modules", vec![json(200, MODULES)])
        .route("GET", "/modules/available", vec![json(200, AVAILABLE)])
        .route("POST", "/modules", vec![json(201, PROBE)])
        .route("POST", "/m/probe/hello", vec![text(200, "hi, world")])
}

mod dynamic_command {
    use super::*;

    // TC: REG-08
    #[tokio::test]
    async fn exits_64_listing_who_when_required_arg_is_missing_without_tty() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["probe", "hello"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.as_str(),
                stub.seen_at("POST", "/m/probe/hello").len()
            ),
            (64, "error: missing required arguments: --who\n", 0),
            "{outcome:?}"
        );
    }

    // TC: REG-08
    #[tokio::test]
    async fn asks_with_manifest_hint_when_required_arg_is_missing_on_tty() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("world")]);
        let outcome = cli.run(&["probe", "hello"]).await;
        let sent: Vec<serde_json::Value> = stub
            .seen_at("POST", "/m/probe/hello")
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        assert_eq!(
            (outcome.code, cli.asked(), sent),
            (
                0,
                vec!["Who to greet".to_owned()],
                vec![serde_json::json!({"who": "world"})]
            ),
            "{outcome:?}"
        );
    }

    // TC: REG-08 (the flag the manifest declares: the same request, no
    // question, the answer as received)
    #[tokio::test]
    async fn sends_flag_value_and_prints_answer_when_arg_is_given() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["probe", "hello", "--who", "world"]).await;
        let sent: Vec<serde_json::Value> = stub
            .seen_at("POST", "/m/probe/hello")
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        assert_eq!(
            (outcome.code, outcome.out.as_str(), sent),
            (0, "hi, world\n", vec![serde_json::json!({"who": "world"})]),
            "{outcome:?}"
        );
    }

    // TC: REG-08 (`--no-input` after the command means what it means
    // before the module: never ask, even on a terminal)
    #[tokio::test]
    async fn exits_64_listing_who_when_no_input_is_given_on_tty() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).terminal(Vec::new());
        let outcome = cli.run(&["probe", "hello", "--no-input"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.as_str(),
                cli.asked(),
                stub.seen_at("POST", "/m/probe/hello").len()
            ),
            (
                64,
                "error: missing required arguments: --who\n",
                Vec::<String>::new(),
                0
            ),
            "{outcome:?}"
        );
    }

    // TC: REG-08 (`--json` after the command means what it means before
    // the module: a refused request prints the problem as received)
    #[tokio::test]
    async fn prints_problem_as_received_when_json_is_given_and_route_refuses() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", vec![json(200, MODULES)])
            .route(
                "POST",
                "/m/probe/hello",
                vec![problem(422, "who: must not be empty")],
            );
        let outcome = Cli::logged_in(&stub.url)
            .run(&["probe", "hello", "--who", "", "--json"])
            .await;
        let printed: Option<serde_json::Value> = serde_json::from_str(&outcome.out).ok();
        assert_eq!(
            (
                outcome.code,
                printed.map(|problem| problem["detail"].clone())
            ),
            (1, Some(serde_json::json!("who: must not be empty"))),
            "{outcome:?}"
        );
    }

    // TC: REG-08 (arguments typed by the schema: an integer is sent as a
    // number, a boolean is a switch)
    #[tokio::test]
    async fn sends_integer_as_number_and_switch_as_bool_when_given() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", vec![json(200, COUNTER)])
            .route("POST", "/m/counter/count", vec![text(200, "3")]);
        let outcome = Cli::logged_in(&stub.url)
            .run(&["counter", "count", "--n", "3", "--loud"])
            .await;
        let sent: Vec<serde_json::Value> = stub
            .seen_at("POST", "/m/counter/count")
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        assert_eq!(
            (outcome.code, sent),
            (0, vec![serde_json::json!({"n": 3, "loud": true})]),
            "{outcome:?}"
        );
    }

    // TC: REG-08 (an integer argument that is no integer is a usage
    // error, before anything is sent)
    #[tokio::test]
    async fn exits_64_without_sending_when_integer_arg_is_not_a_number() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", vec![json(200, COUNTER)])
            .route("POST", "/m/counter/count", vec![text(200, "3")]);
        let outcome = Cli::logged_in(&stub.url)
            .run(&["counter", "count", "--n", "three"])
            .await;
        assert_eq!(
            (outcome.code, stub.seen_at("POST", "/m/counter/count").len()),
            (64, 0),
            "{outcome:?}"
        );
    }

    // TC: REG-06 (installed without a restart: the next run has the
    // command)
    #[tokio::test]
    async fn runs_module_command_right_after_install() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", vec![json(200, "[]"), json(200, MODULES)])
            .route("GET", "/modules/available", vec![json(200, AVAILABLE)])
            .route("POST", "/modules", vec![json(201, PROBE)])
            .route("POST", "/m/probe/hello", vec![text(200, "hi, world")]);
        let cli = Cli::logged_in(&stub.url);
        let installed = cli
            .run(&["module", "install", "probe", "--set", "greeting=hi"])
            .await;
        let greeted = Cli::logged_in(&stub.url)
            .run(&["probe", "hello", "--who", "world"])
            .await;
        let sent: Vec<serde_json::Value> = stub
            .seen_at("POST", "/modules")
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        assert_eq!(
            (
                installed.code,
                sent.first()
                    .map(|body| (body["name"].clone(), body["config"].clone())),
                greeted.code,
                greeted.out.as_str()
            ),
            (
                0,
                Some((
                    serde_json::json!("probe"),
                    serde_json::json!({"greeting": "hi"})
                )),
                0,
                "hi, world\n"
            ),
            "{installed:?} {greeted:?}"
        );
    }
}

mod install {
    use super::*;

    /// The JSON bodies of `POST /modules` so far.
    fn installs(stub: &Stub) -> Vec<serde_json::Value> {
        stub.seen_at("POST", "/modules")
            .iter()
            .map(crate::support::Seen::json)
            .collect()
    }

    // TC: REG-04 (batch mode lists the required fields of the schema)
    #[tokio::test]
    async fn exits_64_listing_set_greeting_when_required_config_is_missing_without_tty() {
        let stub = stub().await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&["module", "install", "probe"])
            .await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), installs(&stub).len()),
            (64, "error: missing required arguments: --set greeting\n", 0),
            "{outcome:?}"
        );
    }

    // TC: REG-04 (on a terminal the schema's description is the question)
    #[tokio::test]
    async fn asks_required_config_field_with_its_description_on_tty() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![say("hi")]);
        let outcome = cli.run(&["module", "install", "probe"]).await;
        assert_eq!(
            (
                outcome.code,
                cli.asked(),
                installs(&stub)
                    .first()
                    .map(|body| (body["name"].clone(), body["config"].clone()))
            ),
            (
                0,
                vec!["How to greet".to_owned()],
                Some((
                    serde_json::json!("probe"),
                    serde_json::json!({"greeting": "hi"})
                ))
            ),
            "{outcome:?}"
        );
    }

    // TC: REG-04 (`--set k=v`)
    #[tokio::test]
    async fn sends_set_values_as_config_when_given() {
        let stub = stub().await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&["module", "install", "probe", "--set", "greeting=hi"])
            .await;
        assert_eq!(
            (
                outcome.code,
                installs(&stub)
                    .first()
                    .map(|body| (body["name"].clone(), body["config"].clone()))
            ),
            (
                0,
                Some((
                    serde_json::json!("probe"),
                    serde_json::json!({"greeting": "hi"})
                ))
            ),
            "{outcome:?}"
        );
    }

    // TC: REG-05 (`--set k=@file` for secrets in automation; the file's final newline is not part of the value)
    #[tokio::test]
    async fn reads_set_value_from_file_when_at_given() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url);
        std::fs::write(cli.cwd.join("token.txt"), "s3cret\n").unwrap();
        let outcome = cli
            .run(&[
                "module",
                "install",
                "probe",
                "--set",
                "greeting=hi",
                "--set",
                "token=@token.txt",
            ])
            .await;
        assert_eq!(
            (
                outcome.code,
                installs(&stub).first().map(|body| body["config"].clone())
            ),
            (
                0,
                Some(serde_json::json!({"greeting": "hi", "token": "s3cret"}))
            ),
            "{outcome:?}"
        );
    }

    /// A stub whose registry offers `forge` 0.1.0, configured by an array
    /// of strings `hosts` and an object `labels`.
    async fn forge() -> Stub {
        const FORGE: &str = r#"[{"name":"forge","releases":[{"version":"0.1.0","wit_version":"0.1.0","liftoffd_min":"2.0.0","requires":[],"config_schema":{"type":"object","properties":{"hosts":{"type":"array","items":{"type":"string"},"description":"Hosts"},"labels":{"type":"object","description":"Labels"}},"required":["hosts"],"additionalProperties":false}}]}]"#;
        Stub::start()
            .await
            .route("GET", "/modules", vec![json(200, "[]")])
            .route("GET", "/modules/available", vec![json(200, FORGE)])
            .route("POST", "/modules", vec![json(201, PROBE)])
    }

    // TC: REG-04 (`--set k=v` sets an array or an object field as JSON)
    #[tokio::test]
    async fn sends_json_array_and_object_when_set_on_such_fields() {
        let stub = forge().await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&[
                "module",
                "install",
                "forge",
                "--set",
                r#"hosts=["git.example.com","code.example.com"]"#,
                "--set",
                r#"labels={"team":"core"}"#,
            ])
            .await;
        assert_eq!(
            (
                outcome.code,
                installs(&stub).first().map(|body| body["config"].clone())
            ),
            (
                0,
                Some(serde_json::json!({
                    "hosts": ["git.example.com", "code.example.com"],
                    "labels": {"team": "core"}
                }))
            ),
            "{outcome:?}"
        );
    }

    // TC: REG-04 (an array of strings also takes `a,b`)
    #[tokio::test]
    async fn splits_on_commas_when_string_array_is_set_without_json() {
        let stub = forge().await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&[
                "module",
                "install",
                "forge",
                "--set",
                "hosts=git.example.com, code.example.com",
            ])
            .await;
        assert_eq!(
            (
                outcome.code,
                installs(&stub).first().map(|body| body["config"].clone())
            ),
            (
                0,
                Some(serde_json::json!({"hosts": ["git.example.com", "code.example.com"]}))
            ),
            "{outcome:?}"
        );
    }

    // TC: REG-04 (validate at the boundary: an object field takes JSON
    // only)
    #[tokio::test]
    async fn exits_64_without_sending_when_object_field_is_not_json() {
        let stub = forge().await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&[
                "module",
                "install",
                "forge",
                "--set",
                "hosts=git.example.com",
                "--set",
                "labels=team",
            ])
            .await;
        assert_eq!(
            (outcome.code, installs(&stub).len()),
            (64, 0),
            "{outcome:?}"
        );
    }

    /// `GET /modules/available` with `tenants` 0.1.0 requiring `requires`.
    fn tenants(requires: &str) -> String {
        format!(
            r#"[{{"name":"tenants","releases":[{{"version":"0.1.0","wit_version":"0.1.0","liftoffd_min":"2.0.0","requires":{requires},"config_schema":null}}]}}]"#
        )
    }

    /// A stub with nothing installed, `tenants` requiring `requires` in
    /// the registry, and `POST /modules` refused with `424`.
    async fn refusing(requires: &str) -> Stub {
        Stub::start()
            .await
            .route("GET", "/modules", vec![json(200, "[]")])
            .route(
                "GET",
                "/modules/available",
                vec![json(200, &tenants(requires))],
            )
            .route(
                "POST",
                "/modules",
                vec![problem(424, "install the module `oidc`")],
            )
    }

    // TC: REG-03 (refused with the module to install before anything is sent)
    #[tokio::test]
    async fn prints_module_to_install_without_sending_when_dependency_is_missing() {
        let stub = refusing(r#"["oidc"]"#).await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&["module", "install", "tenants"])
            .await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), installs(&stub).len()),
            (1, "error: install the module `oidc`\n", 0),
            "{outcome:?}"
        );
    }

    // TC: REG-03 (the server's refusal, 424 with the module to install,
    // when the registry's hint did not list the dependency)
    #[tokio::test]
    async fn prints_servers_refusal_when_server_requires_module() {
        let stub = refusing("[]").await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&["module", "install", "tenants"])
            .await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), installs(&stub).len()),
            (1, "error: install the module `oidc`\n", 1),
            "{outcome:?}"
        );
    }
}

mod remove {
    use super::*;

    async fn removing(args: &[&str]) -> (u8, Vec<Vec<(String, String)>>) {
        let stub = Stub::start().await.route(
            "DELETE",
            "/modules/probe",
            vec![json(200, r#"{"deleted":"probe"}"#)],
        );
        let outcome = Cli::logged_in(&stub.url).run(args).await;
        (
            outcome.code,
            stub.seen_at("DELETE", "/modules/probe")
                .iter()
                .map(crate::support::Seen::query_pairs)
                .collect(),
        )
    }

    // TC: REG-07
    #[tokio::test]
    async fn deletes_without_purge_when_purge_not_given() {
        assert_eq!(
            removing(&["module", "remove", "probe"]).await,
            (0, vec![Vec::new()])
        );
    }

    // TC: REG-07
    #[tokio::test]
    async fn deletes_with_purge_when_purge_given() {
        assert_eq!(
            removing(&["module", "remove", "probe", "--purge"]).await,
            (0, vec![vec![("purge".to_owned(), "true".to_owned())]])
        );
    }
}

mod enable {
    use super::*;

    // TC: MOD-09 (`lof module enable` after a fix)
    #[tokio::test]
    async fn patches_enabled_when_enabled() {
        let stub = Stub::start()
            .await
            .route("PATCH", "/modules/probe", vec![json(200, PROBE)]);
        let outcome = Cli::logged_in(&stub.url)
            .run(&["module", "enable", "probe"])
            .await;
        let sent: Vec<serde_json::Value> = stub
            .seen_at("PATCH", "/modules/probe")
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        assert_eq!(
            (outcome.code, sent),
            (0, vec![serde_json::json!({"enabled": true})]),
            "{outcome:?}"
        );
    }
}

/// `GET /modules` with `probe` installed whose schema has a string
/// `greeting`, a secret `token`, an integer `count` and a boolean `loud`.
const CONFIGURABLE: &str = r#"[{"name":"probe","version":"0.1.0","enabled":true,"error":null,"installed_at":"2026-10-19T10:00:00Z","commands":[],"routes":[],"tools":[],"config_schema":{"type":"object","properties":{"count":{"type":"integer","description":"How many"},"greeting":{"type":"string","description":"How to greet"},"loud":{"type":"boolean","description":"Shout"},"token":{"type":"string","format":"secret","description":"A token to keep secret"}}}}]"#;

mod config {
    use super::*;

    /// A stub with [`CONFIGURABLE`] installed that takes a `PATCH`.
    async fn configurable() -> Stub {
        Stub::start()
            .await
            .route("GET", "/modules", vec![json(200, CONFIGURABLE)])
            .route("PATCH", "/modules/probe", vec![json(200, PROBE)])
    }

    /// The JSON bodies of `PATCH /modules/probe` so far.
    fn patches(stub: &Stub) -> Vec<serde_json::Value> {
        stub.seen_at("PATCH", "/modules/probe")
            .iter()
            .map(crate::support::Seen::json)
            .collect()
    }

    // TC: REG-04 (`lof module config --set` sends only the fields it
    // sets; the server merges them into the stored configuration)
    #[tokio::test]
    async fn patches_only_set_field_when_set_is_given() {
        let stub = configurable().await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&["module", "config", "probe", "--set", "greeting=hey"])
            .await;
        assert_eq!(
            (outcome.code, patches(&stub)),
            (0, vec![serde_json::json!({"config": {"greeting": "hey"}})]),
            "{outcome:?}"
        );
    }

    // TC: REG-04 (a value is typed by the schema: an integer and a
    // boolean go on the wire as JSON numbers and booleans)
    #[tokio::test]
    async fn sends_number_and_bool_when_schema_types_them() {
        let stub = configurable().await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&[
                "module",
                "config",
                "probe",
                "--set",
                "count=3",
                "--set",
                "loud=true",
            ])
            .await;
        assert_eq!(
            (outcome.code, patches(&stub)),
            (
                0,
                vec![serde_json::json!({"config": {"count": 3, "loud": true}})]
            ),
            "{outcome:?}"
        );
    }

    // TC: REG-04 (a value its schema type refuses is refused before
    // anything is sent)
    #[tokio::test]
    async fn exits_64_without_sending_when_value_is_not_of_schema_type() {
        let stub = configurable().await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&["module", "config", "probe", "--set", "count=many"])
            .await;
        assert_eq!((outcome.code, patches(&stub).len()), (64, 0), "{outcome:?}");
    }

    // TC: REG-05 (on a terminal every field is asked, the secret one
    // without echo; an empty answer keeps the field)
    #[tokio::test]
    async fn asks_secret_field_without_echo_when_no_set_on_tty() {
        let stub = configurable().await;
        let cli =
            Cli::logged_in(&stub.url).terminal(vec![say(""), say("hey"), say(""), say("n3w")]);
        let outcome = cli.run(&["module", "config", "probe"]).await;
        assert_eq!(
            (outcome.code, cli.asked_hidden(), patches(&stub)),
            (
                0,
                vec!["A token to keep secret (empty keeps it)".to_owned()],
                vec![serde_json::json!({"config": {"greeting": "hey", "token": "n3w"}})]
            ),
            "{outcome:?} {:?}",
            cli.asked()
        );
    }
}

mod ls {
    use super::*;

    // TC: REG-06 (`lof module ls` shows what `GET /modules` lists)
    #[tokio::test]
    async fn lists_installed_modules_when_ls() {
        let stub = stub().await;
        let outcome = Cli::logged_in(&stub.url).run(&["module", "ls"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.out.contains("probe"),
                stub.seen_at("GET", "/modules").len(),
                stub.seen_at("GET", "/modules/available").len()
            ),
            (0, true, 1, 0),
            "{outcome:?}"
        );
    }

    // TC: REG-02 (`lof module ls --available` shows the releases the
    // server can run)
    #[tokio::test]
    async fn lists_available_releases_when_available_is_given() {
        let stub = stub().await;
        let outcome = Cli::logged_in(&stub.url)
            .run(&["module", "ls", "--available"])
            .await;
        assert_eq!(
            (
                outcome.code,
                outcome.out.contains("probe") && outcome.out.contains("0.1.0"),
                stub.seen_at("GET", "/modules/available").len(),
                stub.seen_at("GET", "/modules").len()
            ),
            (0, true, 1, 0),
            "{outcome:?}"
        );
    }
}
