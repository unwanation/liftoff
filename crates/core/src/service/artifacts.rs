//! [`ArtifactsService`].
//!
//! - [`ArtifactsService::publish`] (`lof artifact publish`, MCP
//!   `publish_artifact`): version 1 of a new private artifact, whose TTL
//!   runs from now, its address and, while it is private, a one-time
//!   link for its owner's browser (ART-01);
//! - [`ArtifactsService::update`] and [`ArtifactsService::rollback`]: a
//!   new current version (HTML and files as a whole; a rollback copies
//!   an older one), the oldest dropped with their files beyond
//!   `[artifacts] versions` (ART-04, ART-05, ART-09); the TTL of a
//!   private artifact runs from its latest version (ART-07);
//! - [`ArtifactsService::share`] lifts the TTL and opens it to anybody,
//!   [`ArtifactsService::unshare`] makes it private again with a TTL
//!   from now (ART-06);
//! - [`ArtifactsService::delete`] and [`ArtifactsService::delete_version`]
//!   (never the current one: ART-10, ART-14);
//! - [`ArtifactsService::sweep`] deletes the private artifacts whose TTL
//!   ran out (ART-08); the daemon runs it every minute.
//!
//! Names of files are [`ArtifactPath`]s, checked where they come in
//! (ART-02); the sum of a version's bytes is held to
//! `[artifacts] max_size_mb` before anything is written (ART-03); each
//! distinct content is a blob stored once (ART-13). Every operation of a
//! user asks the `authorize` hook once; who is neither the owner nor an
//! admin may not change an artifact even when the hook allows (ART-11).

use std::collections::BTreeSet;
use std::sync::Arc;

use super::links::{IssuedLink, LinksService};
use super::settings::Settings;
use super::support::{authorize, authorized};
use crate::domain::{
    Artifact, ArtifactFile, ArtifactId, ArtifactPath, ArtifactVersion, BlobHash, LinkTarget,
    Timestamp, User, VersionNumber, Visibility, clean_title, title_of,
};
use crate::error::{Error, Problem, ValidationError};
use crate::ports::{Action, Blobs, Clock as _, Ports, Resource, Rng as _, Store as _};

/// How many ids [`ArtifactsService::publish`] draws before it gives up
/// on one nobody holds (a clash of 71 random bits is all but
/// impossible; the store's uniqueness decides).
const ID_ATTEMPTS: usize = 3;

/// The title of a version that has none of its own, in its HTML or in
/// the version before it (as the standalone artifacts server).
const UNTITLED: &str = "Untitled";

/// A file sent with a version: its checked name and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactUpload {
    /// The relative path the HTML refers to it by.
    pub name: ArtifactPath,
    /// The content.
    pub bytes: Vec<u8>,
}

/// What a version is made of: the HTML (`index.html` of the version),
/// the files next to it, and a title.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArtifactContent {
    /// The page.
    pub html: Vec<u8>,
    /// The files next to it; two with one name are refused.
    pub files: Vec<ArtifactUpload>,
    /// The title; `None` takes the HTML's `<title>`, else the previous
    /// version's, else `Untitled` (as the standalone artifacts server).
    pub title: Option<String>,
}

/// What [`ArtifactsService::publish`], [`ArtifactsService::update`] and
/// [`ArtifactsService::rollback`] answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifact {
    /// The artifact as stored now.
    pub artifact: Artifact,
    /// The version just made, now the current one.
    pub version: ArtifactVersion,
    /// `https://<api host>/a/<id>`, or `https://<artifacts.host>/<id>`
    /// when it has a host of its own.
    pub url: String,
    /// For [`ArtifactsService::publish`] of a private artifact: a
    /// one-time link (target `artifact:<id>`) that opens a `browser`
    /// session of its owner. `None` otherwise.
    pub login_url: Option<IssuedLink>,
}

