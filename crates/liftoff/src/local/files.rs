//! The files of local mode in the config home: where
//! each is, and how the private ones are made.
//!
//! The config home is the directory of the config file
//! (`~/.config/liftoff/`), made `0700`; the files that hold secrets
//! (`key`, `secrets.db`, `modules.db`) are created `0600`. `lof doctor`
//! warns when the directory or the key is wider ([`Home::key_mode`]).

use std::fmt::Write as _;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use liftoff_core::Error;
use liftoff_core::crypto::MasterKey;
use liftoff_core::manifest::RepoPath;

/// The files of local mode under one config home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Home {
    dir: PathBuf,
}

impl Home {
    /// The config home of the config file at `config_path`.
    pub(crate) fn of(config_path: &Path) -> Self {
        Self {
            dir: config_path
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
        }
    }

    /// The config home itself.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// `key`: the key of the local store (32 random bytes as 64 hex
    /// digits), `0600`.
    pub(crate) fn key(&self) -> PathBuf {
        self.dir.join("key")
    }

    /// `secrets.db`: the local store (secrets, forge connections, the
    /// installed modules), every secret in it sealed under [`Home::key`].
    pub(crate) fn secrets(&self) -> PathBuf {
        self.dir.join("secrets.db")
    }

    /// `modules.db`: the tables and key spaces of the modules.
    pub(crate) fn modules_db(&self) -> PathBuf {
        self.dir.join("modules.db")
    }

    /// `modules/`: the components the registry client downloaded.
    pub(crate) fn modules(&self) -> PathBuf {
        self.dir.join("modules")
    }

    /// `cache/`: wasmtime's compile cache of those components.
    pub(crate) fn cache(&self) -> PathBuf {
        self.dir.join("cache")
    }

    /// Makes the config home (`0700`, refused when it is a symlink or
    /// another user's: the module host runs what it finds in `cache/`).
    pub(crate) fn prepare(&self) -> Result<(), Error> {
        liftoff_wasm::private_dir(&self.dir).map_err(|error| io(&self.dir, &error))
    }

    /// The key of the local store, created on first use.
    pub(crate) fn master_key(&self) -> Result<MasterKey, Error> {
        self.prepare()?;
        let path = self.key();
        match std::fs::read(&path) {
            Ok(bytes) => MasterKey::from_key_bytes(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut key = [0_u8; 32];
                liftoff_core::system::fill(&mut key);
                let hex = key.iter().fold(String::new(), |mut hex, byte| {
                    let _ok = write!(hex, "{byte:02x}");
                    hex
                });
                let written = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .and_then(|mut file| {
                        file.write_all(hex.as_bytes())?;
                        file.sync_all()
                    });
                match written {
                    Ok(()) => MasterKey::from_key_bytes(hex.as_bytes()),
                    // Another run made it first: use that one.
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let bytes = std::fs::read(&path).map_err(|error| io(&path, &error))?;
                        MasterKey::from_key_bytes(&bytes)
                    }
                    Err(error) => Err(io(&path, &error)),
                }
            }
            Err(error) => Err(io(&path, &error)),
        }
    }

    /// The permission bits of the key, `None` when there is none yet.
    pub(crate) fn key_mode(&self) -> std::io::Result<Option<u32>> {
        mode(&self.key())
    }

    /// The permission bits of the config home, `None` when there is none.
    pub(crate) fn dir_mode(&self) -> std::io::Result<Option<u32>> {
        mode(&self.dir)
    }
}

/// The permission bits of `path`, `None` when it does not exist.
fn mode(path: &Path) -> std::io::Result<Option<u32>> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(meta.permissions().mode() & 0o777)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Creates `path` empty and `0600` unless it exists: SQLite then opens a
/// file only its owner reads.
pub(crate) fn private_file(path: &Path) -> Result<(), Error> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_file) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(io(path, &error)),
    }
}

/// `path` under the work tree `root`, when nothing on the way from `root`
/// to it is a symlink: what `create` and `server connect` write stays in
/// the project directory (a template's or a plan's path never leads
/// out). `None` when a component that exists is a symlink; `path` itself
/// is relative and has no `..` ([`RepoPath`]).
///
/// # Errors
///
/// When the metadata of a component cannot be read.
pub(crate) fn inside(root: &Path, path: &RepoPath) -> std::io::Result<Option<PathBuf>> {
    let mut at = root.to_path_buf();
    for component in Path::new(path.as_str()).components() {
        at.push(component);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(None),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Some(at))
}

/// An I/O failure on `path`, for the log and the person.
pub(crate) fn io(path: &Path, error: &std::io::Error) -> Error {
    Error::internal_msg(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(raw: &str) -> RepoPath {
        RepoPath::new(raw).unwrap()
    }

    mod inside {
        use super::*;

        // TC: LCL-03
        #[test]
        fn joins_path_when_no_component_is_a_symlink() {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("deploy")).unwrap();
            assert_eq!(
                inside(root.path(), &repo_path("deploy/compose.yaml")).ok(),
                Some(Some(root.path().join("deploy").join("compose.yaml")))
            );
        }

        // TC: LCL-03 (a symlinked file is not followed)
        #[test]
        fn none_when_file_is_a_symlink() {
            let root = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink("/etc/hostname", root.path().join("README.md")).unwrap();
            assert_eq!(
                inside(root.path(), &repo_path("README.md")).ok(),
                Some(None)
            );
        }

        // TC: LCL-04 (nor a symlinked directory on the way)
        #[test]
        fn none_when_directory_on_the_way_is_a_symlink() {
            let root = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink("/tmp", root.path().join(".github")).unwrap();
            assert_eq!(
                inside(root.path(), &repo_path(".github/workflows/deploy.yml")).ok(),
                Some(None)
            );
        }
    }
}
