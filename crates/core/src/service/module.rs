//! [`ModuleService`]: installing, changing and removing modules.
//!
//! `install` reads the registry's `index.json`
//! ([`ModuleSource`](crate::ports::ModuleSource)), picks the release (the
//! one asked for, else the newest whose `wit_version` is
//! [`WIT_VERSION`] and whose `liftoffd_min`
//! this daemon meets) and checks, before anything is downloaded, that the
//! registry key is pinned, the world matches (REG-02) and every module it
//! `requires` is installed (REG-03). It then downloads the `.wasm`,
//! verifies its SHA-256 and its minisign signature against
//! [`ModuleSettings::registry_key`](crate::service::ModuleSettings) (REG-01:
//! a refused file is never kept), reads the component's manifest
//! ([`ModuleRuntime::inspect`]) and
//! checks it names that module and version, validates the configuration
//! against its `config_schema` (REG-04: a [`Error::Validation`] naming the
//! field as `config.<field>`), seals the fields the schema marks
//! `"format": "secret"` (REG-05), stores the row (with the SHA-256 the
//! kept file is checked against before every later load), keeps the file, and
//! loads the module with its migrations from `0` so its routes, tools,
//! commands and hooks are there at once (REG-06). `remove` unloads it,
//! deletes the file and the row, and drops its tables only with `purge`
//! (REG-07).
//!
//! Every operation asks [`Authorizer::check`](crate::service::Authorizer)
//! once on [`Resource::Admin`] (MOD-04).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::Authorizer;
use crate::domain::{
    ConfigValue, InstalledModule, ModuleConfigRecord, ModuleName, ModuleVersion, User,
};
use crate::error::{Error, Problem, System, ValidationError};
use crate::module::{
    AvailableModule, ModuleManifest, Release, WIT_VERSION, parse_index, sha256_hex, unpinned,
};
use crate::ports::{
    Action, Clock as _, ModuleRuntime, ModuleSource as _, Ports, Resource, Secrets as _, Store as _,
};
use crate::service::Settings;

/// `POST /modules`: what to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    /// The module.
    pub name: ModuleName,
    /// The release; `None` picks the newest one this daemon can run.
    pub version: Option<ModuleVersion>,
    /// The configuration, checked against the module's `config_schema`.
    pub config: Map<String, Value>,
}

/// `PATCH /modules/{name}`: what to change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleChange {
    /// Enable (`lof module enable` after a fix: the error is cleared) or
    /// disable it.
    pub enabled: Option<bool>,
    /// A new configuration, checked and sealed as at install; the module
    /// is loaded again with it.
    pub config: Option<Map<String, Value>>,
}

/// An installed module as `GET /modules` shows it.
#[derive(Debug, Clone)]
pub struct ModuleRecord {
    /// Its row, with `enabled` and `error` as the runtime sees them now.
    pub installed: InstalledModule,
    /// What it declared (commands, routes, tools, hooks, schema); `None`
    /// when it is not loaded (it failed to load at start).
    pub manifest: Option<ModuleManifest>,
}

