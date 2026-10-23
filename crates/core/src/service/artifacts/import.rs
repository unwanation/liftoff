//! [`ArtifactImport`]: the rules of `liftoffd import-artifacts`, which brings the artifacts of
//! the standalone artifacts server over as they were.
//!
//! The daemon reads the predecessor's database and files; this decides
//! what each becomes: the same id, the owner by e-mail (else the admin,
//! IMPA-02), `shared` for good or private with the predecessor's
//! `expires_at` (an expired one is left to the sweeper, IMPA-01), one
//! version created at the predecessor's `created_at` whose files are
//! blobs by their relative paths (IMPA-03), and an id already stored
//! left as it is (IMPA-04). Neither the `authorize` hook nor
//! `[artifacts] max_size_mb` is asked: the import is the operator's, of
//! what the predecessor already held.

use super::{Checked, check, version_of, write_files};
use crate::domain::{
    Artifact, ArtifactId, ArtifactPath, Timestamp, User, VersionNumber, Visibility,
};
use crate::error::Error;
use crate::ports::{Blobs, Store};
use crate::service::{ArtifactContent, ArtifactUpload};

/// An artifact of the standalone artifacts server as the daemon read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedArtifact {
    /// Its id, kept.
    pub id: ArtifactId,
    /// The e-mail of its owner there; `None` when it had none.
    pub owner_email: Option<String>,
    /// The title, as is.
    pub title: String,
    /// Whether it was shared.
    pub shared: bool,
    /// When it was published.
    pub created_at: Timestamp,
    /// When it last changed.
    pub updated_at: Timestamp,
    /// When its TTL runs out (a private one).
    pub expires_at: Option<Timestamp>,
    /// Its `index.html`.
    pub html: Vec<u8>,
    /// The files beside it, by their relative paths.
    pub files: Vec<ArtifactUpload>,
}

/// What [`ArtifactImport::import`] did with one artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    /// Written as an artifact with one version.
    Imported {
        /// Whether no user, or more than one, has the owner's e-mail, so
        /// the admin owns it.
        owner_defaulted: bool,
    },
    /// Its id is stored already; nothing was changed (IMPA-04).
    AlreadyStored,
}

/// One run of the import over `store` and `blobs`.
///
/// Each artifact keeps its id, title and times; its owner is the one
/// user with its owner's e-mail, else (none, or several) the first admin
/// (IMPA-02); a shared one
/// is [`Visibility::Shared`] without expiry, a private one keeps its
/// `expires_at`, an expired one included (the sweeper takes it,
/// IMPA-01); it gets one version created at its `created_at` whose files
/// are blobs by their relative paths (IMPA-03); an id already stored is
/// left as it is (IMPA-04), unless an interrupted run left it at version
/// 1 without its `index.html`: that one is taken back and written again.
/// Neither the `authorize` hook nor
/// `[artifacts] max_size_mb` is asked.
#[derive(Debug)]
pub struct ArtifactImport<'a, S, B> {
    store: &'a S,
    blobs: &'a B,
    users: Vec<User>,
    admin: User,
}