/// What the wrapper page of an artifact shows (`GET /a/{id}`): the artifact and every kept version, oldest first, with
/// its number, title and date for the switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactView {
    /// The artifact as stored now.
    pub artifact: Artifact,
    /// Every kept version, oldest first.
    pub versions: Vec<ArtifactVersion>,
    /// Whether the viewer may share the artifact (when `private`) or
    /// unshare it (when `shared`): the rule of
    /// [`ArtifactsService::share`] and [`ArtifactsService::unshare`]
    /// (its owner or an admin, the `authorize` hook first), so the
    /// wrapper shows the button exactly when the press would pass.
    pub may_change_visibility: bool,
}

/// A file of a version as the viewer serves it (`/a/{id}/v/{n}/{name}`): its bytes and the artifact's visibility,
/// which decides how long a browser may keep them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedFile {
    /// The visibility of the artifact now.
    pub visibility: Visibility,
    /// The bytes of the file.
    pub bytes: Vec<u8>,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct ArtifactsService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for ArtifactsService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

/// A version checked against the rules and the limit, not yet written:
/// its files (`index.html` first, then by path) with their bytes.
struct Checked {
    files: Vec<(ArtifactFile, Vec<u8>)>,
    size_bytes: u64,
    /// Whether the bytes are new content to [`Blobs::put`](crate::ports::Blobs::put);
    /// `false` for a rollback, whose blobs are stored already and whose
    /// bytes are empty (ART-09, ART-13).
    fresh: bool,
}

impl Checked {
    /// The distinct contents of the version, each once (ART-13).
    fn blobs(&self) -> Vec<(&BlobHash, &[u8])> {
        let mut seen = BTreeSet::new();
        self.files
            .iter()
            .filter(|(file, _)| seen.insert(&file.hash))
            .map(|(file, bytes)| (&file.hash, bytes.as_slice()))
            .collect()
    }
}

/// Checks `content` against the rules of a version and `max_bytes`
/// (ART-02, ART-03): one file per name, the sum of the bytes with the
/// HTML at most the limit.
fn check(content: ArtifactContent, max_bytes: u64) -> Result<Checked, Error> {
    let mut seen = BTreeSet::new();
    for upload in &content.files {
        if !seen.insert(upload.name.clone()) {
            return Err(ValidationError::at(
                "files",
                upload.name.as_str(),
                Problem::Invalid("two files have this name".to_owned()),
            )
            .into());
        }
    }
    let size_bytes = content
        .files
        .iter()
        .map(|upload| upload.bytes.len())
        .fold(content.html.len(), usize::saturating_add);
    let size_bytes = u64::try_from(size_bytes).unwrap_or(u64::MAX);
    if size_bytes > max_bytes {
        return Err(Error::invalid(
            "files",
            format!(
                "the version has {size_bytes} bytes with its HTML; \
                 at most {max_bytes} are allowed (`[artifacts] max_size_mb`)"
            ),
        ));
    }
    let mut uploads = content.files;
    uploads.sort_by(|a, b| a.name.cmp(&b.name));
    let files = core::iter::once((ArtifactPath::index(), content.html))
        .chain(
            uploads
                .into_iter()
                .map(|upload| (upload.name, upload.bytes)),
        )
        .map(|(name, bytes)| {
            let file = ArtifactFile {
                name,
                hash: BlobHash::of(&bytes),
                size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            };
            (file, bytes)
        })
        .collect();
    Ok(Checked {
        files,
        size_bytes,
        fresh: true,
    })
}

/// The title of a version: the one given, else the HTML's `<title>`,
/// else `previous` (the version before it), else `Untitled`.
fn title(given: Option<&str>, html: &[u8], previous: Option<&str>) -> String {
    given
        .and_then(clean_title)
        .or_else(|| title_of(html))
        .or_else(|| previous.map(str::to_owned))
        .unwrap_or_else(|| UNTITLED.to_owned())
}

/// What `checked` is as version `artifact.current_version`, titled as
/// `artifact`, by `actor` at `now`.
fn version_of(
    artifact: &Artifact,
    checked: &Checked,
    actor: &User,
    now: Timestamp,
) -> ArtifactVersion {
    ArtifactVersion {
        artifact_id: artifact.id,
        number: artifact.current_version,
        title: artifact.title.clone(),
        size_bytes: checked.size_bytes,
        created_by: actor.id,
        created_at: now,
        files: checked.files.iter().map(|(file, _)| file.clone()).collect(),
    }
}