/// Installs, changes and removes modules over `P`.
#[derive(Debug)]
pub struct ModuleService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for ModuleService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> ModuleService<P> {
    /// The service over `ports`, checking releases against `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// Asks `authorize` once for `action` on the installation.
    async fn authorize(&self, actor: &User, action: Action) -> Result<(), Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, action, &Resource::Admin)
            .await
    }

    /// Every installed module, in installation order (`GET /modules`).
    ///
    /// # Errors
    ///
    /// Whatever `authorize` refuses with; [`Error::Internal`] when the
    /// store fails.
    pub async fn list(&self, actor: &User) -> Result<Vec<ModuleRecord>, Error> {
        self.authorize(actor, Action::ModuleList).await?;
        let rows = self.ports.store().modules().await?;
        Ok(rows.into_iter().map(|row| self.record(row)).collect())
    }

    /// The modules of the registry with the releases this daemon can run
    /// (`GET /modules/available`).
    ///
    /// # Errors
    ///
    /// Whatever `authorize` refuses with; [`Error::Upstream`] when the
    /// registry cannot be read.
    pub async fn available(&self, actor: &User) -> Result<Vec<AvailableModule>, Error> {
        self.authorize(actor, Action::ModuleAvailable).await?;
        let version = &self.settings.modules.liftoffd_version;
        let mut available: Vec<AvailableModule> = Vec::new();
        for release in self.index().await? {
            if release.check_runs_on(version).is_err() {
                continue;
            }
            match available.last_mut() {
                Some(module) if module.name == release.name => module.releases.push(release),
                _ => available.push(AvailableModule {
                    name: release.name.clone(),
                    releases: vec![release],
                }),
            }
        }
        Ok(available)
    }

    /// Installs `request` as `actor` (see the module documentation).
    ///
    /// # Errors
    ///
    /// - [`Error::Unavailable`] when no registry key is pinned;
    /// - [`Error::Conflict`] when the module is installed already, or the
    ///   release is of another WIT world or needs a newer `liftoffd`
    ///   (REG-02), or it replaces a command an installed module replaces
    ///   already (`login`: one module logs people in), as its component
    ///   says and as the manifest it is loaded with says;
    /// - [`Error::ModuleRequired`] naming a module it requires that is not
    ///   installed (REG-03);
    /// - [`Error::Upstream`] of [`System::Registry`]
    ///   when the file's SHA-256 or signature does not verify (REG-01) or
    ///   the registry cannot be read;
    /// - [`Error::Validation`] for a configuration its schema refuses
    ///   (REG-04) or a component whose manifest names another module or
    ///   version;
    /// - [`Error::NotFound`] for a module or version the registry does not
    ///   list.
    pub async fn install(
        &self,
        actor: &User,
        request: InstallRequest,
    ) -> Result<ModuleRecord, Error> {
        self.authorize(actor, Action::ModuleInstall).await?;
        let key = &self.settings.modules.registry_key;
        if !key.is_pinned() {
            return Err(unpinned());
        }
        let name = request.name;
        let store = self.ports.store();
        if store.module(&name).await?.is_some() {
            return Err(Error::conflict(format!(
                "module {name} is installed already"
            )));
        }
        let release = self.pick(&name, request.version.as_ref()).await?;
        self.check_installed(&release.requires).await?;

        let component = self.ports.registry().download(&release.url).await?;
        if !sha256_hex(&component).eq_ignore_ascii_case(release.sha256.trim()) {
            return Err(Error::upstream(
                System::Registry,
                format!(
                    "the SHA-256 of {name} {} is not the one index.json lists",
                    release.version
                ),
            ));
        }
        key.verify(&release, &component)?;

        let runtime = self.ports.modules();
        let manifest = runtime.inspect(&name, &component).await?;
        if manifest.name != name || release.version != *manifest.version {
            return Err(Error::invalid(
                "component",
                format!(
                    "the signed component is {} {}, not {name} {}",
                    manifest.name, manifest.version, release.version
                ),
            ));
        }
        self.check_neighbours(&manifest).await?;
        let (record, opened) = self.seal(&manifest, &request.config)?;

        // The row is the claim on the name: of two installs at once, the
        // one whose row loses has kept nothing yet, so it never deletes
        // the file the other one keeps under the same name.
        let row = InstalledModule {
            name: name.clone(),
            version: release.version.clone(),
            sha256: sha256_hex(&component),
            enabled: true,
            config: record,
            error: None,
            installed_at: self.ports.clock().now(),
        };
        store.insert_module(&row).await?;
        if let Err(error) = runtime.save(&name, &release.version, &component).await {
            self.forget_row(&name).await;
            return Err(error);
        }
        if let Err(error) = runtime.load(&name, &component, &opened, Some(0)).await {
            self.forget_row(&name).await;
            self.forget_file(&name, &release.version).await;
            return Err(error);
        }
        // What the core trusts (`core-call auth.create_session`) is the
        // manifest the module was loaded with, read with its
        // configuration: checked again, also against an install that
        // claimed its row meanwhile.
        if let Some(loaded) = runtime.registry().manifest(&name)
            && let Err(error) = self.check_replaces(&loaded).await
        {
            if let Err(unload) = runtime.unload(&name).await {
                tracing::error!(module = %name, ?unload, "a module refused after its load stays loaded");
            }
            self.forget_row(&name).await;
            self.forget_file(&name, &release.version).await;
            return Err(error);
        }
        tracing::info!(module = %name, version = %release.version, by = %actor.login, "installed a module");
        Ok(self.record(row))
    }

    /// Applies `change` to the installed module `name` as `actor`.
    ///
    /// A new configuration is merged into the stored one field by field
    /// (a field set to `null` is removed), so `lof module config <name>
    /// --set k=v` changes one field and keeps the secrets; the whole
    /// result is checked against the schema (REG-04), its secret fields
    /// sealed (REG-05), and the module loaded again with it, without
    /// migrating. Enabling clears the error (and loads a module that
    /// failed to load at start); disabling keeps it loaded but out of
    /// every call.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a module that is not installed;
    /// [`Error::Validation`] for a configuration its schema refuses
    /// (REG-04); [`Error::Conflict`] when, enabled or reconfigured, it
    /// would replace a command another installed module replaces
    /// (`login`); what loading it again fails with.
    pub async fn update(
        &self,
        actor: &User,
        name: &ModuleName,
        change: ModuleChange,
    ) -> Result<ModuleRecord, Error> {
        self.authorize(actor, Action::ModuleUpdate).await?;
        let store = self.ports.store();
        let runtime = self.ports.modules();
        let previous = store
            .module(name)
            .await?
            .ok_or_else(|| not_installed(name))?;
        let mut row = previous.clone();
        // A module the runtime disabled (MOD-09) is disabled even while
        // `sync_status` has not written that into its row yet: only
        // `enabled: true` brings it back.
        if let Some(status) = runtime.registry().status(name)
            && !status.enabled
        {
            row.enabled = false;
            row.error = status.error.or(row.error);
        }
        let registered = runtime.registry().manifest(name).is_some();
        let reload = change.config.is_some() || change.enabled == Some(true) && !registered;
        let enabled = change.enabled.unwrap_or(row.enabled);
        let reloaded = if reload {
            let component = self.kept(&row).await?;
            let manifest = match runtime.registry().manifest(name) {
                Some(manifest) => manifest,
                None => runtime.inspect(name, &component).await?,
            };
            self.check_neighbours(&manifest).await?;
            let mut config = self.open(&row.config)?;
            for (field, value) in change.config.unwrap_or_default() {
                if value.is_null() {
                    let _removed = config.remove(&field);
                } else {
                    let _previous = config.insert(field, value);
                }
            }
            let (record, opened) = self.seal(&manifest, &config)?;
            row.config = record;
            Some((component, opened))
        } else if change.enabled == Some(true) {
            self.check_registered_replaces(name).await.map(|()| None)?
        } else {
            None
        };
        if enabled {
            row.enabled = true;
            row.error = None;
        } else {
            let why = match (change.enabled, &row.error) {
                (None, Some(error)) => error.clone(),
                _ => format!("disabled by {}", actor.login),
            };
            row.enabled = false;
            row.error = Some(why);
        }
        // The row first: a store that fails leaves the running module as
        // it was; a reload that fails puts the row back.
        store.update_module(&row).await?;
        let applied = async {
            if let Some((component, opened)) = &reloaded {
                runtime.load(name, component, opened, None).await?;
            }
            if row.enabled {
                if runtime
                    .registry()
                    .status(name)
                    .is_some_and(|status| !status.enabled)
                {
                    runtime.registry().enable(name)?;
                }
            } else {
                let why = row.error.clone().unwrap_or_default();
                let _registered = runtime.registry().disable(name, why);
            }
            Ok::<(), Error>(())
        }
        .await;
        if let Err(error) = applied {
            if let Err(restore) = store.update_module(&previous).await {
                tracing::error!(module = %name, ?restore, "the row of a module whose change failed stays changed");
            }
            return Err(error);
        }
        Ok(self.record(row))
    }

    /// Removes the installed module `name` as `actor`: unloads it,
    /// deletes its file and its row; its tables and keys stay unless
    /// `purge` (REG-07).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a module that is not installed;
    /// [`Error::Conflict`] while another installed module requires it.
    pub async fn remove(&self, actor: &User, name: &ModuleName, purge: bool) -> Result<(), Error> {
        self.authorize(actor, Action::ModuleRemove).await?;
        let store = self.ports.store();
        let runtime = self.ports.modules();
        let row = store
            .module(name)
            .await?
            .ok_or_else(|| not_installed(name))?;
        let mut dependents: Vec<String> = Vec::new();
        for other in store.modules().await? {
            if &other.name != name && self.requires(&other).await.contains(name) {
                dependents.push(other.name.to_string());
            }
        }
        if !dependents.is_empty() {
            return Err(Error::conflict(format!(
                "module {name} is required by {}; remove that first",
                dependents.join(", ")
            )));
        }
        runtime.unload(name).await?;
        runtime.discard(name, &row.version).await?;
        if purge {
            runtime.purge(name).await?;
        }
        store.delete_module(name).await?;
        tracing::info!(module = %name, purge, by = %actor.login, "removed a module");
        Ok(())
    }

    /// Loads every installed module at the daemon's start, in installation
    /// order, so their routes, tools, commands and hooks are there again. A module disabled before stays loaded but
    /// disabled with its error; a module that fails to load is marked
    /// `enabled = false` with the error in its row, and the daemon goes on
    /// without it (`lof module enable` after a fix). Asks no `authorize`:
    /// the daemon itself restores what the admin installed.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the store fails.
    pub async fn restore(&self) -> Result<(), Error> {
        let store = self.ports.store();
        let runtime = self.ports.modules();
        for mut row in store.modules().await? {
            let loaded = async {
                let component = self.kept(&row).await?;
                let opened = self
                    .open(&row.config)?
                    .into_iter()
                    .map(|(field, value)| (field, config_text(value)))
                    .collect();
                runtime.load(&row.name, &component, &opened, None).await
            }
            .await;
            match loaded {
                Ok(()) if row.enabled => {}
                Ok(()) => {
                    let why = row.error.clone().unwrap_or_else(|| "disabled".to_owned());
                    let _registered = runtime.registry().disable(&row.name, why);
                }
                Err(error) => {
                    tracing::error!(module = %row.name, ?error, "an installed module failed to load and is disabled");
                    row.enabled = false;
                    row.error = Some(format!("failed to load: {error}"));
                    store.update_module(&row).await?;
                }
            }
        }
        Ok(())
    }

    /// Writes into `modules` what the runtime decided on its own: a module
    /// the registry disabled after
    /// [`FAILURES_TO_DISABLE`](crate::module::FAILURES_TO_DISABLE) failures in a
    /// row (MOD-09) is `enabled = false` with its error in its row, so it
    /// stays disabled across a restart until `lof module enable`. The
    /// daemon runs it periodically (`list` only overlays the live status
    /// and writes nothing). Asks no
    /// `authorize`: it only records what happened.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the store fails.
    pub async fn sync_status(&self) -> Result<(), Error> {
        let store = self.ports.store();
        for mut row in store.modules().await? {
            let Some(status) = self.ports.modules().registry().status(&row.name) else {
                continue;
            };
            if !status.enabled && row.enabled {
                row.enabled = false;
                row.error = status.error;
                store.update_module(&row).await?;
            }
        }
        Ok(())
    }

    /// The registry's releases.
    async fn index(&self) -> Result<Vec<Release>, Error> {
        parse_index(&self.ports.registry().index().await?)
    }

    /// The release of `name` to install: `version`, or the newest one this
    /// daemon can run (REG-02).
    async fn pick(
        &self,
        name: &ModuleName,
        version: Option<&ModuleVersion>,
    ) -> Result<Release, Error> {
        let liftoffd = &self.settings.modules.liftoffd_version;
        let releases: Vec<Release> = self
            .index()
            .await?
            .into_iter()
            .filter(|release| &release.name == name)
            .collect();
        if releases.is_empty() {
            return Err(Error::not_found(format!("module {name} in the registry")));
        }
        if let Some(version) = version {
            let release = releases
                .into_iter()
                .find(|release| &release.version == version)
                .ok_or_else(|| Error::not_found(format!("{name} {version} in the registry")))?;
            release.check_runs_on(liftoffd)?;
            return Ok(release);
        }
        let mut refusal = None;
        for release in releases {
            match release.check_runs_on(liftoffd) {
                Ok(()) => return Ok(release),
                Err(error) => {
                    let _first = refusal.get_or_insert(error);
                }
            }
        }
        Err(refusal.unwrap_or_else(|| {
            Error::conflict(format!(
                "no release of {name} runs on liftoff:module@{WIT_VERSION}"
            ))
        }))
    }

    /// Refuses unless every module of `requires` is installed (REG-03).
    async fn check_installed(&self, requires: &[ModuleName]) -> Result<(), Error> {
        for dependency in requires {
            if self.ports.store().module(dependency).await?.is_none() {
                return Err(Error::module_required(dependency.as_str()));
            }
        }
        Ok(())
    }

    /// Refuses `manifest` when a command it replaces is replaced by
    /// another installed module already: one module takes over `lof login`
    /// (and with it `core-call auth.create_session`), never two. The other
    /// installed modules are known by their loaded manifests, else by
    /// their kept components (a module that failed to load at start still
    /// replaces what it replaces); one whose component cannot be read
    /// refuses, as nothing then says it replaces nothing.
    async fn check_replaces(&self, manifest: &ModuleManifest) -> Result<(), Error> {
        if manifest.replaces.is_empty() {
            return Ok(());
        }
        let runtime = self.ports.modules();
        for installed in self.ports.store().modules().await? {
            if installed.name == manifest.name {
                continue;
            }
            let theirs = if let Some(theirs) = runtime.registry().manifest(&installed.name) {
                theirs.replaces
            } else {
                let inspected = async {
                    let component = self.kept(&installed).await?;
                    runtime.inspect(&installed.name, &component).await
                }
                .await;
                match inspected {
                    Ok(theirs) => theirs.replaces,
                    Err(error) => {
                        tracing::warn!(module = %installed.name, ?error, "what an installed module replaces cannot be read");
                        return Err(Error::conflict(format!(
                            "what module {} replaces cannot be read; fix or remove it before installing or enabling {}",
                            installed.name, manifest.name
                        )));
                    }
                }
            };
            if let Some(command) = manifest
                .replaces
                .iter()
                .find(|command| theirs.contains(command))
            {
                return Err(Error::conflict(format!(
                    "module {} replaces {command} already; remove it before installing or enabling {}",
                    installed.name, manifest.name
                )));
            }
        }
        Ok(())
    }

    /// Refuses `manifest` unless what it requires is installed (REG-03)
    /// and no other installed module replaces what it replaces.
    async fn check_neighbours(&self, manifest: &ModuleManifest) -> Result<(), Error> {
        self.check_installed(&manifest.requires).await?;
        self.check_replaces(manifest).await
    }

    /// [`ModuleService::check_replaces`] for the loaded module `name`, as
    /// its registered manifest says (nothing when it is not loaded).
    async fn check_registered_replaces(&self, name: &ModuleName) -> Result<(), Error> {
        match self.ports.modules().registry().manifest(name) {
            Some(manifest) => self.check_replaces(&manifest).await,
            None => Ok(()),
        }
    }

    /// `config` checked against the manifest's schema (REG-04), as stored
    /// (the secret fields sealed, REG-05) and as the module reads it
    /// (`config-get`: every field as text, secrets opened).
    fn seal(
        &self,
        manifest: &ModuleManifest,
        config: &Map<String, Value>,
    ) -> Result<(ModuleConfigRecord, BTreeMap<String, String>), Error> {
        check_config(manifest.config_schema.as_ref(), config)?;
        let secrets = self.ports.secrets();
        let mut record = ModuleConfigRecord::new();
        let mut opened = BTreeMap::new();
        for (field, value) in config {
            let stored = if is_secret(manifest.config_schema.as_ref(), field) {
                ConfigValue::Sealed(secrets.encrypt(&config_text(value.clone()))?)
            } else {
                ConfigValue::Plain(value.clone())
            };
            let _none = record.insert(field.clone(), stored);
            let _none = opened.insert(field.clone(), config_text(value.clone()));
        }
        Ok((record, opened))
    }

    /// The stored configuration with its secret fields opened, as JSON.
    fn open(&self, record: &ModuleConfigRecord) -> Result<Map<String, Value>, Error> {
        let secrets = self.ports.secrets();
        record
            .iter()
            .map(|(field, value)| {
                let value = match value {
                    ConfigValue::Plain(json) => json.clone(),
                    ConfigValue::Sealed(sealed) => Value::String(secrets.decrypt(sealed)?),
                };
                Ok((field.clone(), value))
            })
            .collect()
    }

    /// Deletes the kept file of a module whose install failed after it
    /// was kept.
    async fn forget_file(&self, name: &ModuleName, version: &ModuleVersion) {
        if let Err(error) = self.ports.modules().discard(name, version).await {
            tracing::error!(module = %name, ?error, "the file of a module whose install failed stays");
        }
    }

    /// Deletes the row of a module whose install failed after it was
    /// stored.
    async fn forget_row(&self, name: &ModuleName) {
        if let Err(error) = self.ports.store().delete_module(name).await {
            tracing::error!(module = %name, ?error, "the row of a module whose install failed stays");
        }
    }

    /// The kept component of `row`, checked against the SHA-256 verified
    /// at install: a file changed since is not loaded.
    async fn kept(&self, row: &InstalledModule) -> Result<Vec<u8>, Error> {
        let component = self.ports.modules().read(&row.name, &row.version).await?;
        if !sha256_hex(&component).eq_ignore_ascii_case(&row.sha256) {
            return Err(Error::invalid(
                "component",
                format!(
                    "the kept file of {} {} is not the one installed: its SHA-256 differs",
                    row.name, row.version
                ),
            ));
        }
        Ok(component)
    }

    /// The modules `row` requires: as its loaded manifest says, else as
    /// its kept component says (a module that failed to load at start
    /// still holds its dependencies). Nothing when neither can be read:
    /// such a module cannot be loaded either, and enabling it checks its
    /// dependencies again.
    async fn requires(&self, row: &InstalledModule) -> Vec<ModuleName> {
        let runtime = self.ports.modules();
        if let Some(manifest) = runtime.registry().manifest(&row.name) {
            return manifest.requires;
        }
        let inspected = async {
            let component = self.kept(row).await?;
            runtime.inspect(&row.name, &component).await
        }
        .await;
        match inspected {
            Ok(manifest) => manifest.requires,
            Err(error) => {
                tracing::warn!(module = %row.name, ?error, "the dependencies of a module that is not loaded cannot be read");
                Vec::new()
            }
        }
    }

    /// The record of `row`, with the status and manifest the runtime has.
    fn record(&self, mut installed: InstalledModule) -> ModuleRecord {
        let registry = self.ports.modules().registry();
        if let Some(status) = registry.status(&installed.name) {
            installed.enabled = status.enabled;
            installed.error = status.error;
        }
        ModuleRecord {
            manifest: registry.manifest(&installed.name),
            installed,
        }
    }
}

