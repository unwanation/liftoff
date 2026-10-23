#![cfg(test)]
//! SETUP-06 [live]: `install.sh` and `liftoffd setup` on a clean Debian
//! with systemd and Docker, ending with `GET /readyz` → 200 (plan task
//! 2.10 criterion).
//!
//! Needs Docker on the machine running the test and a signed release:
//! a directory made by `LIFTOFF_SIGNING_KEY=<private ed25519 key>
//! scripts/package-release.sh x86_64-unknown-linux-musl <dir>`, so that it
//! holds `SHA256SUMS-<target>.txt.sig` and `install.sh`, named by
//! `LIFTOFF_RELEASE_DIR`; and the public half of that
//! key (`ssh-ed25519 AAAA…`) in `LIFTOFF_RELEASE_KEY`, which `install.sh`
//! verifies the release against. Never run on a production host: the container is
//! privileged.

use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The container, removed when the test ends.
struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _removed = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

fn docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("cannot run docker {args:?}: {error}"))
}

fn exec(container: &Container, script: &str) -> Output {
    docker(&["exec", &container.0, "bash", "-euc", script])
}

fn ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// TC: SETUP-06
#[tokio::test]
#[ignore = "live: needs Docker, a privileged Debian container, a signed LIFTOFF_RELEASE_DIR and its LIFTOFF_RELEASE_KEY"]
async fn readyz_answers_200_when_a_clean_debian_is_set_up() {
    let release =
        std::env::var("LIFTOFF_RELEASE_DIR").expect("LIFTOFF_RELEASE_DIR names a release");
    let release_key = std::env::var("LIFTOFF_RELEASE_KEY")
        .expect("LIFTOFF_RELEASE_KEY is the public key that signed the release");
    let name = format!("lf-setup-06-{}", std::process::id());
    let started = docker(&[
        "run",
        "--detach",
        "--privileged",
        "--cgroupns=host",
        "--volume",
        "/sys/fs/cgroup:/sys/fs/cgroup:rw",
        "--volume",
        &format!("{release}:/release:ro"),
        "--name",
        &name,
        "debian:trixie",
        "bash",
        "-euc",
        "apt-get update -qq && \
         DEBIAN_FRONTEND=noninteractive apt-get install -y -qq systemd systemd-sysv dbus \
           docker.io docker-compose curl ca-certificates sudo iproute2 openssh-client >/dev/null && \
         exec /lib/systemd/systemd",
    ]);
    ok(&started, "docker run");
    let container = Container(name);

    let deadline = Instant::now() + Duration::from_secs(600);
    while !exec(&container, "systemctl is-active docker")
        .status
        .success()
    {
        assert!(
            Instant::now() < deadline,
            "docker never came up in the container"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let installed = docker(&[
        "exec",
        "--env",
        &format!("LIFTOFF_RELEASE_KEY={release_key}"),
        "--env",
        "LIFTOFF_RELEASE_URL=file:///release",
        &container.0,
        "sh",
        "/release/install.sh",
    ]);
    ok(&installed, "install.sh");
    ok(
        &exec(
            &container,
            "test -x /usr/local/bin/liftoffd && test -x /usr/local/bin/liftoff && test -L /usr/local/bin/lof",
        ),
        "installed binaries",
    );
    let setup = exec(
        &container,
        "liftoffd setup --yes --base-domain apps.test --tls edge --edge-port 7002 </dev/null",
    );
    ok(&setup, "liftoffd setup");
    assert!(
        String::from_utf8_lossy(&setup.stdout).contains("lft_"),
        "no setup token printed"
    );

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let ready = exec(
            &container,
            "curl -fsS -o /dev/null -w '%{http_code}' http://127.0.0.1:7000/readyz",
        );
        if String::from_utf8_lossy(&ready.stdout) == "200" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "readyz never answered 200: {}",
            String::from_utf8_lossy(&ready.stderr)
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
