//! `TemplateCatalog`: the
//! templates `lof create` offers.
//!
//! The catalogue is the installation's `[templates] repos`
//! ([`Settings::templates`]) plus the repositories the request names (the
//! CLI's `[local] templates`, TPL-06; at most [`MAX_REQUESTED_TEMPLATES`]
//! of them). Each repository's `liftoff.toml` is read from its default branch through
//! [`Git::list_templates`](crate::ports::Git::list_templates) as the
//! actor's own connection on that repository's host, one call per host;
//! a read is cached in the store for [`TEMPLATE_CACHE_TTL`] per `(actor,
//! host, repo)` (TPL-05).
//!
//! An entry without a host (`acme/app`) is read through
//! each of the actor's connections, by host, and the first that has its
//! `liftoff.toml` lists it.
//!
//! A repository is skipped, and named in [`Catalog::warnings`], when the
//! actor has no connection on its host (or no connection at all), when it
//! has no `liftoff.toml` (TPL-04), when its manifest is invalid or has no
//! `[template]`, or when the forge fails on it; the rest of the catalogue
//! is returned. Templates are grouped by `[template] group` in
//! the order of [`TemplateGroup`] and sorted by name within a group
//! (TPL-03).

use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::Arc;

use super::support::open_connection;
use super::{Authorizer, Settings};
use crate::domain::{
    CachedTemplate, GitConnection, Host, RepoRef, TemplateGroup, TemplateSource, Timestamp, User,
};
use crate::error::Error;
use crate::manifest::ProjectManifest;
use crate::ports::{
    Action, Clock as _, Git as _, GitAccess, Ports, Resource, Store as _, Template,
};

/// How long a read of a template repository is served from the store
/// (TPL-05).
pub const TEMPLATE_CACHE_TTL: Duration = Duration::from_mins(10);

/// The most repositories one request may add to the catalogue (the CLI's
/// `[local] templates`, TPL-06): each is a forge read, per connection for
/// an entry without a host, and a cache row.
pub const MAX_REQUESTED_TEMPLATES: usize = 32;

/// See the [module documentation](self).
#[derive(Debug)]
pub struct TemplateCatalog<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for TemplateCatalog<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

/// The catalogue as [`TemplateCatalog::list`] answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    /// The groups that have a template, in the order of
    /// [`TemplateGroup`].
    pub groups: Vec<CatalogGroup>,
    /// The repositories skipped, in catalogue order.
    pub warnings: Vec<CatalogWarning>,
}

/// One group of the catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogGroup {
    /// `[template] group`.
    pub group: TemplateGroup,
    /// Its templates, by `[template] name`.
    pub templates: Vec<CatalogEntry>,
}

/// One template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The forge host it was read from.
    pub host: Host,
    /// `owner/name` on that host.
    pub repo: RepoRef,
    /// Its `liftoff.toml`, which has a `[template]`.
    pub manifest: ProjectManifest,
}

/// A repository the catalogue skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogWarning {
    /// The entry as configured or requested.
    pub source: TemplateSource,
    /// Why, safe to show (`no liftoff.toml`, the validation error, the
    /// host without a connection).
    pub reason: String,
}

