use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(name = "liftoffd", version, about, long_about = None, propagate_version = true)]
pub(crate) struct Args {
    /// Path to a TOML config file. Env vars `LIFTOFF_*` override it.
    #[arg(long, env = "LIFTOFF_CONFIG", value_name = "FILE", global = true)]
    pub(crate) config: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Option<RootCommand>,
}

/// Operational commands of the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Default, Subcommand)]
pub(crate) enum RootCommand {
    /// Run the REST listener.
    #[default]
    Serve,
    /// Install liftoff on this host.
    Setup(crate::setup::SetupArgs),
    /// Import the artifacts of the standalone artifacts server from its data directory. Run it while the daemon is stopped.
    ImportArtifacts(crate::import_artifacts::ImportArtifactsArgs),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_when_no_subcommand() {
        let args = Args::try_parse_from(["liftoffd"]).unwrap();
        assert_eq!(args.command.unwrap_or_default(), RootCommand::Serve);
    }

    mod setup {
        use super::*;
        use crate::config::TlsKind;
        use crate::setup::SetupArgs;

        // TC: SETUP-01 (the documented flags)
        #[test]
        fn carries_every_answer_when_given_as_flags() {
            let args = Args::try_parse_from([
                "liftoffd",
                "setup",
                "--yes",
                "--base-domain",
                "apps.test",
                "--public-url",
                "https://liftoff.apps.test",
                "--public-ip",
                "203.0.113.10",
                "--tls",
                "edge",
                "--edge-port",
                "7002",
                "--acme-email",
                "ops@example.com",
                "--proxy-admin",
                "127.0.0.1:2029",
                "--force",
            ])
            .unwrap();
            assert_eq!(
                args.command,
                Some(RootCommand::Setup(SetupArgs {
                    base_domain: Some("apps.test".to_owned()),
                    public_url: Some("https://liftoff.apps.test".to_owned()),
                    public_ip: Some(core::net::Ipv4Addr::new(203, 0, 113, 10)),
                    tls: Some(TlsKind::Edge),
                    edge_port: Some(7002),
                    acme_email: Some("ops@example.com".to_owned()),
                    proxy_admin: Some(core::net::SocketAddr::from((
                        core::net::Ipv4Addr::LOCALHOST,
                        2029
                    ))),
                    yes: true,
                    force: true,
                    import: None,
                    import_issuer: None,
                    import_git_host: None,
                }))
            );
        }

        // TC: IMP-01 (`liftoffd setup --import <path>`)
        #[test]
        fn carries_the_import_when_given_with_issuer_and_git_host() {
            let args = Args::try_parse_from([
                "liftoffd",
                "setup",
                "--import",
                "/srv/liftoff-v1.db",
                "--import-issuer",
                "https://auth.example.com",
                "--import-git-host",
                "git.example.com",
            ])
            .unwrap();
            let Some(RootCommand::Setup(setup)) = args.command else {
                panic!("not setup: {:?}", args.command);
            };
            assert_eq!(
                (setup.import, setup.import_issuer, setup.import_git_host),
                (
                    Some(PathBuf::from("/srv/liftoff-v1.db")),
                    Some("https://auth.example.com".to_owned()),
                    Some("git.example.com".to_owned())
                )
            );
        }

        // TC: IMP-01
        #[test]
        fn refuses_import_when_issuer_and_git_host_are_missing() {
            assert!(
                Args::try_parse_from(["liftoffd", "setup", "--import", "/srv/liftoff-v1.db"])
                    .is_err()
            );
        }

        // TC: IMP-01
        #[test]
        fn refuses_import_issuer_when_import_is_missing() {
            assert!(
                Args::try_parse_from([
                    "liftoffd",
                    "setup",
                    "--import-issuer",
                    "https://auth.example.com"
                ])
                .is_err()
            );
        }

        // TC: SETUP-01
        #[test]
        fn refuses_when_tls_is_neither_acme_nor_edge() {
            assert!(Args::try_parse_from(["liftoffd", "setup", "--tls", "manual"]).is_err());
        }
    }

    #[test]
    fn accepted_anywhere_when_config_flag() {
        let args = Args::try_parse_from(["liftoffd", "serve", "--config", "x.toml"]).unwrap();
        assert_eq!(args.config, Some(PathBuf::from("x.toml")));
    }
}