/// Whether `artifact` is private and its TTL ran out at `now`: it no
/// longer exists for anybody, swept or not (ART-08).
fn expired(artifact: &Artifact, now: Timestamp) -> bool {
    artifact.visibility == Visibility::Private && artifact.expires_at.is_some_and(|at| at <= now)
}

/// [`Error::Forbidden`] unless `actor` owns `artifact` or is an admin
/// (ART-11), whatever the `authorize` hook said.
fn owner_or_admin(actor: &User, artifact: &Artifact, action: Action) -> Result<(), Error> {
    if artifact.owner_user_id == actor.id || actor.is_admin() {
        Ok(())
    } else {
        Err(Error::forbidden(format!(
            "{} may not {action}: artifact {} is another user's",
            actor.login, artifact.id
        )))
    }
}

/// Whether `actor` may share `artifact` (when `private`) or unshare it
/// (when `shared`): its owner or an admin, once the `authorize` hook
/// lets them (asked only of the owner or an admin).
async fn may_change_visibility<P: Ports>(ports: &P, actor: &User, artifact: &Artifact) -> bool {
    let action = if artifact.visibility == Visibility::Shared {
        Action::ArtifactUnshare
    } else {
        Action::ArtifactShare
    };
    owner_or_admin(actor, artifact, action).is_ok()
        && authorize(ports, actor, action, &Resource::Artifact(artifact.id))
            .await
            .is_ok()
}

impl<P: Ports> ArtifactsService<P> {
    /// A service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// The address of the artifact `id`.
    #[must_use]
    pub fn url(&self, id: ArtifactId) -> String {
        self.settings.artifacts.host.as_ref().map_or_else(
            || format!("https://{}/a/{id}", self.settings.proxy.api_host),
            |host| format!("https://{host}/{id}"),
        )
    }

    /// `publish`: version 1 of a new artifact of `actor`, private until
    /// now + `[artifacts] ttl`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a version above `max_size_mb` or two
    /// files of one name (nothing is written); [`Error::Forbidden`] when
    /// the `authorize` hook refuses ([`Action::ArtifactPublish`] about
    /// [`Resource::Admin`]); whatever the store or the blobs report.
    pub async fn publish(
        &self,
        actor: &User,
        content: ArtifactContent,
    ) -> Result<PublishedArtifact, Error> {
        let ports = self.ports.as_ref();
        authorize(ports, actor, Action::ArtifactPublish, &Resource::Admin).await?;
        let title = title(content.title.as_deref(), &content.html, None);
        let checked = check(content, self.settings.artifacts.max_size_bytes)?;
        let now = ports.clock().now();
        let mut attempts: usize = 0;
        let (artifact, version) = loop {
            attempts = attempts.saturating_add(1);
            let id = ArtifactId::from_secret(&ports.rng().token_secret());
            let artifact = Artifact {
                id,
                owner_user_id: actor.id,
                title: title.clone(),
                visibility: Visibility::Private,
                current_version: VersionNumber::FIRST,
                expires_at: Some(self.expiry(now)),
                created_at: now,
                updated_at: now,
            };
            let version = version_of(&artifact, &checked, actor, now);
            match ports.store().insert_artifact(&artifact, &version).await {
                Ok(()) => break (artifact, version),
                Err(Error::Conflict(_)) if attempts < ID_ATTEMPTS => {}
                Err(error) => return Err(error),
            }
        };
        let links = LinksService::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        let written = async {
            self.write_files(&checked, &version).await?;
            links
                .issue_checked(actor, &LinkTarget::Artifact(artifact.id))
                .await
        };
        let login_url = match written.await {
            Ok(issued) => Some(issued),
            Err(error) => {
                self.undo_publish(artifact.id).await;
                return Err(error);
            }
        };
        tracing::info!(artifact = %artifact.id, by = %actor.login, "published an artifact");
        Ok(PublishedArtifact {
            url: self.url(artifact.id),
            artifact,
            version,
            login_url,
        })
    }

