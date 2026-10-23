//! TC: CRE-17. `lof tenant use <slug>` writes the default tenant into the CLI config for the current
//! server, as `server use` writes the default server; `lof create`
//! without `--tenant` then names it in its plan request, which the
//! server's plan carries into `POST /projects {tenant}`; `--tenant`
//! overrides it; without the `tenants` module the server answers `424`
//! and the CLI shows that error.

use serde_json::{Value, json};

use crate::create::{BATCH, TEMPLATE, bodies, machine, plan, server, server_planning};
use crate::support::{Cli, SERVER, SESSION, Stub, json as reply, problem};

/// The config of a CLI logged in to `server` whose default tenant there
/// is `acme`.
fn with_default_tenant(cli: &Cli, server: &Stub) {
    cli.write_config(&format!(
        "default_server = \"{SERVER}\"\n\n[servers.{SERVER}]\nurl = \"{}\"\ntoken = \"{SESSION}\"\ntenant = \"acme\"\n",
        server.url
    ));
}

/// The `tenant` of every plan request `server` saw.
fn tenants(server: &Stub) -> Vec<Value> {
    bodies(server, "POST", "/git/create-plan")
        .iter()
        .map(|body| body["tenant"].clone())
        .collect()
}

mod use_tenant {
    use super::*;

    // TC: CRE-17
    #[tokio::test]
    async fn exits_0_when_tenant_use_given() {
        let stub = Stub::start().await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["tenant", "use", "acme"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
    }

    // TC: CRE-17 (the config of the current server holds it)
    #[tokio::test]
    async fn writes_tenant_of_current_server_when_tenant_use_given() {
        let stub = Stub::start().await;
        let cli = Cli::logged_in(&stub.url);
        let _outcome = cli.run(&["tenant", "use", "acme"]).await;
        let config = cli.config_toml();
        assert_eq!(
            config
                .get("servers")
                .and_then(|servers| servers.get(SERVER))
                .and_then(|server| server.get("tenant"))
                .and_then(toml::Value::as_str),
            Some("acme")
        );
    }

    // TC: CRE-17 (the current server is the one `--server` chose: its
    // `[servers.<name>]` gets the tenant, the default server's does not)
    #[tokio::test]
    async fn writes_tenant_only_of_chosen_server_when_server_flag_given() {
        let stub = Stub::start().await;
        let cli = Cli::new();
        cli.write_config(&format!(
            "default_server = \"{SERVER}\"\n\n[servers.{SERVER}]\nurl = \"{url}\"\ntoken = \"{SESSION}\"\n\n[servers.other]\nurl = \"{url}\"\ntoken = \"{SESSION}\"\n",
            url = stub.url
        ));
        let outcome = cli
            .run(&["--server", "other", "tenant", "use", "acme"])
            .await;
        let config = cli.config_toml();
        let tenant_of = |name: &str| {
            config
                .get("servers")
                .and_then(|servers| servers.get(name))
                .and_then(|server| server.get("tenant"))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        };
        assert_eq!(
            (outcome.code, tenant_of("other"), tenant_of(SERVER)),
            (0, Some("acme".to_owned()), None),
            "{outcome:?}"
        );
    }

    // TC: CRE-17 (a slug is validated at the boundary: 64, the config
    // unchanged)
    #[tokio::test]
    async fn exits_64_and_keeps_config_when_slug_invalid() {
        let stub = Stub::start().await;
        let cli = Cli::logged_in(&stub.url);
        let before = cli.config_text();
        let outcome = cli.run(&["tenant", "use", "Bad Slug"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.starts_with("error: <slug>: "),
                cli.config_text()
            ),
            (64, true, before),
            "{outcome:?}"
        );
    }

    // TC: CRE-17 (a built-in command: nothing is sent to the server, and the
    // session stays)
    #[tokio::test]
    async fn keeps_session_and_sends_nothing_when_tenant_use_given() {
        let stub = Stub::start().await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["tenant", "use", "acme"]).await;
        let config = cli.config_toml();
        assert_eq!(
            (
                outcome.code,
                stub.trail(),
                config
                    .get("servers")
                    .and_then(|servers| servers.get(SERVER))
                    .and_then(|server| server.get("token"))
                    .and_then(toml::Value::as_str)
            ),
            (0, vec![], Some(SESSION)),
            "{outcome:?}"
        );
    }
}

/// TC: CRE-17. A new session for a server of the config keeps the
/// default tenant `lof tenant use` wrote for it.
mod login_again {
    use super::*;