impl<P: Ports> TemplateCatalog<P> {
    /// A catalogue over `ports`, of the repositories in `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// The catalogue of the configured repositories plus `extra` as
    /// `actor` reads it, only `group` when given.
    ///
    /// Asks [`Action::GitTemplates`] on [`Resource::Admin`] once, before
    /// anything is read (MOD-04); every read then goes through the actor's
    /// own forge connections.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] on field `repos` when `extra` names more than
    /// [`MAX_REQUESTED_TEMPLATES`] repositories (nothing is read);
    /// [`Error::Forbidden`] when the authorizer refuses; whatever the store
    /// or the secrets port report. A repository the actor cannot read, or
    /// a forge that fails, is a warning, not an error.
    pub async fn list(
        &self,
        actor: &User,
        extra: &[TemplateSource],
        group: Option<TemplateGroup>,
    ) -> Result<Catalog, Error> {
        if extra.len() > MAX_REQUESTED_TEMPLATES {
            return Err(Error::invalid(
                "repos",
                format!(
                    "at most {MAX_REQUESTED_TEMPLATES} repositories, got {}",
                    extra.len()
                ),
            ));
        }
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::GitTemplates, &Resource::Admin)
            .await?;
        let store = self.ports.store();
        let connections = store.git_connections_of_user(actor.id).await?;
        let mut reads: Vec<Wanted> = Vec::new();
        let mut slots: Vec<Slot> = Vec::new();
        for source in self.settings.templates.iter().chain(extra) {
            let place = candidates(source, &connections).map(|found| {
                found
                    .into_iter()
                    .map(|connection| read_index(&mut reads, connection, &source.repo))
                    .collect()
            });
            slots.push(Slot {
                source: source.clone(),
                place,
            });
        }
        let now = self.ports.clock().now();
        let ttl = time::Duration::try_from(TEMPLATE_CACHE_TTL).unwrap_or(time::Duration::MAX);
        for item in &mut reads {
            if let Some(cached) = store
                .cached_template(actor.id, &item.connection.host, &item.repo)
                .await?
                && now < cached.fetched_at.saturating_add(ttl)
            {
                item.manifest = Some(Read::Text(cached.manifest));
            }
        }
        self.read_uncached(actor, &mut reads, now).await?;
        let mut taken: Vec<usize> = Vec::new();
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        for slot in slots {
            let outcome = match slot.place {
                Err(reason) => Err(reason),
                Ok(indexes) => {
                    let Some(outcome) = settle(&reads, &indexes, &mut taken) else {
                        continue;
                    };
                    outcome
                }
            };
            match outcome {
                Ok(found) => entries.push(found),
                Err(reason) => warnings.push(CatalogWarning {
                    source: slot.source,
                    reason,
                }),
            }
        }
        Ok(Catalog {
            groups: grouped(entries, group),
            warnings,
        })
    }

    /// Reads every entry of `wanted` the cache did not answer, one
    /// [`Git::list_templates`](crate::ports::Git::list_templates) call per
    /// host in order of first appearance, and caches what the forge said.
    ///
    /// When a call for several repositories fails, each of them is asked
    /// again on its own, so one repository the forge fails on (a
    /// `liftoff.toml` that is not UTF-8) does not hide the others. A
    /// failure is never cached.
    async fn read_uncached(
        &self,
        actor: &User,
        wanted: &mut [Wanted],
        now: Timestamp,
    ) -> Result<(), Error> {
        let mut hosts: Vec<Host> = Vec::new();
        for item in wanted.iter().filter(|item| item.manifest.is_none()) {
            if !hosts.contains(&item.connection.host) {
                hosts.push(item.connection.host.clone());
            }
        }
        for host in hosts {
            let pending: Vec<usize> = (0..wanted.len())
                .filter(|&index| {
                    wanted
                        .get(index)
                        .is_some_and(|item| item.manifest.is_none() && item.connection.host == host)
                })
                .collect();
            let Some(connection) = pending
                .first()
                .and_then(|&index| wanted.get(index))
                .map(|item| item.connection.clone())
            else {
                continue;
            };
            let access = match open_connection(self.ports.as_ref(), &connection) {
                Ok(access) => access,
                Err(error) => {
                    fail(wanted, &pending, &host, &error);
                    continue;
                }
            };
            let batches: Vec<Vec<usize>> = match self.ask(wanted, &access, &pending).await {
                Ok(answered) => {
                    self.keep(actor, wanted, &pending, answered, now).await?;
                    Vec::new()
                }
                Err(error) if pending.len() == 1 => {
                    fail(wanted, &pending, &host, &error);
                    Vec::new()
                }
                Err(_) => pending.iter().map(|&index| vec![index]).collect(),
            };
            for batch in batches {
                match self.ask(wanted, &access, &batch).await {
                    Ok(answered) => self.keep(actor, wanted, &batch, answered, now).await?,
                    Err(error) => fail(wanted, &batch, &host, &error),
                }
            }
        }
        Ok(())
    }

    /// One [`Git::list_templates`](crate::ports::Git::list_templates) call
    /// for the repositories of `wanted` at `indexes`.
    async fn ask(
        &self,
        wanted: &[Wanted],
        access: &GitAccess,
        indexes: &[usize],
    ) -> Result<Vec<Template>, Error> {
        let repos: Vec<RepoRef> = indexes
            .iter()
            .filter_map(|&index| wanted.get(index).map(|item| item.repo.clone()))
            .collect();
        self.ports.git().list_templates(access, &repos).await
    }

    /// Caches and records what the forge `answered` for the repositories
    /// of `wanted` at `indexes`.
    async fn keep(
        &self,
        actor: &User,
        wanted: &mut [Wanted],
        indexes: &[usize],
        answered: Vec<Template>,
        now: Timestamp,
    ) -> Result<(), Error> {
        for template in answered {
            let Some(&index) = indexes.iter().find(|&&index| {
                wanted
                    .get(index)
                    .is_some_and(|item| item.repo == template.repo)
            }) else {
                continue;
            };
            let Some(item) = wanted.get_mut(index) else {
                continue;
            };
            self.ports
                .store()
                .put_cached_template(&CachedTemplate {
                    user_id: actor.id,
                    host: item.connection.host.clone(),
                    repo: template.repo,
                    manifest: template.manifest.clone(),
                    fetched_at: now,
                })
                .await?;
            item.manifest = Some(Read::Text(template.manifest));
        }
        Ok(())
    }
}

