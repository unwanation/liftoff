//! TC: SETUP-08. `scripts/install.sh`, the installer of the release that
//! `lof server upgrade` runs too, against a release in a temporary
//! directory (`LIFTOFF_RELEASE_URL=file://…`): it installs only binaries
//! whose `SHA256SUMS` carries the release key's signature.
//!
//! Needs `sh`, `curl`, `sha256sum` and `ssh-keygen` (OpenSSH 8.1+), as
//! the installer does on a server.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// The installer of the repository.
fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh")
}

/// The release target of this machine, as the installer picks it.
fn target() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "aarch64-unknown-linux-musl",
        _ => "x86_64-unknown-linux-musl",
    }
}

fn sums() -> String {
    format!("SHA256SUMS-{}.txt", target())
}

fn ok(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A release directory, the keys it may be signed with and a bin dir.
struct Release {
    dir: TempDir,
}

impl Release {
    /// `liftoffd` and `liftoff` of version `version`, `SHA256SUMS` and its
    /// signature by the `release` key.
    fn new(version: &str) -> Self {
        let release = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        for dir in ["release", "keys", "bin"] {
            std::fs::create_dir(release.path(dir)).unwrap();
        }
        for name in ["release", "other"] {
            ok(&Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
                .arg(release.path(&format!("keys/{name}")))
                .output()
                .unwrap());
        }
        for bin in ["liftoffd", "liftoff"] {
            release.binary(bin, &format!("{bin} {version}"));
        }
        release.sum_all();
        release.sign("release");
        release
    }

    fn path(&self, path: &str) -> PathBuf {
        self.dir.path().join(path)
    }

    /// A release binary that prints `says` for `--version`.
    fn binary(&self, bin: &str, says: &str) {
        let file = self.path(&format!("release/{bin}-{}", target()));
        std::fs::write(&file, format!("#!/bin/sh\necho '{says}'\n")).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `SHA256SUMS` of every file, as `scripts/package-release.sh` writes it.
    fn sum_all(&self) {
        let output = Command::new("sh")
            .args(["-c", &format!("sha256sum ./*-{} > {}", target(), sums())])
            .current_dir(self.path("release"))
            .output()
            .unwrap();
        ok(&output);
    }

    /// Signs `SHA256SUMS` with key `key`, as the release workflow does.
    fn sign(&self, key: &str) {
        let _stale = std::fs::remove_file(self.path(&format!("release/{}.sig", sums())));
        ok(&Command::new("ssh-keygen")
            .args(["-q", "-Y", "sign", "-n", "liftoff-release", "-f"])
            .arg(self.path(&format!("keys/{key}")))
            .arg(self.path(&format!("release/{}", sums())))
            .output()
            .unwrap());
    }

    fn public_key(&self, key: &str) -> String {
        std::fs::read_to_string(self.path(&format!("keys/{key}.pub")))
            .unwrap()
            .trim()
            .to_owned()
    }

    /// Runs the installer from `url` into `bin`, trusting the `release` key.
    fn install_from(&self, url: &str) -> Output {
        Command::new("sh")
            .arg(script())
            .env("LIFTOFF_RELEASE_URL", url)
            .env("LIFTOFF_BIN_DIR", self.path("bin"))
            .env("LIFTOFF_RELEASE_KEY", self.public_key("release"))
            .output()
            .unwrap()
    }

    /// Runs the installer from the release directory with `key` as
    /// `LIFTOFF_RELEASE_KEY`, or without the variable.
    fn install_with_key(&self, key: Option<&str>) -> Output {
        let mut command = Command::new("sh");
        command
            .arg(script())
            .env(
                "LIFTOFF_RELEASE_URL",
                format!("file://{}", self.path("release").display()),
            )
            .env("LIFTOFF_BIN_DIR", self.path("bin"));
        match key {
            Some(key) => command.env("LIFTOFF_RELEASE_KEY", key),
            None => command.env_remove("LIFTOFF_RELEASE_KEY"),
        };
        command.output().unwrap()
    }

    fn install(&self) -> Output {
        self.install_from(&format!("file://{}", self.path("release").display()))
    }

    /// What `bin/<bin> --version` says, if it is there.
    fn installed(&self, bin: &str) -> Option<String> {
        let output = Command::new(self.path(&format!("bin/{bin}")))
            .arg("--version")
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// A release of version 1 installed, then this one prepared.
    fn over_an_install(version: &str) -> Self {
        let release = Self::new("1");
        ok(&release.install());
        for bin in ["liftoffd", "liftoff"] {
            release.binary(bin, &format!("{bin} {version}"));
        }
        release.sum_all();
        release.sign("release");
        release
    }

    fn installed_pair(&self) -> (Option<String>, Option<String>) {
        (self.installed("liftoffd"), self.installed("liftoff"))
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

mod signed {
    use super::*;

    // TC: SETUP-08
    #[test]
    fn installs_both_binaries_when_the_release_is_signed() {
        let release = Release::new("2");
        ok(&release.install());
        assert_eq!(
            release.installed_pair(),
            (Some("liftoffd 2".to_owned()), Some("liftoff 2".to_owned()))
        );
    }

    // TC: SETUP-08 (the CLI answers to `lof`)
    #[test]
    fn links_lof_to_liftoff_when_the_release_is_signed() {
        let release = Release::new("2");
        ok(&release.install());
        assert_eq!(
            std::fs::read_link(release.path("bin/lof")).ok(),
            Some(PathBuf::from("liftoff"))
        );
    }

    // TC: SETUP-08
    #[test]
    fn replaces_the_binaries_when_a_newer_release_is_signed() {
        let release = Release::over_an_install("2");
        ok(&release.install());
        assert_eq!(
            release.installed_pair(),
            (Some("liftoffd 2".to_owned()), Some("liftoff 2".to_owned()))
        );
    }
}

mod refused {
    use super::*;

    /// The installer failed and version 1 is still installed.
    fn refused_and_kept(release: &Release, output: &Output) {
        assert_eq!(
            (output.status.success(), release.installed_pair()),
            (
                false,
                (Some("liftoffd 1".to_owned()), Some("liftoff 1".to_owned()))
            ),
            "{}",
            stderr(output)
        );
    }

    // TC: SETUP-08
    #[test]
    fn keeps_the_binaries_when_a_binary_does_not_match_its_sum() {
        let release = Release::over_an_install("2");
        release.binary("liftoffd", "liftoffd tampered");
        let output = release.install();
        refused_and_kept(&release, &output);
        assert!(
            stderr(&output).contains("checksum mismatch"),
            "{}",
            stderr(&output)
        );
    }

    // TC: SETUP-08
    #[test]
    fn keeps_the_binaries_when_a_binary_is_missing_from_the_sums() {
        let release = Release::over_an_install("2");
        let sums_file = release.path(&format!("release/{}", sums()));
        let mut kept = String::new();
        for line in std::fs::read_to_string(&sums_file)
            .unwrap()
            .lines()
            .filter(|line| !line.contains("/liftoff-"))
        {
            kept.push_str(line);
            kept.push('\n');
        }
        std::fs::write(&sums_file, kept).unwrap();
        release.sign("release");
        let output = release.install();
        refused_and_kept(&release, &output);
        assert!(
            stderr(&output).contains("does not list both"),
            "{}",
            stderr(&output)
        );
    }

    // TC: SETUP-08 (the signature, not only the sums)
    #[test]
    fn keeps_the_binaries_when_the_sums_and_binaries_are_replaced_together() {
        let release = Release::over_an_install("2");
        release.binary("liftoffd", "liftoffd forged");
        release.sum_all();
        let output = release.install();
        refused_and_kept(&release, &output);
        assert!(stderr(&output).contains("signature"), "{}", stderr(&output));
    }

    // TC: SETUP-08
    #[test]
    fn keeps_the_binaries_when_another_key_signed_the_sums() {
        let release = Release::over_an_install("2");
        release.sign("other");
        let output = release.install();
        refused_and_kept(&release, &output);
        assert!(stderr(&output).contains("signature"), "{}", stderr(&output));
    }

    // TC: SETUP-08
    #[test]
    fn keeps_the_binaries_when_the_signature_is_missing() {
        let release = Release::over_an_install("2");
        std::fs::remove_file(release.path(&format!("release/{}.sig", sums()))).unwrap();
        let output = release.install();
        refused_and_kept(&release, &output);
    }

    // TC: SETUP-08 (fail closed: without LIFTOFF_RELEASE_KEY the pinned
    // release key decides, and the test release is not signed by it)
    #[test]
    fn keeps_the_binaries_when_the_release_is_not_signed_by_the_pinned_key() {
        let release = Release::over_an_install("2");
        let output = release.install_with_key(None);
        refused_and_kept(&release, &output);
        assert!(stderr(&output).contains("signature"), "{}", stderr(&output));
    }

    // TC: SETUP-08 (an empty key is no key: the pinned one decides)
    #[test]
    fn keeps_the_binaries_when_the_release_key_is_empty() {
        let release = Release::over_an_install("2");
        let output = release.install_with_key(Some(""));
        refused_and_kept(&release, &output);
        assert!(stderr(&output).contains("signature"), "{}", stderr(&output));
    }

    // TC: SETUP-08 (a downgrade to plain http is refused before any download)
    #[test]
    fn keeps_the_binaries_when_the_release_url_is_plain_http() {
        let release = Release::over_an_install("2");
        let output = release.install_from("http://127.0.0.1:9/release");
        refused_and_kept(&release, &output);
        assert!(stderr(&output).contains("https"), "{}", stderr(&output));
    }
}
