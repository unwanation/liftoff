//! `liftoff.override.yaml` (CMP-11).

use core::fmt::Write as _;

use super::model::ComposeFile;
use crate::domain::{ContainerPort, HostPort, ServiceName, SliceName};

/// One published port of a service: what a binding contributes to the
/// override (`127.0.0.1:<host_port>:<container_port>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedPort {
    /// The compose service.
    pub service: ServiceName,
    /// The port inside the container.
    pub container_port: ContainerPort,
    /// The loopback port on the host.
    pub host_port: HostPort,
}

/// Renders the override compose file that liftoff passes after the user's
/// `compose.yaml`.
///
/// Every service of `compose` gets `cgroup_parent: <slice>`,
/// `restart: unless-stopped` and the `json-file` logging limits, which
/// replace whatever the user wrote for `restart` and `logging`. A service
/// that has published ports also gets
/// `ports: ["127.0.0.1:<host_port>:<container_port>"]`, one entry per
/// port, lowest host port first. Ports of services the compose file does
/// not define are ignored here (the deploy checks them earlier).
///
/// The output is deterministic: services in file order, no timestamps.
/// The slice name is emitted as a plain scalar because it may contain a
/// literal `\x2d`, which a double-quoted YAML scalar would turn into `-`.
#[must_use]
pub fn render_override(
    compose: &ComposeFile,
    slice: &SliceName,
    bindings: &[PublishedPort],
) -> String {
    let mut out = String::from("services:\n");
    for name in compose.services() {
        let mut ports: Vec<&PublishedPort> =
            bindings.iter().filter(|b| b.service == name).collect();
        ports.sort_by_key(|b| b.host_port);
        // Writing to a String cannot fail.
        let _written = write!(
            out,
            "  {key}:\n    cgroup_parent: {slice}\n    restart: unless-stopped\n    logging: {{ driver: json-file, options: {{ max-size: \"10m\", max-file: \"3\" }} }}\n",
            key = key(name.as_str()),
        );
        if !ports.is_empty() {
            out.push_str("    ports:\n");
            for binding in ports {
                let _written = writeln!(
                    out,
                    "      - \"127.0.0.1:{}:{}\"",
                    binding.host_port, binding.container_port
                );
            }
        }
    }
    out
}

/// A mapping key that YAML reads back as the same string: quoted when it
/// would otherwise be a number, boolean or null (`on`, `123`, `null`).
fn key(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let reserved = matches!(
        lower.as_str(),
        "y" | "n" | "yes" | "no" | "on" | "off" | "true" | "false" | "null"
    );
    let numeric = name.starts_with(|c: char| c.is_ascii_digit() || c == '.');
    if reserved || numeric {
        format!("\"{name}\"")
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EnvName, ProjectName};

    const COMPOSE: &str = "services:
  web:
    image: git.example.com/bob/app:sha-1
    restart: always
    logging:
      driver: none
  worker:
    image: git.example.com/bob/app:sha-1
  db:
    image: postgres:17
";

    fn compose(text: &str) -> ComposeFile {
        ComposeFile::parse(text).unwrap()
    }

    fn slice(project: &str, env: &str) -> SliceName {
        SliceName::new(
            &ProjectName::new(project).unwrap(),
            &EnvName::new(env).unwrap(),
        )
    }

    fn binding(service: &str, container: u16, host: u16) -> PublishedPort {
        PublishedPort {
            service: ServiceName::new(service).unwrap(),
            container_port: ContainerPort::new(container).unwrap(),
            host_port: HostPort::new(host).unwrap(),
        }
    }

    mod render_override {
        use super::*;

        // TC: v1 CMP-11
        #[test]
        fn matches_snapshot_when_three_services_and_two_bindings() {
            let bindings = [binding("web", 3000, 20017), binding("worker", 8080, 20018)];
            let out = render_override(&compose(COMPOSE), &slice("acme", "qa"), &bindings);
            insta::assert_snapshot!(out);
        }

        // TC: v1 CMP-11 (ports only for bound services, user restart/logging replaced)
        #[test]
        fn has_ports_only_for_bound_services_when_some_are_unbound() {
            let bindings = [binding("web", 3000, 20017)];
            let out = render_override(&compose(COMPOSE), &slice("acme", "qa"), &bindings);
            assert_eq!(out.matches("ports:").count(), 1);
            assert_eq!(out.matches("restart: unless-stopped").count(), 3);
            assert!(
                !out.contains("always") && !out.contains("none"),
                "user restart and logging are not echoed: {out}"
            );
        }

        #[test]
        fn lists_every_port_lowest_first_when_a_service_has_two_bindings() {
            let bindings = [binding("web", 9000, 20030), binding("web", 3000, 20017)];
            let out = render_override(&compose(COMPOSE), &slice("acme", "qa"), &bindings);
            assert!(
                out.contains(
                    "      - \"127.0.0.1:20017:3000\"\n      - \"127.0.0.1:20030:9000\"\n"
                ),
                "{out}"
            );
        }

        #[test]
        fn keeps_the_escape_literal_when_slice_name_has_dashes() {
            let out = render_override(&compose(COMPOSE), &slice("acme", "qa-2"), &[]);
            assert_eq!(
                out.matches("cgroup_parent: apps-lf-acme-qa\\x2d2.slice\n")
                    .count(),
                3
            );
        }

        #[test]
        fn has_no_ports_when_no_bindings() {
            let out = render_override(&compose(COMPOSE), &slice("acme", "qa"), &[]);
            assert!(!out.contains("ports"), "{out}");
        }

        #[test]
        fn ignores_binding_when_service_is_not_in_compose() {
            let out = render_override(
                &compose(COMPOSE),
                &slice("acme", "qa"),
                &[binding("ghost", 80, 20001)],
            );
            assert!(!out.contains("ports") && !out.contains("ghost"), "{out}");
        }

        #[test]
        fn quotes_the_key_when_service_name_reads_as_a_yaml_scalar() {
            let out = render_override(
                &compose("services:\n  \"on\":\n    image: x\n  \"123\":\n    image: y\n"),
                &slice("acme", "qa"),
                &[],
            );
            assert!(
                out.contains("  \"on\":\n") && out.contains("  \"123\":\n"),
                "{out}"
            );
        }

        #[test]
        fn parses_back_as_yaml_when_rendered() {
            let out = render_override(
                &compose(COMPOSE),
                &slice("acme", "qa-2"),
                &[binding("web", 3000, 20017)],
            );
            let value = crate::compose::yaml::parse(&out).unwrap();
            let web = value.get("services").and_then(|s| s.get("web")).unwrap();
            assert_eq!(
                (
                    web.get("cgroup_parent").and_then(|v| v.as_str()),
                    web.get("ports")
                        .and_then(|p| p.as_sequence())
                        .and_then(|p| p.first())
                        .and_then(|p| p.as_str()),
                    web.get("logging")
                        .and_then(|l| l.get("options"))
                        .and_then(|o| o.get("max-size"))
                        .and_then(|v| v.as_str())
                ),
                (
                    Some("apps-lf-acme-qa\\x2d2.slice"),
                    Some("127.0.0.1:20017:3000"),
                    Some("10m")
                )
            );
        }
    }
}