/// Marks the repositories of `wanted` at `indexes` as failed with
/// `error`, logging an internal one (its source is never shown).
fn fail(wanted: &mut [Wanted], indexes: &[usize], host: &Host, error: &Error) {
    if matches!(error, Error::Internal(_)) {
        tracing::error!(%host, error = ?error, "reading the template catalogue failed");
    }
    let reason = error.to_string();
    for &index in indexes {
        if let Some(item) = wanted.get_mut(index) {
            item.manifest = Some(Read::Failed(reason.clone()));
        }
    }
}

/// One entry of the catalogue as configured or requested, and the reads
/// that may answer it.
struct Slot {
    /// As configured or requested, for the warning.
    source: TemplateSource,
    /// The indexes of its candidate reads, in order; or why it cannot be
    /// read.
    place: Result<Vec<usize>, String>,
}

/// One repository on one host to read, shared by every [`Slot`] that
/// names it.
struct Wanted {
    /// The actor's connection on its host.
    connection: GitConnection,
    /// `owner/name` there.
    repo: RepoRef,
    /// What the cache or the forge said; `None` while unread.
    manifest: Option<Read>,
}

/// What reading one repository gave.
enum Read {
    /// The `liftoff.toml` text, `None` when there is none.
    Text(Option<String>),
    /// The forge failed; the reason is safe to show.
    Failed(String),
}

/// The actor's connections that may read `source`, in order, or the
/// reason it cannot be read: the one on its host, or, for an entry
/// without a host (`acme/app`), every connection by host.
fn candidates(
    source: &TemplateSource,
    connections: &[GitConnection],
) -> Result<Vec<GitConnection>, String> {
    match &source.host {
        Some(host) => connections
            .iter()
            .find(|connection| &connection.host == host)
            .map(|connection| vec![connection.clone()])
            .ok_or_else(|| {
                format!(
                    "no git connection to {host}: connect it with `lof git connect --host {host}`"
                )
            }),
        None if connections.is_empty() => {
            Err("no git connection: connect a forge with `lof git connect`".to_owned())
        }
        None => {
            let mut found = connections.to_vec();
            found.sort_by(|a, b| a.host.cmp(&b.host));
            Ok(found)
        }
    }
}

