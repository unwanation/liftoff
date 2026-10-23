#![cfg(test)]
//! SETUP-03: the compose project `lf_system` (plan section 3: Caddy on the host network, since every upstream is on the host's loopback; what it listens on comes from the loaded config).

use core::net::{Ipv4Addr, SocketAddr};

use liftoffd::system_compose::{PROJECT, render};

fn admin() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 2019))
}

/// The value of the first `<key>: <value>` line, unquoted.
fn value_of(yaml: &str, key: &str) -> Option<String> {
    yaml.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(key)?.strip_prefix(':')?;
        Some(rest.trim().trim_matches('"').trim_matches('\'').to_owned())
    })
}

mod render {
    use super::*;

    // TC: SETUP-03
    #[test]
    fn names_the_project_lf_system_when_rendered() {
        assert_eq!(
            (PROJECT, value_of(&render(admin()), "name")),
            ("lf_system", Some("lf_system".to_owned()))
        );
    }

    // TC: SETUP-03 (`caddy:2.x` pinned)
    #[test]
    fn pins_a_caddy_2_release_when_rendered() {
        let image = value_of(&render(admin()), "image").unwrap_or_default();
        let tag = image
            .strip_prefix("caddy:")
            .and_then(|tag| tag.split('@').next())
            .unwrap_or_default();
        let parts: Vec<&str> = tag.split('.').collect();
        assert_eq!(
            (
                parts.len(),
                parts.first().copied(),
                parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
            ),
            (3, Some("2"), true),
            "{image:?}"
        );
    }

    // TC: SETUP-03 (upstreams are on the host's loopback)
    #[test]
    fn uses_the_host_network_when_rendered() {
        assert_eq!(
            value_of(&render(admin()), "network_mode"),
            Some("host".to_owned())
        );
    }

    // TC: SETUP-03 (nothing is published by compose; Caddy's config
    // decides between 80/443 and `127.0.0.1:<edge_port>`)
    #[test]
    fn publishes_no_ports_when_rendered() {
        let yaml = render(admin());
        assert!(
            !yaml
                .lines()
                .any(|line| line.trim_start().starts_with("ports:")),
            "{yaml}"
        );
    }

    // TC: SETUP-03 (admin API on loopback)
    #[test]
    fn puts_the_admin_api_on_loopback_2019_when_rendered() {
        let yaml = render(admin());
        assert!(yaml.contains("127.0.0.1:2019"), "{yaml}");
    }

    // TC: SETUP-03
    #[test]
    fn puts_the_admin_api_where_asked_when_another_address_is_given() {
        let yaml = render(SocketAddr::from((Ipv4Addr::LOCALHOST, 2119)));
        assert!(
            yaml.contains("127.0.0.1:2119") && !yaml.contains("127.0.0.1:2019"),
            "{yaml}"
        );
    }

    // TC: SETUP-03 (ACME certificates survive a container restart)
    #[test]
    fn keeps_caddy_data_in_a_volume_when_rendered() {
        let yaml = render(admin());
        assert!(
            yaml.lines().any(|line| line.trim_end().ends_with(":/data")),
            "{yaml}"
        );
    }

    // TC: SETUP-03 (the image's default Caddyfile would serve :80)
    #[test]
    fn runs_caddy_without_the_image_caddyfile_when_rendered() {
        let yaml = render(admin());
        let command = value_of(&yaml, "command").unwrap_or_default();
        assert!(
            command.contains("run") && !yaml.contains("Caddyfile"),
            "{yaml}"
        );
    }

    // TC: SETUP-03
    #[test]
    fn restarts_caddy_with_the_host_when_rendered() {
        assert_eq!(
            value_of(&render(admin()), "restart"),
            Some("unless-stopped".to_owned())
        );
    }

    // TC: SETUP-02, SETUP-03
    #[test]
    fn matches_the_snapshot_when_rendered() {
        insta::with_settings!({ prepend_module_to_snapshot => false, omit_expression => true }, {
            insta::assert_snapshot!("setup03_lf_system_compose", render(admin()));
        });
    }
}