impl<'a, S: Store, B: Blobs> ArtifactImport<'a, S, B> {
    /// Reads the users the owners are found among; the admin who owns
    /// what nobody else does is the first created.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when there is no admin; whatever the store
    /// reports.
    pub async fn new(store: &'a S, blobs: &'a B) -> Result<Self, Error> {
        let users = store.users().await?;
        let admin = users
            .iter()
            .filter(|user| user.is_admin())
            .min_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.login.as_str().cmp(b.login.as_str()))
            })
            .cloned()
            .ok_or_else(|| Error::not_found("an admin user to own the imported artifacts"))?;
        Ok(Self {
            store,
            blobs,
            users,
            admin,
        })
    }

    /// Whether the store holds the artifact `id` already, with its files
    /// (so reading them can be spared). One an interrupted run left
    /// without them is not.
    ///
    /// # Errors
    ///
    /// Whatever the store or the blobs report.
    pub async fn is_stored(&self, id: ArtifactId) -> Result<bool, Error> {
        Ok(self.stored(id).await? == Stored::Complete)
    }

    /// What the store holds of `id`: an artifact at version 1 whose
    /// `index.html` is not linked is what an interrupted import leaves
    /// (its rows are written before its files). A later version is the
    /// owner's, and stays whatever it holds.
    async fn stored(&self, id: ArtifactId) -> Result<Stored, Error> {
        let Some(artifact) = self.store.artifact(id).await? else {
            return Ok(Stored::Absent);
        };
        if artifact.current_version != VersionNumber::FIRST {
            return Ok(Stored::Complete);
        }
        match self
            .blobs
            .read(id, VersionNumber::FIRST, &ArtifactPath::index())
            .await
        {
            Ok(_html) => Ok(Stored::Complete),
            Err(Error::NotFound(_)) => Ok(Stored::Incomplete),
            Err(error) => Err(error),
        }
    }

    /// The one user whose e-mail is `email`; none for an empty one, and
    /// none (logged) when several share it, so the admin owns rather
    /// than whoever sorts first.
    fn owner_by(&self, id: ArtifactId, email: Option<&str>) -> Option<&User> {
        let email = email.filter(|email| !email.is_empty())?;
        let mut matching = self.users.iter().filter(|user| user.email == email);
        let first = matching.next()?;
        if matching.next().is_some() {
            tracing::warn!(artifact = %id, "more than one user has the owner's e-mail: the admin owns it");
            return None;
        }
        Some(first)
    }

    /// Writes `imported` as an artifact with one version, unless its id
    /// is stored already.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for two files of one name; whatever the
    /// store or the blobs report (the rows written are taken back).
    pub async fn import(&self, imported: ImportedArtifact) -> Result<ImportOutcome, Error> {
        match self.stored(imported.id).await? {
            Stored::Absent => {}
            Stored::Complete => return Ok(ImportOutcome::AlreadyStored),
            Stored::Incomplete => {
                tracing::warn!(artifact = %imported.id, "an earlier import left the artifact without its files: writing it again");
                self.take_back(imported.id).await?;
            }
        }
        let owner = self.owner_by(imported.id, imported.owner_email.as_deref());
        let owner_defaulted = owner.is_none();
        let owner = owner.unwrap_or(&self.admin);
        let (visibility, expires_at) = if imported.shared {
            (Visibility::Shared, None)
        } else {
            (Visibility::Private, imported.expires_at)
        };
        let artifact = Artifact {
            id: imported.id,
            owner_user_id: owner.id,
            title: imported.title,
            visibility,
            current_version: VersionNumber::FIRST,
            expires_at,
            created_at: imported.created_at,
            updated_at: imported.updated_at,
        };
        let checked: Checked = check(
            ArtifactContent {
                html: imported.html,
                files: imported.files,
                title: None,
            },
            u64::MAX,
        )?;
        let version = version_of(&artifact, &checked, owner, imported.created_at);
        match self.store.insert_artifact(&artifact, &version).await {
            Ok(()) => {}
            Err(Error::Conflict(_)) => return Ok(ImportOutcome::AlreadyStored),
            Err(error) => return Err(error),
        }
        if let Err(error) = write_files(self.blobs, &checked, &version).await {
            self.undo(artifact.id).await;
            return Err(error);
        }
        tracing::info!(artifact = %artifact.id, owner = %owner.login, "imported an artifact");
        Ok(ImportOutcome::Imported { owner_defaulted })
    }

    /// Takes back an artifact whose files could not be written; a run
    /// after a failure here writes it again ([`Self::stored`]).
    async fn undo(&self, id: ArtifactId) {
        if let Err(error) = self.take_back(id).await {
            tracing::error!(artifact = %id, %error, "could not take back a failed import");
        }
    }

    /// Removes the files and the rows of `id`.
    async fn take_back(&self, id: ArtifactId) -> Result<(), Error> {
        self.blobs.remove_artifact(id).await?;
        self.store.delete_artifact(id).await
    }
}

/// What the store holds of an id being imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stored {
    /// Nothing.
    Absent,
    /// The artifact, with its files.
    Complete,
    /// The rows an interrupted import wrote, without the files.
    Incomplete,
}

#[cfg(test)]
mod tests;