    /// `update`: a new version `n + 1` of `id` from `content`, current
    /// now; a private artifact's TTL runs from now again.
    ///
    /// # Errors
    ///
    /// As [`ArtifactsService::publish`]; [`Error::NotFound`] for an
    /// unknown or expired artifact; [`Error::Forbidden`] for who is
    /// neither its owner nor an admin.
    pub async fn update(
        &self,
        actor: &User,
        id: ArtifactId,
        content: ArtifactContent,
    ) -> Result<PublishedArtifact, Error> {
        let artifact = self.authorized(actor, Action::ArtifactUpdate, id).await?;
        let title = title(
            content.title.as_deref(),
            &content.html,
            Some(&artifact.title),
        );
        let checked = check(content, self.settings.artifacts.max_size_bytes)?;
        self.add_version(actor, artifact, title, &checked).await
    }

    /// `share`: open to anybody, no TTL.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown or expired artifact;
    /// [`Error::Forbidden`] for who is neither its owner nor an admin, or
    /// when the `authorize` hook refuses; whatever the store reports.
    pub async fn share(&self, actor: &User, id: ArtifactId) -> Result<Artifact, Error> {
        let artifact = self.authorized(actor, Action::ArtifactShare, id).await?;
        if artifact.visibility == Visibility::Shared {
            return Ok(artifact);
        }
        let shared = Artifact {
            visibility: Visibility::Shared,
            expires_at: None,
            updated_at: self.ports.clock().now(),
            ..artifact
        };
        self.ports.store().update_artifact(&shared).await?;
        tracing::info!(artifact = %id, by = %actor.login, "shared an artifact");
        Ok(shared)
    }

    /// `unshare`: private again, until now + `[artifacts] ttl`.
    ///
    /// # Errors
    ///
    /// As [`ArtifactsService::share`].
    pub async fn unshare(&self, actor: &User, id: ArtifactId) -> Result<Artifact, Error> {
        let artifact = self.authorized(actor, Action::ArtifactUnshare, id).await?;
        let now = self.ports.clock().now();
        let private = Artifact {
            visibility: Visibility::Private,
            expires_at: Some(self.expiry(now)),
            updated_at: now,
            ..artifact
        };
        self.ports.store().update_artifact(&private).await?;
        tracing::info!(artifact = %id, by = %actor.login, "unshared an artifact");
        Ok(private)
    }

    /// `rollback --to N`: version `to` copied as a new current version
    /// (the same files and blobs).
    ///
    /// # Errors
    ///
    /// As [`ArtifactsService::share`]; [`Error::NotFound`] for an unknown
    /// version.
    pub async fn rollback(
        &self,
        actor: &User,
        id: ArtifactId,
        to: VersionNumber,
    ) -> Result<PublishedArtifact, Error> {
        let artifact = self.authorized(actor, Action::ArtifactRollback, id).await?;
        let source = self
            .ports
            .store()
            .artifact_version(id, to)
            .await?
            .ok_or_else(|| Error::not_found(format!("version {to} of artifact {id}")))?;
        // The same blobs: nothing is put (ART-09, ART-13).
        let checked = Checked {
            files: source
                .files
                .into_iter()
                .map(|file| (file, Vec::new()))
                .collect(),
            size_bytes: source.size_bytes,
            fresh: false,
        };
        self.add_version(actor, artifact, source.title, &checked)
            .await
    }

    /// `rm <id>`: every version, and every blob no other artifact names.
    ///
    /// # Errors
    ///
    /// As [`ArtifactsService::share`].
    pub async fn delete(&self, actor: &User, id: ArtifactId) -> Result<(), Error> {
        let _artifact = self.authorized(actor, Action::ArtifactRemove, id).await?;
        self.remove(id).await?;
        tracing::info!(artifact = %id, by = %actor.login, "deleted an artifact");
        Ok(())
    }