/// The index in `reads` of `repo` through `connection`, added when new.
fn read_index(reads: &mut Vec<Wanted>, connection: GitConnection, repo: &RepoRef) -> usize {
    if let Some(index) = reads
        .iter()
        .position(|other| other.connection.host == connection.host && &other.repo == repo)
    {
        return index;
    }
    let index = reads.len();
    reads.push(Wanted {
        connection,
        repo: repo.clone(),
        manifest: None,
    });
    index
}

/// What a slot with the candidate reads `indexes` lists: the first read
/// that found a `liftoff.toml` (or its only read), as an entry or a
/// warning; with several candidates and none found, one warning naming
/// each host. `None` when an earlier slot already listed those reads.
fn settle(
    reads: &[Wanted],
    indexes: &[usize],
    taken: &mut Vec<usize>,
) -> Option<Result<CatalogEntry, String>> {
    let found = indexes.iter().copied().find(|&index| {
        reads
            .get(index)
            .is_some_and(|item| matches!(item.manifest, Some(Read::Text(Some(_)))))
    });
    let only = match indexes {
        &[index] => Some(index),
        _ => None,
    };
    let Some(chosen) = found.or(only) else {
        if indexes.iter().all(|index| taken.contains(index)) {
            return None;
        }
        taken.extend_from_slice(indexes);
        let reasons: Vec<String> = indexes
            .iter()
            .filter_map(|&index| reads.get(index))
            .map(|item| format!("{}: {}", item.connection.host, unread_reason(item)))
            .collect();
        return Some(Err(format!(
            "not found on any connected forge ({})",
            reasons.join("; ")
        )));
    };
    if taken.contains(&chosen) {
        return None;
    }
    taken.push(chosen);
    let item = reads.get(chosen)?;
    Some(match &item.manifest {
        Some(Read::Text(Some(text))) => entry(&item.connection.host, &item.repo, text),
        _ => Err(unread_reason(item)),
    })
}

/// Why `item` gave no `liftoff.toml`.
fn unread_reason(item: &Wanted) -> String {
    match &item.manifest {
        Some(Read::Text(Some(_))) => String::new(),
        Some(Read::Text(None)) => "no liftoff.toml on the default branch".to_owned(),
        Some(Read::Failed(reason)) => reason.clone(),
        None => "the forge did not answer for this repository".to_owned(),
    }
}

/// The catalogue entry of `text`, or why it is not one.
fn entry(host: &Host, repo: &RepoRef, text: &str) -> Result<CatalogEntry, String> {
    let manifest = ProjectManifest::parse(text).map_err(|error| error.to_string())?;
    if manifest.template.is_none() {
        return Err("liftoff.toml has no [template] table".to_owned());
    }
    Ok(CatalogEntry {
        host: host.clone(),
        repo: repo.clone(),
        manifest,
    })
}

/// `entries` grouped in the order of [`TemplateGroup`], each group sorted
/// by name (then host and repository), only `only` when given.
fn grouped(entries: Vec<CatalogEntry>, only: Option<TemplateGroup>) -> Vec<CatalogGroup> {
    let mut by_group: BTreeMap<TemplateGroup, Vec<CatalogEntry>> = BTreeMap::new();
    for found in entries {
        let Some(group) = found.manifest.template.as_ref().map(|t| t.group) else {
            continue;
        };
        if only.is_none_or(|only| only == group) {
            by_group.entry(group).or_default().push(found);
        }
    }
    by_group
        .into_iter()
        .map(|(group, mut templates)| {
            templates.sort_by(|a, b| {
                (name_of(a), &a.host, &a.repo).cmp(&(name_of(b), &b.host, &b.repo))
            });
            CatalogGroup { group, templates }
        })
        .collect()
}

/// `[template] name` of an entry.
fn name_of(found: &CatalogEntry) -> &str {
    found
        .manifest
        .template
        .as_ref()
        .map_or("", |template| template.name.as_str())
}

#[cfg(test)]
mod tests;
