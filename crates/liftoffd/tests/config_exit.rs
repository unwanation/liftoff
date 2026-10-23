#![cfg(test)]
//! SETUP-05: a configuration the daemon cannot use ends the binary with
//! exit 78 before anything is opened, through the same entry
//! point as `main`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use liftoffd::error::AppError;
use liftoffd::setup::SetupError;

/// A complete configuration inside `dir`, plus `extra` TOML.
fn config(dir: &Path, key: &Path, extra: &str) -> String {
    format!(
        "[api]\npublic_url = \"https://liftoff.test\"\n\
         [domains]\nbase = \"apps.test\"\npublic_ip = \"203.0.113.10\"\n\
         [proxy]\ntls = \"edge\"\nedge_port = 7002\nadmin = \"127.0.0.1:9\"\n\
         [store]\npath = \"{db}\"\n\
         [paths]\ndata = \"{data}\"\nmaster_key = \"{key}\"\n{extra}",
        db = dir.join("liftoff.db").display(),
        data = dir.display(),
        key = key.display(),
    )
}

async fn run_with(file: &Path) -> u8 {
    liftoffd::run_cli(["liftoffd", "--config", file.to_str().unwrap(), "serve"]).await
}

mod run_cli {
    use super::*;

    // TC: SETUP-05
    #[tokio::test]
    async fn exits_78_when_the_file_has_an_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("master.key");
        std::fs::write(&key, "00".repeat(32)).unwrap();
        let file = dir.path().join("liftoffd.toml");
        std::fs::write(
            &file,
            config(dir.path(), &key, "[deploy]\nparallelism = 4\n"),
        )
        .unwrap();
        assert_eq!(run_with(&file).await, 78);
    }

    // TC: SETUP-05
    #[tokio::test]
    async fn exits_78_when_the_file_has_an_unknown_section() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("master.key");
        std::fs::write(&key, "00".repeat(32)).unwrap();
        let file = dir.path().join("liftoffd.toml");
        std::fs::write(&file, config(dir.path(), &key, "[deploys]\nparallel = 4\n")).unwrap();
        assert_eq!(run_with(&file).await, 78);
    }

    // TC: SETUP-05 (secrets only as files)
    #[tokio::test]
    async fn exits_78_without_a_database_when_the_master_key_is_given_inline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("liftoffd.toml");
        let inline = PathBuf::from("00".repeat(32));
        std::fs::write(&file, config(dir.path(), &inline, "")).unwrap();
        assert_eq!(
            (
                run_with(&file).await,
                dir.path().join("liftoff.db").exists()
            ),
            (78, false)
        );
    }

    // TC: SETUP-05 (an empty secret file is exit 78)
    #[tokio::test]
    async fn exits_78_without_a_database_when_the_master_key_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("master.key");
        std::fs::write(&key, "").unwrap();
        let file = dir.path().join("liftoffd.toml");
        std::fs::write(&file, config(dir.path(), &key, "")).unwrap();
        assert_eq!(
            (
                run_with(&file).await,
                dir.path().join("liftoff.db").exists()
            ),
            (78, false)
        );
    }

    // TC: SETUP-05 (a broken `master.key` is exit 78)
    #[tokio::test]
    async fn exits_78_without_a_database_when_the_master_key_is_broken() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("master.key");
        std::fs::write(&key, "zz".repeat(32)).unwrap();
        let file = dir.path().join("liftoffd.toml");
        std::fs::write(&file, config(dir.path(), &key, "")).unwrap();
        assert_eq!(
            (
                run_with(&file).await,
                dir.path().join("liftoff.db").exists()
            ),
            (78, false)
        );
    }

    // TC: SETUP-05
    #[tokio::test]
    async fn exits_78_when_the_named_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_with(&dir.path().join("nope.toml")).await, 78);
    }
}

mod exit_code {
    use super::*;

    // TC: SETUP-01 (a missing answer is a usage error)
    #[test]
    fn is_64_when_setup_misses_an_answer() {
        let error = AppError::from(SetupError::Missing(vec!["--base-domain"]));
        assert_eq!(error.exit_code(), 64);
    }

    // TC: SETUP-01
    #[test]
    fn is_64_when_a_setup_answer_is_malformed() {
        let error = AppError::from(SetupError::Invalid {
            flag: "--base-domain",
            reason: "is not a host".to_owned(),
        });
        assert_eq!(error.exit_code(), 64);
    }
}

mod setup_binary {
    use super::*;

    // TC: SETUP-01 (without a terminal a missing answer is refused with 64
    // before the first host step)
    #[test]
    fn exits_64_when_setup_misses_the_answers_without_a_terminal() {
        let output = Command::new(env!("CARGO_BIN_EXE_liftoffd"))
            .args(["setup", "--force"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(64),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