    /// `rm <id> --version N`: one version that is not the current one,
    /// with its files.
    ///
    /// # Errors
    ///
    /// [`Error::Conflict`] for the current version; as
    /// [`ArtifactsService::rollback`] otherwise.
    pub async fn delete_version(
        &self,
        actor: &User,
        id: ArtifactId,
        version: VersionNumber,
    ) -> Result<(), Error> {
        let artifact = self
            .authorized(actor, Action::ArtifactRemoveVersion, id)
            .await?;
        if artifact.current_version == version {
            return Err(Error::conflict(format!(
                "version {version} is the current version of artifact {id}; \
                 roll back or update first"
            )));
        }
        // Files first: a failure leaves the version whole, to delete
        // again; its row without files would be served as `404` only.
        self.ports.blobs().remove_version(id, version).await?;
        self.ports
            .store()
            .delete_artifact_version(id, version)
            .await?;
        tracing::info!(artifact = %id, %version, by = %actor.login, "deleted an artifact version");
        Ok(())
    }

    /// `ls`: `actor`'s own artifacts, newest first; with `all`, every
    /// user's, for an admin only.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] for `all` asked by who is not an admin, or
    /// when the `authorize` hook refuses; whatever the store reports.
    pub async fn list(&self, actor: &User, all: bool) -> Result<Vec<Artifact>, Error> {
        let ports = self.ports.as_ref();
        authorize(ports, actor, Action::ArtifactList, &Resource::Admin).await?;
        if all && !actor.is_admin() {
            return Err(Error::forbidden(format!(
                "{} may not list every user's artifacts: that is the admin's",
                actor.login
            )));
        }
        let owner = if all { None } else { Some(actor.id) };
        let now = ports.clock().now();
        Ok(ports
            .store()
            .artifacts(owner)
            .await?
            .into_iter()
            .filter(|artifact| !expired(artifact, now))
            .collect())
    }

    /// `GET /artifacts/{id}`: the artifact as
    /// stored now, for its owner or an admin. It asks the `authorize`
    /// hook its own question ([`Action::ArtifactGet`]), apart from
    /// reading the versions, so modules and the audit tell them apart.
    ///
    /// # Errors
    ///
    /// As [`ArtifactsService::share`].
    pub async fn get(&self, actor: &User, id: ArtifactId) -> Result<Artifact, Error> {
        self.authorized(actor, Action::ArtifactGet, id).await
    }

    /// `versions <id>`: every kept version, oldest first.
    ///
    /// # Errors
    ///
    /// As [`ArtifactsService::share`].
    pub async fn versions(
        &self,
        actor: &User,
        id: ArtifactId,
    ) -> Result<Vec<ArtifactVersion>, Error> {
        let _artifact = self.authorized(actor, Action::ArtifactVersions, id).await?;
        self.ports.store().artifact_versions(id).await
    }

    /// The bytes of the file `name` of version `version` of `id`, for a
    /// request [`Access::check`](super::Access::check) let through
    /// (`/a/{id}/v/{n}/{name}`).
    ///
    /// Only a file the stored version names is read: the files of a
    /// version whose row is gone (dropped by trimming or deleted while
    /// its files could not be removed) are never served.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown or expired artifact, version
    /// or file; whatever the store or the blobs report.
    pub async fn content(
        &self,
        id: ArtifactId,
        version: VersionNumber,
        name: &ArtifactPath,
    ) -> Result<Vec<u8>, Error> {
        self.served(id, version, name).await.map(|file| file.bytes)
    }

    /// [`ArtifactsService::content`] with the artifact's visibility, as
    /// the viewer serves it.
    ///
    /// # Errors
    ///
    /// As [`ArtifactsService::content`].
    pub async fn served(
        &self,
        id: ArtifactId,
        version: VersionNumber,
        name: &ArtifactPath,
    ) -> Result<ServedFile, Error> {
        let ports = self.ports.as_ref();
        let artifact = live(ports, id).await?;
        let named = ports
            .store()
            .artifact_version(id, version)
            .await?
            .is_some_and(|stored| stored.files.iter().any(|file| file.name == *name));
        if !named {
            return Err(Error::not_found(format!(
                "{name} of version {version} of artifact {id}"
            )));
        }
        let bytes = ports.blobs().read(id, version, name).await?;
        Ok(ServedFile {
            visibility: artifact.visibility,
            bytes,
        })
    }