/// The refusal of a module that is not installed.
fn not_installed(name: &ModuleName) -> Error {
    Error::not_found(format!("module {name}"))
}

/// A configuration value as `config-get` answers it: a string as it is,
/// anything else as its JSON.
fn config_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

/// Whether the schema marks the top-level `field` `"format": "secret"`.
fn is_secret(schema: Option<&Value>, field: &str) -> bool {
    schema
        .and_then(|schema| schema.get("properties"))
        .and_then(|properties| properties.get(field))
        .and_then(|property| property.get("format"))
        .and_then(Value::as_str)
        == Some("secret")
}

/// Checks `config` against `schema` (REG-04): the first error the schema
/// finds, as a [`Error::Validation`] of `config.<path>` (the field a
/// `required` or `additionalProperties` keyword names, else where the
/// value sits). The message never repeats the value, which may be a
/// secret. A module without a schema takes no configuration.
fn check_config(schema: Option<&Value>, config: &Map<String, Value>) -> Result<(), Error> {
    let Some(schema) = schema else {
        return config.keys().next().map_or(Ok(()), |field| {
            Err(ValidationError::at(
                "config",
                field.clone(),
                Problem::Invalid("the module takes no configuration".to_owned()),
            )
            .into())
        });
    };
    check_schema(
        "config",
        "config_schema",
        schema,
        &Value::Object(config.clone()),
    )
}