    // TC: CRE-17 (a re-login after the session expired keeps `tenant`)
    #[tokio::test]
    async fn keeps_default_tenant_when_logged_in_again() {
        let stub = Stub::start().await.route(
            "POST",
            "/auth/token",
            vec![reply(200, crate::fixtures::SESSION_GRANTED)],
        );
        let cli = Cli::new();
        with_default_tenant(&cli, &stub);
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_setup"]).await;
        let config = cli.config_toml();
        assert_eq!(
            config
                .get("servers")
                .and_then(|servers| servers.get(SERVER))
                .and_then(|server| server.get("tenant"))
                .and_then(toml::Value::as_str),
            Some("acme"),
            "{outcome:?}"
        );
    }
}

mod create_with_tenant {
    use super::*;

    // TC: CRE-17 (`tenant use acme`, then `create` without `--tenant`)
    #[tokio::test]
    async fn sends_tenant_of_tenant_use_when_create_lacks_tenant() {
        let server = server().await;
        let (cli, _git) = machine(&server);
        let used = cli.run(&["tenant", "use", "acme"]).await;
        let _outcome = cli.run(BATCH).await;
        assert_eq!(tenants(&server), vec![json!("acme")], "{used:?}");
    }

    // TC: CRE-17 (the config's default tenant)
    #[tokio::test]
    async fn sends_default_tenant_when_create_lacks_tenant() {
        let server = server().await;
        let (cli, _git) = machine(&server);
        with_default_tenant(&cli, &server);
        let _outcome = cli.run(BATCH).await;
        assert_eq!(tenants(&server), vec![json!("acme")]);
    }

    // TC: CRE-17 (`--tenant` overrides the default)
    #[tokio::test]
    async fn sends_tenant_flag_when_tenant_given_over_default() {
        let server = server().await;
        let (cli, _git) = machine(&server);
        with_default_tenant(&cli, &server);
        let args: Vec<&str> = BATCH.iter().copied().chain(["--tenant", "beta"]).collect();
        let _outcome = cli.run(&args).await;
        assert_eq!(tenants(&server), vec![json!("beta")]);
    }

    // TC: CRE-17 (no default and no flag: the server decides, nothing is
    // named)
    #[tokio::test]
    async fn names_no_tenant_when_neither_default_nor_flag() {
        let server = server().await;
        let (cli, _git) = machine(&server);
        let _outcome = cli.run(BATCH).await;
        assert_eq!(tenants(&server), vec![Value::Null]);
    }

    // TC: CRE-17 (without the `tenants` module: the server's `424`, shown
    // as it says; nothing is generated)
    #[tokio::test]
    async fn shows_server_error_when_tenants_module_missing() {
        let server = server_planning(vec![problem(424, "install the module `tenants`")]).await;
        let (cli, _git) = machine(&server);
        with_default_tenant(&cli, &server);
        let outcome = cli.run(BATCH).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.as_str(),
                server.seen_at("POST", "/git/generate").len()
            ),
            (1, "error: install the module `tenants`\n", 0),
            "{outcome:?}"
        );
    }

    // TC: CRE-17 (the server's plan carries the tenant into its
    // `create_project` step; the CLI sends that body to `POST /projects`)
    #[tokio::test]
    async fn sends_tenant_of_plan_to_projects_when_plan_names_one() {
        let mut planned = plan();
        planned["steps"][7]["project"]["tenant"] = json!("acme");
        let server = server_planning(vec![reply(200, &planned.to_string())]).await;
        let (cli, _git) = machine(&server);
        with_default_tenant(&cli, &server);
        let outcome = cli.run(BATCH).await;
        let sent: Vec<Value> = bodies(&server, "POST", "/projects")
            .iter()
            .map(|body| body["tenant"].clone())
            .collect();
        assert_eq!(sent, vec![json!("acme")], "{outcome:?}");
    }

    // TC: CRE-17 (the tenant is the server's: the template is still sent
    // as given)
    #[tokio::test]
    async fn plans_template_and_tenant_when_default_tenant_set() {
        let server = server().await;
        let (cli, _git) = machine(&server);
        with_default_tenant(&cli, &server);
        let _outcome = cli.run(BATCH).await;
        assert_eq!(
            bodies(&server, "POST", "/git/create-plan"),
            vec![json!({"name": "demo", "template": TEMPLATE, "public": false, "tenant": "acme"})]
        );
    }
}