    /// What the wrapper page of `id` shows to `viewer` (`None` without a
    /// session), for a request [`Access::check`](super::Access::check)
    /// let through (`/a/{id}`, `/a/{id}/v/{n}`):
    /// opening it asks no `authorize` hook, since a `shared` artifact is
    /// open to anybody; whether `viewer` gets the share or unshare
    /// button asks it once, as the press would.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown or expired artifact; whatever
    /// the store reports.
    pub async fn view(&self, id: ArtifactId, viewer: Option<&User>) -> Result<ArtifactView, Error> {
        let ports = self.ports.as_ref();
        let artifact = live(ports, id).await?;
        let versions = ports.store().artifact_versions(id).await?;
        let may_change_visibility = match viewer {
            Some(viewer) => may_change_visibility(ports, viewer, &artifact).await,
            None => false,
        };
        Ok(ArtifactView {
            artifact,
            versions,
            may_change_visibility,
        })
    }

    /// Deletes every private artifact whose `expires_at` is not after
    /// now, with its files; shared ones stay (ART-08). The ids deleted.
    ///
    /// One that cannot be deleted is logged and left for the next sweep.
    ///
    /// # Errors
    ///
    /// Whatever the store reports when it lists them.
    pub async fn sweep(&self) -> Result<Vec<ArtifactId>, Error> {
        let now = self.ports.clock().now();
        let mut swept = Vec::new();
        for artifact in self.ports.store().expired_artifacts(now).await? {
            match self.remove(artifact.id).await {
                Ok(()) => swept.push(artifact.id),
                Err(error) => {
                    tracing::error!(artifact = %artifact.id, %error, "could not sweep an expired artifact");
                }
            }
        }
        if !swept.is_empty() {
            tracing::info!(count = swept.len(), "swept expired artifacts");
        }
        Ok(swept)
    }

    /// When a private artifact whose TTL starts at `now` expires.
    fn expiry(&self, now: Timestamp) -> Timestamp {
        let ttl =
            time::Duration::try_from(self.settings.artifacts.ttl).unwrap_or(time::Duration::MAX);
        now.saturating_add(ttl)
    }

    /// The live artifact `id`, once `actor` may do `action` on it: the
    /// one question to `authorize`, then the owner rule (ART-11).
    async fn authorized(
        &self,
        actor: &User,
        action: Action,
        id: ArtifactId,
    ) -> Result<Artifact, Error> {
        let ports = self.ports.as_ref();
        let found = live(ports, id).await;
        let artifact = authorized(ports, actor, action, found, |artifact| {
            Resource::Artifact(artifact.id)
        })
        .await?;
        owner_or_admin(actor, &artifact, action)?;
        Ok(artifact)
    }

    /// Stores each distinct new content of `checked` once (nothing for a
    /// rollback), then links the files of `version`, whose rows are
    /// written, to their blobs, then releases every blob it put, linked
    /// or not (so a failed write leaves no blob behind).
    async fn write_files(&self, checked: &Checked, version: &ArtifactVersion) -> Result<(), Error> {
        write_files(self.ports.blobs(), checked, version).await
    }

    /// Version `n + 1` of `artifact` from `checked`, current now, titled
    /// `title`: its rows, then its blobs and links; the TTL of a private
    /// artifact runs from now (ART-07); the oldest versions beyond
    /// `[artifacts] versions` go with their files (ART-05).
    async fn add_version(
        &self,
        actor: &User,
        artifact: Artifact,
        title: String,
        checked: &Checked,
    ) -> Result<PublishedArtifact, Error> {
        let ports = self.ports.as_ref();
        let now = ports.clock().now();
        let number = artifact.current_version.next().ok_or_else(|| {
            Error::conflict(format!("artifact {} has no versions left", artifact.id))
        })?;
        let previous = artifact.clone();
        let expires_at = match artifact.visibility {
            Visibility::Private => Some(self.expiry(now)),
            Visibility::Shared => None,
        };
        let updated = Artifact {
            title,
            current_version: number,
            expires_at,
            updated_at: now,
            ..artifact
        };
        let version = version_of(&updated, checked, actor, now);
        ports
            .store()
            .add_artifact_version(&updated, &version)
            .await?;
        if let Err(error) = self.write_files(checked, &version).await {
            self.undo_version(&previous, number).await;
            return Err(error);
        }
        self.trim(updated.id).await;
        tracing::info!(
            artifact = %updated.id,
            version = %number,
            by = %actor.login,
            "added an artifact version"
        );
        Ok(PublishedArtifact {
            url: self.url(updated.id),
            artifact: updated,
            version,
            login_url: None,
        })
    }