/// Checks `instance` against `schema` (the manifest's `schema_name`): the
/// first error the schema finds, as a [`Error::Validation`] of
/// `field.<path>` (the property a `required` or `additionalProperties`
/// keyword names, else where the value sits). The message never repeats
/// the value, which may be a secret. Shared by a module's configuration
/// (REG-04) and the arguments of its MCP tools (MCP-04).
pub(crate) fn check_schema(
    field: &'static str,
    schema_name: &str,
    schema: &Value,
    instance: &Value,
) -> Result<(), Error> {
    let validator = jsonschema::validator_for(schema).map_err(|error| {
        Error::invalid(
            "manifest",
            format!("{schema_name} is not a JSON Schema: {}", error.masked()),
        )
    })?;
    let Some(error) = validator.iter_errors(instance).next() else {
        return Ok(());
    };
    let mut path: Vec<String> = error
        .instance_path()
        .iter()
        .map(|segment| segment.to_string())
        .collect();
    match error.kind() {
        jsonschema::error::ValidationErrorKind::Required { property } => {
            path.push(
                property
                    .as_str()
                    .map_or_else(|| property.to_string(), str::to_owned),
            );
        }
        jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } => {
            path.extend(unexpected.first().cloned());
        }
        _ => {}
    }
    let problem = Problem::Invalid(error.masked().to_string());
    Err(if path.is_empty() {
        ValidationError::new(field, problem)
    } else {
        ValidationError::at(field, path.join("."), problem)
    }
    .into())
}

#[cfg(test)]
mod tests;
