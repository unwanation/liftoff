//! Real `git`, hermetic: a `GIT_CONFIG_GLOBAL` of the test's own (an
//! identity, and `https://github.com/` rewritten to bare repositories
//! under a tempdir), no system config, no prompt. The CLI's `git`
//! children get the same environment through `Local::with_git_env`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The tempdir with the git config and the "forge's" bare repositories.
#[derive(Debug)]
pub(crate) struct GitHome {
    dir: tempfile::TempDir,
}

impl GitHome {
    /// A fresh one: `gitconfig` and an empty `remotes/`.
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let remotes = dir.path().join("remotes");
        std::fs::create_dir_all(&remotes).unwrap();
        std::fs::write(
            dir.path().join("gitconfig"),
            format!(
                "[user]\n\tname = Owner\n\temail = owner@example.com\n[init]\n\tdefaultBranch = main\n[url \"file://{}/\"]\n\tinsteadOf = https://github.com/\n",
                remotes.display()
            ),
        )
        .unwrap();
        Self { dir }
    }

    /// The environment of every `git` of the test (and of the CLI's).
    pub(crate) fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "GIT_CONFIG_GLOBAL".to_owned(),
                self.dir.path().join("gitconfig").display().to_string(),
            ),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ]
    }

    /// The bare repository of `owner/name` on the "forge".
    pub(crate) fn remote(&self, repo: &str) -> PathBuf {
        self.dir.path().join("remotes").join(format!("{repo}.git"))
    }

    /// Runs `git args` in `dir`; its stdout.
    pub(crate) fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .envs(self.env())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Makes `dir` a repository on `main` whose one commit holds `files`.
    pub(crate) fn commit_tree(&self, dir: &Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        self.git(dir, &["init", "--quiet", "--initial-branch=main"]);
        for (path, content) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        self.git(dir, &["add", "--all"]);
        self.git(dir, &["commit", "--quiet", "--message", "template"]);
    }

    /// What the forge holds after it generated `owner/name` from a template
    /// of `files`: a bare repository of one commit on `main`, also reachable
    /// without the `.git` suffix.
    pub(crate) fn seed_remote(&self, repo: &str, files: &[(&str, &str)]) {
        let seed = self.dir.path().join("seed").join(repo);
        self.commit_tree(&seed, files);
        let bare = self.remote(repo);
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        self.git(
            self.dir.path(),
            &[
                "clone",
                "--quiet",
                "--bare",
                &seed.display().to_string(),
                &bare.display().to_string(),
            ],
        );
        let plain = bare.with_extension("");
        std::os::unix::fs::symlink(&bare, plain).unwrap();
    }

    /// [`GitHome::seed_remote`] whose template also holds the symlinks
    /// `links` (`(path, target)`), committed as symlinks.
    pub(crate) fn seed_remote_with_links(
        &self,
        repo: &str,
        files: &[(&str, &str)],
        links: &[(&str, &Path)],
    ) {
        let seed = self.dir.path().join("seed").join(repo);
        std::fs::create_dir_all(&seed).unwrap();
        for (path, target) in links {
            let link = seed.join(path);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, link).unwrap();
        }
        self.seed_remote(repo, files);
    }

    /// An empty bare repository for `owner/name` on the "forge", its
    /// `HEAD` on `main`.
    pub(crate) fn bare_remote(&self, repo: &str) {
        let bare = self.remote(repo);
        std::fs::create_dir_all(&bare).unwrap();
        self.git(
            &bare,
            &["init", "--quiet", "--bare", "--initial-branch=main"],
        );
    }

    /// `git show <rev>:<path>` of the bare repository of `repo`.
    pub(crate) fn remote_file(&self, repo: &str, rev: &str, path: &str) -> String {
        self.git(&self.remote(repo), &["show", &format!("{rev}:{path}")])
    }
}