    /// Drops the oldest versions of `id` beyond `[artifacts] versions`
    /// with their files (ART-05). The new version is written by then, so
    /// a failure here is logged and the next version trims again.
    async fn trim(&self, id: ArtifactId) {
        let ports = self.ports.as_ref();
        let trimmed = async {
            let dropped = ports
                .store()
                .prune_artifact_versions(id, self.settings.artifacts.versions)
                .await?;
            for old in dropped {
                ports.blobs().remove_version(id, old).await?;
            }
            Ok::<(), Error>(())
        };
        if let Err(error) = trimmed.await {
            tracing::error!(artifact = %id, %error, "could not drop old artifact versions");
        }
    }

    /// Deletes the artifact `id` and its files: files first, so a failure
    /// leaves the artifact to delete (or sweep) again.
    async fn remove(&self, id: ArtifactId) -> Result<(), Error> {
        self.ports.blobs().remove_artifact(id).await?;
        self.ports.store().delete_artifact(id).await
    }

    /// Takes back a publish whose files could not be written or whose
    /// login link could not be issued.
    async fn undo_publish(&self, id: ArtifactId) {
        if let Err(error) = self.remove(id).await {
            tracing::error!(artifact = %id, %error, "could not take back a failed publish");
        }
    }

    /// Takes back version `number` whose files could not be written:
    /// `previous` current again, the version and its links removed.
    async fn undo_version(&self, previous: &Artifact, number: VersionNumber) {
        let store = self.ports.store();
        let undone = async {
            store.update_artifact(previous).await?;
            store.delete_artifact_version(previous.id, number).await?;
            self.ports.blobs().remove_version(previous.id, number).await
        };
        if let Err(error) = undone.await {
            tracing::error!(artifact = %previous.id, version = %number, %error, "could not take back a failed version");
        }
    }
}

/// The artifact `id` unless it is unknown or expired ([`Error::NotFound`]
/// either way, ART-08).
pub(super) async fn live<P: Ports>(ports: &P, id: ArtifactId) -> Result<Artifact, Error> {
    let now = ports.clock().now();
    ports
        .store()
        .artifact(id)
        .await?
        .filter(|artifact| !expired(artifact, now))
        .ok_or_else(|| Error::not_found(format!("artifact {id}")))
}

/// Puts the blobs of `checked` (when they are new content), links the
/// files of `version`, whose rows are written, to their blobs, then
/// releases every blob it put, linked or not (so a failed write leaves no blob behind).
async fn write_files<B: Blobs>(
    blobs: &B,
    checked: &Checked,
    version: &ArtifactVersion,
) -> Result<(), Error> {
    let mut put = Vec::new();
    let mut written = Ok(());
    if checked.fresh {
        for (hash, bytes) in checked.blobs() {
            written = blobs.put(hash, bytes).await;
            if written.is_err() {
                break;
            }
            put.push(hash);
        }
    }
    if written.is_ok() {
        for file in &version.files {
            written = blobs
                .link(version.artifact_id, version.number, &file.name, &file.hash)
                .await;
            if written.is_err() {
                break;
            }
        }
    }
    for hash in put {
        if let Err(error) = blobs.release(hash).await {
            tracing::error!(%hash, %error, "could not release an artifact blob");
        }
    }
    written
}

mod import;
pub use import::{ArtifactImport, ImportOutcome, ImportedArtifact};

#[cfg(test)]
mod tests;
