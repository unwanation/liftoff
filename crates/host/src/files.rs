//! Plain files on disk with the permissions liftoff promises.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use liftoff_core::Error;
use liftoff_core::domain::ComposeProject;
use liftoff_core::ports::{ProjectFile, ProjectFiles};

/// Mode of every file liftoff writes (env files hold secrets).
pub(crate) const FILE_MODE: u32 = 0o640;
/// Mode of a compose project directory.
const DIR_MODE: u32 = 0o750;

/// Writes `content` to `path` and makes the mode exactly [`FILE_MODE`]
/// whatever the umask or a previous file was.
pub(crate) async fn write_private(path: &Path, content: &str) -> io::Result<()> {
    tokio::fs::write(path, content).await?;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(FILE_MODE)).await
}

/// Reads a file; `None` when it does not exist.
pub(crate) async fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// An I/O failure is the adapter's own fault: internal, logged, never shown.
pub(crate) fn io_error(what: &str, path: &Path, source: &io::Error) -> Error {
    Error::internal(io::Error::new(
        source.kind(),
        format!("{what} {}: {source}", path.display()),
    ))
}

/// Project directories under `<projects_dir>/<project>/`.
#[derive(Debug, Clone)]
pub struct FileProjectFiles {
    projects_dir: PathBuf,
}

impl FileProjectFiles {
    /// `projects_dir` holds one directory per project.
    #[must_use]
    pub fn new(projects_dir: impl Into<PathBuf>) -> Self {
        Self {
            projects_dir: projects_dir.into(),
        }
    }

    fn dir(&self, project: &ComposeProject) -> PathBuf {
        self.projects_dir.join(project.as_str())
    }
}

impl ProjectFiles for FileProjectFiles {
    async fn write(
        &self,
        project: &ComposeProject,
        file: ProjectFile,
        content: &str,
    ) -> Result<(), Error> {
        let dir = self.dir(project);
        let prepare = async {
            tokio::fs::create_dir_all(&dir).await?;
            tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(DIR_MODE)).await
        };
        prepare
            .await
            .map_err(|source| io_error("cannot create", &dir, &source))?;
        let path = dir.join(file.file_name());
        write_private(&path, content)
            .await
            .map_err(|source| io_error("cannot write", &path, &source))
    }

    async fn read(
        &self,
        project: &ComposeProject,
        file: ProjectFile,
    ) -> Result<Option<String>, Error> {
        let path = self.dir(project).join(file.file_name());
        read_optional(&path)
            .await
            .map_err(|source| io_error("cannot read", &path, &source))
    }

    async fn remove_all(&self, project: &ComposeProject) -> Result<(), Error> {
        let dir = self.dir(project);
        match tokio::fs::remove_dir_all(&dir).await {
            Err(source) if source.kind() != io::ErrorKind::NotFound => {
                Err(io_error("cannot remove", &dir, &source))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use liftoff_core::domain::{EnvName, ProjectName};

    use super::*;

    fn project() -> ComposeProject {
        let project = ProjectName::new("acme").expect("valid project");
        let env = EnvName::new("qa").expect("valid env");
        ComposeProject::new(&project, &env)
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("exists")
            .permissions()
            .mode()
            & 0o777
    }

    mod project_files {
        use super::*;

        #[tokio::test]
        async fn creates_directory_0750_and_file_0640_when_first_write() {
            let root = tempfile::tempdir().expect("tempdir");
            let files = FileProjectFiles::new(root.path());
            files
                .write(&project(), ProjectFile::Env, "A=1\n")
                .await
                .expect("writes");
            let dir = root.path().join("lf_acme_qa");
            assert_eq!(mode(&dir), 0o750);
            assert_eq!(mode(&dir.join(".env")), 0o640);
        }

        #[tokio::test]
        async fn returns_what_was_written_when_read_back() {
            let root = tempfile::tempdir().expect("tempdir");
            let files = FileProjectFiles::new(root.path());
            files
                .write(&project(), ProjectFile::Compose, "services: {}\n")
                .await
                .expect("writes");
            let read = files.read(&project(), ProjectFile::Compose).await;
            assert_eq!(read.expect("reads").as_deref(), Some("services: {}\n"));
        }

        #[tokio::test]
        async fn none_when_file_is_absent() {
            let root = tempfile::tempdir().expect("tempdir");
            let files = FileProjectFiles::new(root.path());
            let read = files.read(&project(), ProjectFile::Override).await;
            assert_eq!(read.expect("reads"), None);
        }

        #[tokio::test]
        async fn removes_everything_when_directory_exists() {
            let root = tempfile::tempdir().expect("tempdir");
            let files = FileProjectFiles::new(root.path());
            files
                .write(&project(), ProjectFile::Env, "A=1\n")
                .await
                .expect("writes");
            files.remove_all(&project()).await.expect("removes");
            assert!(!root.path().join("lf_acme_qa").exists());
        }

        #[tokio::test]
        async fn succeeds_when_directory_is_already_gone() {
            let root = tempfile::tempdir().expect("tempdir");
            let files = FileProjectFiles::new(root.path());
            assert!(files.remove_all(&project()).await.is_ok());
        }
    }
}
